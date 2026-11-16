// SPDX-License-Identifier: GPL-2.0-only
//! WCN3990 HTT 3.x, 64-bit DMA wire formats. Addresses remain leases until
//! firmware completion; a CE transfer only retires its control-message copy.
use alloc::{vec, vec::Vec};
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Address,
    Duplicate,
    Ownership,
    Full,
}
pub const RX_ENTRIES: usize = 2048;
pub const RX_POSTED: usize = 2047;
pub const RX_BYTES: usize = 2048;
pub const RX_DESCRIPTOR: usize = 360;
pub const TX_ENTRIES: usize = 128;
pub const TX_BYTES: usize = 4096;
pub const TX_DESCRIPTOR: usize = 72;
fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
pub fn rx_configuration(index: u64, ring: u64, producer: u16) -> Result<Vec<u8>, Error> {
    if index & 3 != 0
        || ring & 7 != 0
        || index >= 1 << 35
        || ring >= 1 << 35
        || producer as usize >= RX_ENTRIES
    {
        return Err(Error::Address);
    }
    let mut b = vec![2, 1, 0, 0];
    b.extend_from_slice(&index.to_le_bytes());
    b.extend_from_slice(&ring.to_le_bytes());
    for v in [
        RX_ENTRIES as u16,
        RX_BYTES as u16,
        0xffff,
        producer,
        296 / 4,
        360 / 4,
        108 / 4,
        148 / 4,
        16 / 4,
        104 / 4,
        28 / 4,
        48 / 4,
        4 / 4,
        8 / 4,
    ] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    Ok(b)
}
pub fn fragment_bank(address: u64) -> Result<Vec<u8>, Error> {
    if address & 7 != 0 || address >= 1 << 35 {
        return Err(Error::Address);
    }
    let mut b = vec![0; 64];
    b[0] = 6;
    b[2] = 1;
    b[3] = TX_DESCRIPTOR as u8;
    b[4..12].copy_from_slice(&address.to_le_bytes());
    b[38..40].copy_from_slice(&(TX_ENTRIES as u16 - 1).to_le_bytes());
    // q_state is absent, not advertised as valid.
    Ok(b)
}
#[derive(Debug)]
pub struct RxIndication {
    pub address: u64,
    pub len: usize,
    pub fw_desc: u8,
}
pub fn rx_indications(b: &[u8]) -> Result<(u8, Vec<RxIndication>), Error> {
    if b.len() < 8 || b[0] != 0x12 {
        return Err(Error::Malformed);
    }
    let count = u16_at(b, 6) as usize;
    if count == 0 || count > RX_POSTED || 8 + count * 12 > b.len() {
        return Err(Error::Malformed);
    }
    let mut out = Vec::with_capacity(count);
    for p in b[8..8 + count * 12].chunks_exact(12) {
        let address = u64::from_le_bytes(p[..8].try_into().unwrap());
        if out.iter().any(|v: &RxIndication| v.address == address) {
            return Err(Error::Duplicate);
        }
        out.push(RxIndication {
            address,
            len: u16_at(p, 8) as usize,
            fw_desc: p[10],
        });
    }
    Ok((b[1], out))
}
pub fn tx_completions(b: &[u8]) -> Result<(u8, Vec<u16>), Error> {
    if b.len() < 4 || b[0] != 7 || b.len() < 4 + b[2] as usize * 2 {
        return Err(Error::Malformed);
    }
    let mut ids = Vec::with_capacity(b[2] as usize);
    for v in b[4..4 + b[2] as usize * 2].chunks_exact(2) {
        let id = u16_at(v, 0);
        if id == 0xffff {
            continue;
        }
        if id as usize >= TX_ENTRIES {
            return Err(Error::Ownership);
        }
        if ids.contains(&id) {
            return Err(Error::Duplicate);
        }
        ids.push(id);
    }
    Ok((b[1] & 7, ids))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layouts_and_completion_bounds() {
        let b = rx_configuration(0x1000, 0x2000, 2047).unwrap();
        assert_eq!(b.len(), 48);
        assert_eq!(u16_at(&b, 28), 74);
        assert_eq!(fragment_bank(0x3000).unwrap().len(), 64);
        assert_eq!(tx_completions(&[7, 0, 2, 0, 1, 0]), Err(Error::Malformed));
        assert_eq!(
            tx_completions(&[7, 0, 2, 0, 1, 0, 1, 0]),
            Err(Error::Duplicate)
        );
        assert_eq!(tx_completions(&[7, 0, 1, 0, 128, 0]), Err(Error::Ownership));
    }
}
