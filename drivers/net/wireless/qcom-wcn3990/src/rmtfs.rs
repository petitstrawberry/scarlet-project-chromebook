// SPDX-License-Identifier: GPL-2.0-only
//! Wi-Fi-only MSS remote filesystem, following linux-msm/rmtfs's QMI schema.
//! No cellular FSG/NV import: the non-LTE firmware starts with four blank 2MiB
//! images, as msm-cros-efs-loader does for Wi-Fi SKUs. Writes survive reconnect
//! within this modem lifetime. Persistent storage is a separate backend.
use crate::qmi;
use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
pub const PORT: u32 = 14;
const SECTORS: u32 = 4096;
pub trait Memory {
    fn region(&self) -> (u64, usize);
    fn read(&self, physical: u64, bytes: &mut [u8]) -> Result<(), ()>;
    fn write(&self, physical: u64, bytes: &[u8]) -> Result<(), ()>;
}
#[derive(Clone, Copy)]
struct Handle {
    node: u32,
    file: usize,
}
pub struct Service<M: Memory> {
    memory: M,
    handles: BTreeMap<u32, Handle>,
    next: u32,
    files: [BTreeMap<u32, Box<[u8; 512]>>; 4],
}
fn word(b: &[u8]) -> Result<u32, u16> {
    b.try_into().map(u32::from_le_bytes).map_err(|_| 2)
}
impl<M: Memory> Service<M> {
    pub fn new(memory: M) -> Self {
        Self {
            memory,
            handles: BTreeMap::new(),
            next: 1,
            files: core::array::from_fn(|_| BTreeMap::new()),
        }
    }
    pub fn reset_clients(&mut self) {
        self.handles.clear();
    }
    pub fn request(&mut self, node: u32, bytes: &[u8]) -> Result<Vec<u8>, qmi::Error> {
        let request = qmi::Message::parse(bytes)?;
        if request.kind != 0 {
            return Err(qmi::Error::Malformed);
        }
        let mut body = Vec::new();
        let mut extra = Vec::new();
        let result = self.process(node, &request, &mut extra);
        let status = match result {
            Ok(()) => [0, 0, 0, 0],
            Err(code) => [1, 0, code as u8, (code >> 8) as u8],
        };
        qmi::tlv(&mut body, 2, &status)?;
        if result.is_ok() {
            body.extend_from_slice(&extra);
        }
        let mut reply = Vec::with_capacity(body.len() + 7);
        reply.push(2);
        reply.extend_from_slice(&request.transaction.to_le_bytes());
        reply.extend_from_slice(&request.id.to_le_bytes());
        reply.extend_from_slice(&(body.len() as u16).to_le_bytes());
        reply.extend_from_slice(&body);
        Ok(reply)
    }
    fn process(&mut self, node: u32, m: &qmi::Message<'_>, extra: &mut Vec<u8>) -> Result<(), u16> {
        if m.id == 1 {
            let raw = m.tlv(1).ok_or(2u16)?;
            let path = raw.strip_suffix(&[0]).unwrap_or(raw);
            let file = match path {
                b"/boot/modem_fs1" => 0,
                b"/boot/modem_fs2" => 1,
                b"/boot/modem_fsc" => 2,
                b"/boot/modem_fsg" => 3,
                _ => return Err(1),
            };
            if self.handles.len() >= 16 {
                return Err(1);
            }
            let id = self.next;
            self.next = id.checked_add(1).ok_or(1u16)?;
            self.handles.insert(id, Handle { node, file });
            qmi::tlv(extra, 16, &id.to_le_bytes()).map_err(|_| 2u16)?;
            return Ok(());
        }
        let id = word(m.tlv(1).ok_or(2u16)?)?;
        let handle = *self
            .handles
            .get(&id)
            .filter(|h| h.node == node)
            .ok_or(1u16)?;
        match m.id {
            2 => {
                self.handles.remove(&id);
            }
            4 => {
                let size = word(m.tlv(2).ok_or(2u16)?)? as usize;
                let (base, capacity) = self.memory.region();
                if size == 0 || size > capacity {
                    return Err(1);
                }
                qmi::tlv(extra, 16, &base.to_le_bytes()).map_err(|_| 2u16)?;
            }
            5 => qmi::tlv(extra, 16, &[0]).map_err(|_| 2u16)?,
            3 => {
                let direction = m.tlv(2).filter(|b| b.len() == 1 && b[0] <= 1).ok_or(2u16)?[0];
                let sync = m.tlv(4).filter(|b| b.len() == 1 && b[0] <= 1).ok_or(2u16)?;
                let _ = sync;
                let iov = m.tlv(3).ok_or(2u16)?;
                if iov.is_empty() || iov.len() != 1 + iov[0] as usize * 12 {
                    return Err(2);
                }
                // Linux RMTFS uses one shared carveout for all file handles;
                // ALLOC_BUFF reports that region, it is not a per-file buffer.
                let (base, capacity) = self.memory.region();
                // Validate the entire request before moving a single byte.
                for entry in iov[1..].chunks_exact(12) {
                    let sector = word(&entry[..4])?;
                    let physical = u64::from(word(&entry[4..8])?);
                    let count = word(&entry[8..12])?;
                    let length = u64::from(count) * 512;
                    if count == 0
                        || sector.checked_add(count).is_none_or(|end| end > SECTORS)
                        || physical < base
                        || physical
                            .checked_add(length)
                            .is_none_or(|end| end > base + capacity as u64)
                    {
                        return Err(2);
                    }
                }
                for entry in iov[1..].chunks_exact(12) {
                    let sector = word(&entry[..4])?;
                    let physical = u64::from(word(&entry[4..8])?);
                    let count = word(&entry[8..12])?;
                    for offset in 0..count {
                        let physical = physical + u64::from(offset) * 512;
                        let sector = sector + offset;
                        if direction == 1 {
                            let mut bytes = Box::new([0; 512]);
                            self.memory.read(physical, &mut *bytes).map_err(|_| 1u16)?;
                            self.files[handle.file].insert(sector, bytes);
                        } else {
                            let zero = [0; 512];
                            let bytes =
                                self.files[handle.file].get(&sector).map_or(&zero, |b| &**b);
                            self.memory.write(physical, bytes).map_err(|_| 1u16)?;
                        }
                    }
                }
            }
            _ => return Err(0x5e),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    struct Ram(RefCell<[u8; 1024]>);
    impl Memory for Ram {
        fn region(&self) -> (u64, usize) {
            (0x10000, 1024)
        }
        fn read(&self, p: u64, b: &mut [u8]) -> Result<(), ()> {
            let n = p.checked_sub(0x10000).ok_or(())? as usize;
            let data = self.0.borrow();
            b.copy_from_slice(data.get(n..n + b.len()).ok_or(())?);
            Ok(())
        }
        fn write(&self, p: u64, b: &[u8]) -> Result<(), ()> {
            let n = p.checked_sub(0x10000).ok_or(())? as usize;
            self.0
                .borrow_mut()
                .get_mut(n..n + b.len())
                .ok_or(())?
                .copy_from_slice(b);
            Ok(())
        }
    }
    fn request(service: &mut Service<Ram>, node: u32, id: u16, tlvs: &[(u8, &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        for (tag, data) in tlvs {
            qmi::tlv(&mut b, *tag, data).unwrap();
        }
        service
            .request(node, &qmi::request(1, id, &b).unwrap())
            .unwrap()
    }
    #[test]
    fn refuses_cross_node_handles_and_preflights_entire_iovec() {
        let mut service = Service::new(Ram(RefCell::new([0x5a; 1024])));
        let b = request(&mut service, 0, 1, &[(1, b"/boot/modem_fs1")]);
        let m = qmi::Message::parse(&b).unwrap();
        assert!(m.result().is_ok());
        let id = word(m.tlv(16).unwrap()).unwrap().to_le_bytes();
        let b = request(&mut service, 1, 4, &[(1, &id), (2, &1024u32.to_le_bytes())]);
        assert!(qmi::Message::parse(&b).unwrap().result().is_err());
        let b = request(&mut service, 0, 4, &[(1, &id), (2, &1024u32.to_le_bytes())]);
        assert!(qmi::Message::parse(&b).unwrap().result().is_ok());
        let mut iov = alloc::vec![2];
        for (sector, physical) in [(0u32, 0x10000u32), (1, 0x20000)] {
            for v in [sector, physical, 1] {
                iov.extend_from_slice(&v.to_le_bytes());
            }
        }
        let b = request(
            &mut service,
            0,
            3,
            &[(1, &id), (2, &[0]), (3, &iov), (4, &[0])],
        );
        assert!(qmi::Message::parse(&b).unwrap().result().is_err());
        assert_eq!(*service.memory.0.borrow(), [0x5a; 1024]);
        iov.truncate(13);
        iov[0] = 1;
        let b = request(
            &mut service,
            0,
            3,
            &[(1, &id), (2, &[1]), (3, &iov), (4, &[0])],
        );
        assert!(qmi::Message::parse(&b).unwrap().result().is_ok());
        service.memory.0.borrow_mut().fill(0);
        let b = request(
            &mut service,
            0,
            3,
            &[(1, &id), (2, &[0]), (3, &iov), (4, &[0])],
        );
        assert!(qmi::Message::parse(&b).unwrap().result().is_ok());
        assert_eq!(&service.memory.0.borrow()[..512], &[0x5a; 512]);
    }
}
#[cfg(target_os = "none")]
pub mod native {
    use super::*;
    use qcom_scm::{Host, Permission};
    use scarlet::{
        arch::{self, mmio},
        device::fdt::FdtManager,
        vm,
    };
    pub struct Shared {
        base: usize,
        physical: u64,
        size: usize,
    }
    impl Memory for Shared {
        fn region(&self) -> (u64, usize) {
            (self.physical, self.size)
        }
        fn read(&self, physical: u64, bytes: &mut [u8]) -> Result<(), ()> {
            let off = physical.checked_sub(self.physical).ok_or(())? as usize;
            if off
                .checked_add(bytes.len())
                .is_none_or(|end| end > self.size)
            {
                return Err(());
            }
            arch::io_mb();
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = unsafe { mmio::read8(self.base + off + i) }
            }
            Ok(())
        }
        fn write(&self, physical: u64, bytes: &[u8]) -> Result<(), ()> {
            let off = physical.checked_sub(self.physical).ok_or(())? as usize;
            if off
                .checked_add(bytes.len())
                .is_none_or(|end| end > self.size)
            {
                return Err(());
            }
            for (i, b) in bytes.iter().enumerate() {
                unsafe { mmio::write8(self.base + off + i, *b) }
            }
            arch::io_mb();
            Ok(())
        }
    }
    pub fn prepare() -> Result<Service<Shared>, &'static str> {
        let fdt = FdtManager::get_manager()
            .get_fdt()
            .ok_or("rmtfs: FDT unavailable")?;
        let node = fdt
            .all_nodes()
            .find(|n| {
                n.compatible()
                    .is_some_and(|c| c.all().any(|s| s == "qcom,rmtfs-mem"))
            })
            .ok_or("rmtfs: missing reserved shared memory")?;
        let region = node
            .reg()
            .and_then(|mut r| r.next())
            .ok_or("rmtfs: missing shared memory range")?;
        let physical = region.starting_address as u64;
        let size = region.size.ok_or("rmtfs: missing shared memory length")?;
        if physical % 4096 != 0
            || size == 0
            || size > 8 * 1024 * 1024
            || size % 4096 != 0
            || physical
                .checked_add(size as u64)
                .is_none_or(|end| end > 1u64 << 32)
        {
            return Err("rmtfs: invalid reserved shared memory");
        }
        let mut owners = 1u64 << (Host::Hlos as u32);
        let permissions = [
            Permission {
                host: Host::Hlos,
                bits: 6,
            },
            Permission {
                host: Host::Mss,
                bits: 6,
            },
        ];
        unsafe { qcom_scm::assign_memory(physical, size, &mut owners, &permissions) }
            .map_err(|_| "rmtfs: SCM shared ownership failed")?;
        let base = vm::ioremap(physical, size)?;
        Ok(Service::new(Shared {
            base,
            physical,
            size,
        }))
    }
}
