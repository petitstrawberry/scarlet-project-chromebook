// SPDX-License-Identifier: GPL-2.0-only
//! All firmware-visible allocations live in the device, including on failure.
use crate::{
    ce::{Bus, HostMemory},
    htt::*,
};
use alloc::{vec, vec::Vec};
pub struct Received {
    pub bytes: Vec<u8>,
    pub attention: u32,
    pub info: u8,
    pub descriptor: Vec<u8>,
}
pub struct Datapath {
    index: HostMemory,
    ring: HostMemory,
    rx: HostMemory,
    tx: HostMemory,
    fragments: HostMemory,
    mgmt: HostMemory,
    mgmt_owned: Vec<bool>,
    rx_owned: Vec<bool>,
    tx_owned: Vec<bool>,
    producer: usize,
}
impl Datapath {
    pub fn prepare(bus: &Bus) -> Result<Self, &'static str> {
        let this = Self {
            index: bus.host_memory(4)?,
            ring: bus.host_memory(RX_ENTRIES * 8)?,
            rx: bus.host_memory(RX_POSTED * RX_BYTES)?,
            tx: bus.host_memory(TX_ENTRIES * TX_BYTES)?,
            fragments: bus.host_memory(TX_ENTRIES * TX_DESCRIPTOR)?,
            mgmt: bus.host_memory(16 * 2048)?,
            mgmt_owned: vec![false; 16],
            rx_owned: vec![true; RX_POSTED],
            tx_owned: vec![false; TX_ENTRIES],
            producer: RX_POSTED,
        };
        for i in 0..RX_POSTED {
            this.ring.write(
                i * 8,
                &(this.rx.address() + (i * RX_BYTES) as u64).to_le_bytes(),
            )?;
        }
        this.index.write_index(this.producer as u32)?;
        Ok(this)
    }
    pub fn management(&mut self, frame: &[u8]) -> Result<(u32, Vec<u8>), &'static str> {
        if frame.len() > 2048 {
            return Err("htt: management frame too large");
        }
        let id = self
            .mgmt_owned
            .iter()
            .position(|v| !*v)
            .ok_or("htt: management queue full")?;
        self.mgmt.write(id * 2048, frame)?;
        let cmd =
            crate::wmi::management_tx(id as u32, self.mgmt.address() + (id * 2048) as u64, frame)
                .map_err(|_| "htt: management format")?;
        self.mgmt_owned[id] = true;
        Ok((id as u32, cmd))
    }
    pub fn management_done(&mut self, id: u32) -> Result<(), &'static str> {
        let slot = self
            .mgmt_owned
            .get_mut(id as usize)
            .ok_or("htt: foreign management completion")?;
        if !*slot {
            return Err("htt: duplicate management completion");
        }
        *slot = false;
        Ok(())
    }
    pub fn configuration(&self) -> Result<[Vec<u8>; 2], &'static str> {
        Ok([
            fragment_bank(self.fragments.address()).map_err(|_| "htt: fragment bank layout")?,
            rx_configuration(
                self.index.address(),
                self.ring.address(),
                self.producer as u16,
            )
            .map_err(|_| "htt: RX ring layout")?,
        ])
    }
    pub fn receive(&mut self, b: &[u8]) -> Result<Vec<Received>, &'static str> {
        let (info, ind) = rx_indications(b).map_err(|_| "htt: malformed RX completion")?;
        // Validate the whole batch before transferring any ownership.
        for item in &ind {
            let off = item
                .address
                .checked_sub(self.rx.address())
                .ok_or("htt: foreign RX address")? as usize;
            let i = off / RX_BYTES;
            if off % RX_BYTES != 0
                || i >= RX_POSTED
                || !self.rx_owned[i]
                || item.len > RX_BYTES
                || (info & 0x20 == 0 && item.len > RX_BYTES - RX_DESCRIPTOR)
            {
                return Err("htt: invalid RX ownership/length");
            }
        }
        let mut out = Vec::with_capacity(ind.len());
        for item in ind {
            let off = (item.address - self.rx.address()) as usize;
            let i = off / RX_BYTES;
            self.rx_owned[i] = false;
            let (bytes, descriptor, attention) = if info & 0x20 != 0 {
                let header = self.rx.read(off, 7)?;
                let n = u16::from_le_bytes([header[0], header[1]]) as usize;
                if n < 24 || n > RX_BYTES - 7 || header[4] != 0 || header[5] > 16 {
                    return Err("htt: malformed offload MSDU");
                }
                (self.rx.read(off + 7, n)?, Vec::new(), 0)
            } else {
                let desc = self.rx.read(off, RX_DESCRIPTOR)?;
                let attention = u32::from_le_bytes(desc[4..8].try_into().unwrap());
                if attention & (1 << 31) == 0 {
                    return Err("htt: RX completion precedes MSDU_DONE");
                }
                (
                    self.rx.read(off + RX_DESCRIPTOR, item.len)?,
                    desc,
                    attention,
                )
            };
            out.push(Received {
                bytes,
                descriptor,
                attention,
                info,
            });
            // Clear attention before returning the lease. Publish producer last.
            self.rx.write(off + 4, &[0; 4])?;
            self.ring
                .write(self.producer * 8, &item.address.to_le_bytes())?;
            self.producer = (self.producer + 1) & (RX_ENTRIES - 1);
            self.rx_owned[i] = true;
        }
        self.index.write_index(self.producer as u32)?;
        Ok(out)
    }
    pub fn outstanding(&self) -> usize {
        self.tx_owned.iter().filter(|v| **v).count()
    }
    pub fn management_outstanding(&self) -> bool {
        self.mgmt_owned.iter().any(|owned| *owned)
    }
    pub fn complete(&mut self, b: &[u8]) -> Result<Vec<(u16, u8)>, &'static str> {
        let (status, ids) = tx_completions(b).map_err(|_| "htt: malformed TX completion")?;
        if ids.iter().any(|id| !self.tx_owned[*id as usize]) {
            return Err("htt: foreign/duplicate TX completion");
        }
        for id in &ids {
            self.tx_owned[*id as usize] = false;
        }
        Ok(ids.into_iter().map(|id| (id, status)).collect())
    }
    pub fn transmit(
        &mut self,
        frame: &[u8],
        tid: u8,
        encrypted: bool,
    ) -> Result<(u16, Vec<u8>), &'static str> {
        if frame.len() < 24 || frame.len() > TX_BYTES || tid > 16 {
            return Err("htt: invalid MPDU");
        }
        let id = self
            .tx_owned
            .iter()
            .position(|owned| !*owned)
            .ok_or("htt: TX queue full")?;
        self.tx.write(id * TX_BYTES, frame)?;
        let address = self.tx.address() + (id * TX_BYTES) as u64;
        let mut ext = [0u8; TX_DESCRIPTOR];
        ext[24..28].copy_from_slice(&(address as u32).to_le_bytes());
        ext[28..30].copy_from_slice(&((address >> 32) as u16).to_le_bytes());
        ext[30..32].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        self.fragments.write(id * TX_DESCRIPTOR, &ext)?;
        // A single CE descriptor contains HTT header and bounded prefetch.
        // Full MPDU remains mapped separately until HTT completion, not CE copy.
        let prefetch = frame.len().min(50).next_multiple_of(4);
        let mut cmd = vec![0; 20 + prefetch];
        cmd[0] = 1;
        cmd[1] = 1 | 2 | (1 << 5) | if encrypted { 0 } else { 4 }; // Native Wi-Fi, host-selected cipher admission
        cmd[2..4].copy_from_slice(&((u16::from(tid) << 6) | (1 << 11)).to_le_bytes());
        cmd[4..6].copy_from_slice(&(frame.len() as u16).to_le_bytes());
        cmd[6..8].copy_from_slice(&(id as u16).to_le_bytes());
        cmd[8..16].copy_from_slice(
            &(self.fragments.address() + (id * TX_DESCRIPTOR) as u64).to_le_bytes(),
        );
        cmd[16..20].copy_from_slice(&0xffffu32.to_le_bytes());
        let n = frame.len().min(prefetch);
        cmd[20..20 + n].copy_from_slice(&frame[..n]);
        self.tx_owned[id] = true;
        Ok((id as u16, cmd))
    }
    /// Only for a command never accepted by HTC/CE. Submitted frames require
    /// a real HTT completion or a confirmed firmware shutdown barrier.
    pub fn cancel_unsubmitted(&mut self, id: u16) {
        self.tx_owned[id as usize] = false;
    }
}
