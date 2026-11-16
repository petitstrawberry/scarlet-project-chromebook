// SPDX-License-Identifier: GPL-2.0-only
//! Bounded infrastructure STA management frames. No credential material.
use alloc::{vec, vec::Vec};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Unsupported,
    Peer,
    Rejected(u16),
}
fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
pub fn ies(mut b: &[u8]) -> Result<Vec<(u8, &[u8])>, Error> {
    let mut out = Vec::new();
    while !b.is_empty() {
        if b.len() < 2 || out.len() >= 128 {
            return Err(Error::Malformed);
        }
        let n = b[1] as usize;
        let value = b.get(2..2 + n).ok_or(Error::Malformed)?;
        out.push((b[0], value));
        b = &b[2 + n..];
    }
    Ok(out)
}
#[derive(Clone)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub ssid: Vec<u8>,
    pub channel: u8,
    pub signal_dbm: i32,
    pub interval: u16,
    pub capabilities: u16,
    pub rates: Vec<u8>,
    pub rsn: Vec<u8>,
    pub rsnxe: Vec<u8>,
    pub timestamp: u64,
}
pub fn beacon(frame: &[u8], channel: u8, signal_dbm: i32, timestamp: u64) -> Result<Bss, Error> {
    if frame.len() < 36 || !matches!(frame[0], 0x80 | 0x50) || frame[1] & 0x43 != 0 {
        return Err(Error::Malformed);
    }
    let bssid: [u8; 6] = frame[16..22].try_into().unwrap();
    if bssid == [0; 6] || bssid[0] & 1 != 0 || frame[10..16] != bssid {
        return Err(Error::Peer);
    }
    let capabilities = le16(frame, 34);
    if capabilities & 3 != 1 {
        return Err(Error::Unsupported);
    }
    let records = ies(&frame[36..])?;
    let mut ssid = None;
    let mut rates = Vec::new();
    let mut rsn = Vec::new();
    let mut rsnxe = Vec::new();
    let mut channel = channel;
    for (id, value) in records {
        match id {
            0 => {
                if ssid.is_some() || value.len() > 32 {
                    return Err(Error::Malformed);
                }
                ssid = Some(value.to_vec());
            }
            1 | 50 => {
                if rates.len() + value.len() > 32 {
                    return Err(Error::Malformed);
                }
                rates.extend_from_slice(value);
            }
            3 => {
                if value.len() != 1 {
                    return Err(Error::Malformed);
                }
                channel = value[0];
            }
            61 if value.len() >= 22 => {
                channel = value[0];
            }
            48 => {
                if !rsn.is_empty() || value.len() > 62 {
                    return Err(Error::Malformed);
                }
                rsn.extend_from_slice(&[48, value.len() as u8]);
                rsn.extend_from_slice(value);
            }
            244 => {
                if !rsnxe.is_empty() || value.is_empty() || value.len() > 62 {
                    return Err(Error::Malformed);
                }
                rsnxe.extend_from_slice(&[244, value.len() as u8]);
                rsnxe.extend_from_slice(value);
            }
            _ => {}
        }
    }
    if !crate::channel::supported(channel)
        || rates.is_empty()
        || rsn.len() + rsnxe.len() > 64
        || (rsn.is_empty() && !rsnxe.is_empty())
    {
        return Err(Error::Unsupported);
    }
    if capabilities & 16 != 0 && !rsn_psk_ccmp(&rsn) {
        return Err(Error::Unsupported);
    }
    Ok(Bss {
        bssid,
        ssid: ssid.ok_or(Error::Malformed)?,
        channel,
        signal_dbm,
        interval: le16(frame, 32),
        capabilities,
        rates,
        rsn,
        rsnxe,
        timestamp,
    })
}
pub fn rsn_psk_ccmp(ie: &[u8]) -> bool {
    if ie.len() < 20
        || ie[0] != 48
        || ie[1] as usize + 2 != ie.len()
        || le16(ie, 2) != 1
        || ie[4..8] != [0, 15, 172, 4]
    {
        return false;
    }
    let pairs = le16(ie, 8) as usize;
    let end = 10 + pairs * 4;
    if pairs == 0
        || end + 2 > ie.len()
        || !ie[10..end].chunks_exact(4).any(|v| v == [0, 15, 172, 4])
    {
        return false;
    }
    let akms = le16(ie, end) as usize;
    let stop = end + 2 + akms * 4;
    if akms == 0
        || stop > ie.len()
        || !ie[end + 2..stop]
            .chunks_exact(4)
            .any(|v| v == [0, 15, 172, 2])
    {
        return false;
    }
    if stop < ie.len() && (stop + 2 > ie.len() || le16(ie, stop) & (1 << 6) != 0) {
        return false;
    }
    true
}
fn header(subtype: u8, own: [u8; 6], peer: [u8; 6], sequence: u16) -> Vec<u8> {
    let mut b = vec![0u8; 24];
    b[0] = subtype;
    b[4..10].copy_from_slice(&peer);
    b[10..16].copy_from_slice(&own);
    b[16..22].copy_from_slice(&peer);
    b[22..24].copy_from_slice(&((sequence & 0xfff) << 4).to_le_bytes());
    b
}
pub fn authenticate(own: [u8; 6], peer: [u8; 6], sequence: u16) -> Vec<u8> {
    let mut b = header(0xb0, own, peer, sequence);
    b.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
    b
}
pub fn associate(own: [u8; 6], ap: &Bss, sequence: u16, rsn: &[u8]) -> Result<Vec<u8>, Error> {
    if !rsn.is_empty() && !rsn_psk_ccmp(rsn) {
        return Err(Error::Unsupported);
    }
    for rate in &ap.rates {
        if rate & 0x80 != 0 && !crate::channel::rate_supported(ap.channel, *rate) {
            return Err(Error::Unsupported);
        }
    }
    let rates: Vec<_> = ap
        .rates
        .iter()
        .copied()
        .filter(|v| crate::channel::rate_supported(ap.channel, *v))
        .collect();
    if rates.is_empty() {
        return Err(Error::Unsupported);
    }
    let mut b = header(0, own, ap.bssid, sequence);
    b.extend_from_slice(&(ap.capabilities & (1 | 16 | 32 | 1024)).to_le_bytes());
    b.extend_from_slice(&10u16.to_le_bytes());
    b.extend_from_slice(&[0, ap.ssid.len() as u8]);
    b.extend_from_slice(&ap.ssid);
    let n = rates.len().min(8);
    b.extend_from_slice(&[1, n as u8]);
    b.extend_from_slice(&rates[..n]);
    if rates.len() > n {
        b.extend_from_slice(&[50, (rates.len() - n) as u8]);
        b.extend_from_slice(&rates[n..]);
    }
    b.extend_from_slice(rsn);
    Ok(b)
}
pub fn auth_response(frame: &[u8], own: [u8; 6], peer: [u8; 6]) -> Result<(), Error> {
    validate_peer(frame, 0xb0, own, peer)?;
    if frame.len() < 30 || le16(frame, 24) != 0 || le16(frame, 26) != 2 {
        return Err(Error::Malformed);
    }
    let status = le16(frame, 28);
    if status != 0 {
        return Err(Error::Rejected(status));
    }
    Ok(())
}
pub fn assoc_response(frame: &[u8], own: [u8; 6], peer: [u8; 6]) -> Result<u16, Error> {
    validate_peer(frame, 0x10, own, peer)?;
    if frame.len() < 30 {
        return Err(Error::Malformed);
    }
    let status = le16(frame, 26);
    if status != 0 {
        return Err(Error::Rejected(status));
    }
    let aid = le16(frame, 28) & 0x3fff;
    if !(1..=2007).contains(&aid) {
        return Err(Error::Malformed);
    }
    ies(&frame[30..])?;
    Ok(aid)
}
pub fn validate_peer(frame: &[u8], subtype: u8, own: [u8; 6], peer: [u8; 6]) -> Result<(), Error> {
    if frame.len() < 24 || frame[0] != subtype || frame[1] & 0xc7 != 0 {
        return Err(Error::Malformed);
    }
    if frame[4..10] != own || frame[10..16] != peer || frame[16..22] != peer {
        return Err(Error::Peer);
    }
    Ok(())
}
pub fn deauthenticate(own: [u8; 6], peer: [u8; 6], sequence: u16) -> Vec<u8> {
    let mut b = header(0xc0, own, peer, sequence);
    b.extend_from_slice(&3u16.to_le_bytes());
    b
}

pub fn encapsulate(
    ethernet: &[u8],
    own: [u8; 6],
    peer: [u8; 6],
    sequence: u16,
) -> Result<Vec<u8>, Error> {
    if ethernet.len() < 14 || ethernet.len() > 1514 || ethernet[6..12] != own {
        return Err(Error::Malformed);
    }
    let mut b = header(8, own, peer, sequence);
    b[1] = 1;
    b[16..22].copy_from_slice(&ethernet[..6]);
    b.extend_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0]);
    b.extend_from_slice(&ethernet[12..]);
    Ok(b)
}
/// Raw receive MPDUs contain FCS; remove it before CCMP authentication. CCMP
/// frames stay intact in the private transport envelope for the common facade.
pub fn decapsulate(mpdu: &[u8], own: [u8; 6], peer: [u8; 6]) -> Result<Vec<u8>, Error> {
    if mpdu.len() < 28
        || !matches!(mpdu[0], 8 | 0x88)
        || mpdu[1] & 0x87 != 2
        || mpdu[22] & 15 != 0
        || mpdu[10..16] != peer
        || (mpdu[4] & 1 == 0 && mpdu[4..10] != own)
    {
        return Err(Error::Malformed);
    }
    let n = mpdu.len() - 4;
    if mpdu[1] & 0x40 != 0 {
        let mut b = Vec::with_capacity(14 + n);
        b.extend_from_slice(&own);
        b.extend_from_slice(&peer);
        b.extend_from_slice(&[0x88, 0xb5]);
        b.extend_from_slice(&mpdu[..n]);
        return Ok(b);
    }
    let h = if mpdu[0] == 0x88 { 26 } else { 24 };
    if n < h + 8
        || mpdu[h..h + 6] != [0xaa, 0xaa, 3, 0, 0, 0]
        || (h == 26 && (mpdu[24] & 0x80 != 0 || mpdu[25] != 0))
    {
        return Err(Error::Unsupported);
    }
    let mut b = Vec::with_capacity(14 + n - h - 8);
    b.extend_from_slice(&mpdu[4..10]);
    b.extend_from_slice(&mpdu[16..22]);
    b.extend_from_slice(&mpdu[h + 6..n]);
    Ok(b)
}

/// Firmware Native Wi-Fi has no cipher parameters, MIC or FCS. It may also
/// emit ordinary Ethernet DIX packets; neither path accepts foreign peers.
pub fn native_ethernet(
    frame: &[u8],
    mode: u8,
    own: [u8; 6],
    peer: [u8; 6],
) -> Result<Vec<u8>, Error> {
    if mode == 0 {
        if frame.len() < 28 || frame[1] & 0x40 != 0 {
            return Err(Error::Unsupported);
        }
        return decapsulate(frame, own, peer);
    }
    if mode == 2 {
        if frame.len() < 14 || frame.len() > 1514 || (frame[0] & 1 == 0 && frame[..6] != own) {
            return Err(Error::Peer);
        }
        return Ok(frame.to_vec());
    }
    if mode != 1
        || frame.len() < 32
        || frame.len() > 1600
        || frame[0] & 0x0c != 8
        || frame[1] & 0x87 != 2
        || frame[22] & 15 != 0
        || frame[10..16] != peer
        || (frame[4] & 1 == 0 && frame[4..10] != own)
    {
        return Err(Error::Malformed);
    }
    let h = if frame[0] & 0x80 != 0 { 28 } else { 24 };
    if frame.len() < h + 8 || frame[h..h + 6] != [0xaa, 0xaa, 3, 0, 0, 0] {
        return Err(Error::Unsupported);
    }
    let mut b = Vec::with_capacity(frame.len() - h + 6);
    b.extend_from_slice(&frame[4..10]);
    b.extend_from_slice(&frame[16..22]);
    b.extend_from_slice(&frame[h + 6..]);
    Ok(b)
}
