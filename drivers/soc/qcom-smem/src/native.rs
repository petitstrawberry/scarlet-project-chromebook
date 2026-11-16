// SPDX-License-Identifier: GPL-2.0-only
extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};
use scarlet::{
    arch::mmio,
    device::{
        fdt::FdtManager,
        manager::{DeviceManager, DriverPriority},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    sync::IrqSpinLock,
    time, vm,
};

use crate::layout::{self, Layout, Memory, Region};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    NotReady,
    LockTimeout,
    Layout(layout::Error),
}
impl From<layout::Error> for Error {
    fn from(error: layout::Error) -> Self {
        Self::Layout(error)
    }
}

struct Mapping {
    base: usize,
    size: usize,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}
#[derive(Clone, Copy)]
struct View {
    base: usize,
    size: usize,
}
impl Memory for View {
    fn len(&self) -> usize {
        self.size
    }
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: layout validates the aligned offset against this mapping.
        u32::from_le(unsafe { mmio::read32(self.base + offset) })
    }
    fn write32(&mut self, offset: usize, value: u32) {
        // SAFETY: layout validates the aligned offset against this mapping.
        unsafe {
            mmio::write32(self.base + offset, value.to_le());
        }
    }
    fn publish(&self) {
        release();
    }
}

fn release() {
    // Shared with a different processor, not just other AP cores.
    unsafe {
        core::arch::asm!("dmb oshst", options(nostack, preserves_flags));
    }
}
fn acquire() {
    unsafe {
        core::arch::asm!("dmb oshld", options(nostack, preserves_flags));
    }
}

struct Shared {
    physical: u64,
    memory: Mapping,
    mutex: Mapping,
    mutex_offset: usize,
    local: IrqSpinLock<()>,
    layout: Layout,
}
static PROVIDER: IrqSpinLock<Option<Arc<Shared>>> = IrqSpinLock::new(None);

/// Firmware-owned shared region, for bounded coprocessor IOMMU mappings.
pub fn physical_region() -> Option<(u64, usize)> {
    PROVIDER
        .lock()
        .as_ref()
        .map(|shared| (shared.physical, shared.memory.size))
}

struct RemoteLock(usize);
impl RemoteLock {
    fn take(address: usize) -> Result<Self, Error> {
        let started = time::current_time();
        loop {
            // TCSR accepts a new owner only when the mutex is unowned. An AP
            // local lock is necessary: all AP CPUs use the same owner ID (1).
            unsafe {
                mmio::write32(address, 1);
                if mmio::read32(address) == 1 {
                    acquire();
                    return Ok(Self(address));
                }
            }
            if time::current_time().wrapping_sub(started) >= 1000 {
                return Err(Error::LockTimeout);
            }
            core::hint::spin_loop();
        }
    }
}
impl Drop for RemoteLock {
    fn drop(&mut self) {
        release();
        // SAFETY: the local lock prevents another AP CPU from releasing us.
        unsafe {
            mmio::write32(self.0, 0);
        }
    }
}

impl Shared {
    fn lookup(self: &Arc<Self>, host: u16, item: u16, size: Option<usize>) -> Result<Item, Error> {
        let _local = self.local.lock();
        let _remote = RemoteLock::take(self.mutex.base + self.mutex_offset)?;
        let mut view = View {
            base: self.memory.base,
            size: self.memory.size,
        };
        let region = match size {
            Some(size) => self.layout.allocate(&mut view, host, item, size)?,
            None => self.layout.get(&view, host, item)?,
        };
        Ok(Item {
            owner: self.clone(),
            region,
        })
    }
}

/// Stable, bounded access to a firmware-visible allocation. Clones retain the
/// mapping. SMEM has no free operation; neither dropping an Item nor cancellation
/// reclaims storage or resets a peer's protocol state.
#[derive(Clone)]
pub struct Item {
    owner: Arc<Shared>,
    region: Region,
}
impl Item {
    pub fn len(&self) -> usize {
        self.region.len
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn address(&self, offset: usize, len: usize) -> Result<usize, Error> {
        if offset
            .checked_add(len)
            .is_none_or(|end| end > self.region.len)
        {
            return Err(layout::Error::Bounds.into());
        }
        Ok(self.owner.memory.base + self.region.offset + offset)
    }
    /// Acquire a peer-published index/state word before accessing its payload.
    pub fn read32(&self, offset: usize) -> Result<u32, Error> {
        let address = self.address(offset, 4)?;
        if address % 4 != 0 {
            return Err(layout::Error::Bounds.into());
        }
        let value = u32::from_le(unsafe { mmio::read32(address) });
        acquire();
        Ok(value)
    }
    /// Publish a producer index/state after writing the corresponding payload.
    pub fn write32(&self, offset: usize, value: u32) -> Result<(), Error> {
        let address = self.address(offset, 4)?;
        if address % 4 != 0 {
            return Err(layout::Error::Bounds.into());
        }
        release();
        unsafe {
            mmio::write32(address, value.to_le());
        }
        Ok(())
    }
    pub fn read(&self, offset: usize, bytes: &mut [u8]) -> Result<(), Error> {
        let address = self.address(offset, bytes.len())?;
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = unsafe { mmio::read8(address + i) };
        }
        Ok(())
    }
    pub fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        let address = self.address(offset, bytes.len())?;
        for (i, byte) in bytes.iter().enumerate() {
            unsafe {
                mmio::write8(address + i, *byte);
            }
        }
        Ok(())
    }
}

/// Obtain an existing global (`u16::MAX`) or remote-host partition item.
pub fn get(host: u16, item: u16) -> Result<Item, Error> {
    let owner = PROVIDER.lock().as_ref().cloned().ok_or(Error::NotReady)?;
    owner.lookup(host, item, None)
}
/// Allocate once under the firmware's hardware lock. Reopening an existing
/// exact-size item preserves its contents; callers must negotiate peer restart.
pub fn allocate(host: u16, item: u16, size: usize) -> Result<Item, Error> {
    let owner = PROVIDER.lock().as_ref().cloned().ok_or(Error::NotReady)?;
    owner.lookup(host, item, Some(size))
}

fn be_word(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if PROVIDER.lock().is_some() {
        return Err("qcom-smem: already registered");
    }
    let region =
        DeviceManager::get_manager().resolve_platform_memory_region(device, "memory-region", 0)?;
    let size = region.size()?;
    if region.start % 4096 != 0 || size < 8192 || size % 4096 != 0 {
        return Err("qcom-smem: invalid reserved memory");
    }
    let locks = device
        .property("hwlocks")
        .ok_or("qcom-smem: missing hwlocks")?
        .value();
    if locks.len() != 8 {
        return Err("qcom-smem: expected one hardware lock");
    }
    let phandle = be_word(locks).ok_or("qcom-smem: malformed lock phandle")?;
    let index = be_word(&locks[4..])
        .filter(|index| *index < 32)
        .ok_or("qcom-smem: invalid lock index")?;
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("qcom-smem: FDT unavailable")?;
    let node = fdt
        .all_nodes()
        .find(|node| {
            node.property("phandle")
                .or_else(|| node.property("linux,phandle"))
                .and_then(|property| be_word(property.value))
                == Some(phandle)
        })
        .ok_or("qcom-smem: lock provider absent")?;
    if !node
        .compatible()
        .is_some_and(|compatible| compatible.all().any(|value| value == "qcom,tcsr-mutex"))
        || node
            .property("#hwlock-cells")
            .and_then(|property| be_word(property.value))
            != Some(1)
        || node.property("status").is_some_and(|property| {
            property.as_str() != Some("okay") && property.as_str() != Some("ok")
        })
    {
        return Err("qcom-smem: unsupported lock provider");
    }
    let lock_region = node
        .reg()
        .and_then(|mut regions| regions.next())
        .ok_or("qcom-smem: lock has no reg")?;
    let lock_size = lock_region.size.ok_or("qcom-smem: lock has no size")?;
    let mutex_offset = index as usize * 0x1000;
    if mutex_offset + 4 > lock_size {
        return Err("qcom-smem: lock out of range");
    }
    let memory = Mapping {
        base: vm::ioremap(region.start, size)?,
        size,
    };
    let mutex = Mapping {
        base: vm::ioremap(lock_region.starting_address as u64, lock_size)?,
        size: lock_size,
    };
    let layout = {
        let _lock = RemoteLock::take(mutex.base + mutex_offset)
            .map_err(|_| "qcom-smem: remote lock timeout")?;
        let view = View {
            base: memory.base,
            size,
        };
        Layout::discover(&view).map_err(|error| {
            scarlet::println!("[qcom-smem] heap rejected: {:?}, initialized={:#x} version={:#x} toc={:#x} partitions={}",
                error, view.read32(192), view.read32(92), view.read32(size - 4096), view.read32(size - 4096 + 8));
            // Bounded startup-only metadata; never dump firmware payloads.
            for index in 0..view.read32(size - 4096 + 8).min(16) as usize {
                let entry = size - 4096 + 32 + index * 48;
                scarlet::println!("[qcom-smem] partition {} offset={:#x} size={:#x} hosts={:#x} cacheline={}",
                    index, view.read32(entry), view.read32(entry + 4), view.read32(entry + 12), view.read32(entry + 16));
            }
            "qcom-smem: invalid or unsupported firmware heap"
        })?
    };
    let modem = layout.has_private_partition(1);
    *PROVIDER.lock() = Some(Arc::new(Shared {
        physical: region.start,
        memory,
        mutex,
        mutex_offset,
        local: IrqSpinLock::new(()),
        layout,
    }));
    scarlet::println!(
        "[qcom-smem] v12 paddr={:#x} size={:#x} modem-partition={}",
        region.start,
        size,
        modem
    );
    Ok(())
}

fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let mut provider = PROVIDER.lock();
    if provider
        .as_ref()
        .is_some_and(|owner| Arc::strong_count(owner) != 1)
    {
        return Err("qcom-smem: active transport retains mapping");
    }
    let old = provider.take();
    drop(provider);
    drop(old);
    Ok(())
}
fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-smem",
            probe,
            remove,
            vec!["qcom,smem"],
        )),
        DriverPriority::Critical,
    );
}
scarlet::driver_initcall!(register_driver);
#[used]
static SCARLET_DRIVER_QCOM_SMEM_ANCHOR: fn() = crate::force_link;
