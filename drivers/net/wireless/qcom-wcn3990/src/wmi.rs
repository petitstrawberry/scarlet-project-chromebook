// SPDX-License-Identifier: GPL-2.0-only
//! WMI-TLV wire formats (ath10k wmi-tlv.h/c). All lengths are checked before
//! firmware-provided memory requests become allocations or DMA addresses.
use alloc::{vec, vec::Vec};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Unsupported,
    Duplicate,
    Missing,
    Overflow,
}
pub fn word(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
pub fn tlvs(mut b: &[u8]) -> Result<Vec<(u16, &[u8])>, Error> {
    let mut records = Vec::new();
    while !b.is_empty() {
        if b.len() < 4 || records.len() >= 128 {
            return Err(Error::Malformed);
        }
        let len = u16::from_le_bytes(b[..2].try_into().unwrap()) as usize;
        let tag = u16::from_le_bytes(b[2..4].try_into().unwrap());
        let value = b.get(4..4 + len).ok_or(Error::Malformed)?;
        records.push((tag, value));
        b = &b[4 + len..];
    }
    Ok(records)
}
fn one<'a>(records: &[(u16, &'a [u8])], tag: u16) -> Result<&'a [u8], Error> {
    let mut found = records.iter().filter(|(id, _)| *id == tag);
    let value = found.next().ok_or(Error::Missing)?.1;
    if found.next().is_some() {
        return Err(Error::Duplicate);
    }
    Ok(value)
}
pub fn put(out: &mut Vec<u8>, tag: u16, bytes: &[u8]) -> Result<(), Error> {
    let n = u16::try_from(bytes.len()).map_err(|_| Error::Overflow)?;
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}
pub fn words(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|v| v.to_le_bytes()).collect()
}
#[derive(Debug, Clone, Copy)]
pub struct MemoryRequest {
    pub id: u32,
    pub unit_size: usize,
    pub units: usize,
}
impl MemoryRequest {
    pub fn bytes(self) -> Result<usize, Error> {
        self.unit_size
            .checked_add(3)
            .map(|v| v & !3)
            .and_then(|v| v.checked_mul(self.units))
            .filter(|v| *v > 0 && *v <= 16 * 1024 * 1024)
            .ok_or(Error::Overflow)
    }
}
pub struct ServiceReady {
    pub abi: [u32; 6],
    pub build: u32,
    pub chains: u32,
    pub services: Vec<u32>,
    pub memory: Vec<MemoryRequest>,
    pub regulatory: [u32; 9],
}
impl ServiceReady {
    pub fn service(&self, id: usize) -> bool {
        if id < self.services.len() * 4 {
            self.services[id / 4] & (1 << (id % 4)) != 0
        } else {
            false
        }
    }
    pub fn parse(event: &[u8]) -> Result<Self, Error> {
        if event.len() < 4 || word(event) & 0xffffff != 1 {
            return Err(Error::Malformed);
        }
        let records = tlvs(&event[4..])?;
        let ready = one(&records, 32)?;
        let regulatory = one(&records, 33)?;
        if ready.len() < 104 || regulatory.len() < 36 {
            return Err(Error::Malformed);
        }
        let abi = core::array::from_fn(|i| word(&ready[4 + i * 4..]));
        if abi[0] != 0x01000000 || abi[2..] != [0x5f414351, 0x4c4d, 0, 0] {
            return Err(Error::Unsupported);
        }
        let chains = word(&ready[36..]);
        if !(1..=4).contains(&chains) {
            return Err(Error::Malformed);
        }
        let count = word(&ready[72..]) as usize;
        if count > 16 {
            return Err(Error::Malformed);
        }
        let bitmap = records
            .iter()
            .find(|(tag, _)| *tag == 16)
            .ok_or(Error::Missing)?
            .1;
        if bitmap.is_empty() || bitmap.len() % 4 != 0 || bitmap.len() > 1024 {
            return Err(Error::Malformed);
        }
        let services = bitmap.chunks_exact(4).map(word).collect();
        let mut memory = Vec::new();
        let requests = one(&records, 18)?;
        for (tag, bytes) in tlvs(requests)? {
            if tag != 34 || bytes.len() != 16 {
                return Err(Error::Malformed);
            }
            let id = word(bytes);
            if memory
                .iter()
                .any(|request: &MemoryRequest| request.id == id)
            {
                return Err(Error::Duplicate);
            }
            let flags = word(&bytes[8..]);
            if flags & !7 != 0 {
                return Err(Error::Unsupported);
            }
            let units = if flags & 6 != 0 {
                34
            } else if flags & 1 != 0 {
                5
            } else {
                word(&bytes[12..]) as usize
            };
            let request = MemoryRequest {
                id,
                unit_size: word(&bytes[4..]) as usize,
                units,
            };
            request.bytes()?;
            memory.push(request);
        }
        if memory.len() != count
            || memory
                .iter()
                .try_fold(0usize, |sum, r| sum.checked_add(r.bytes().ok()?))
                .is_none_or(|size| size > 32 * 1024 * 1024)
        {
            return Err(Error::Malformed);
        }
        Ok(Self {
            abi,
            build: word(ready),
            chains,
            services,
            memory,
            regulatory: core::array::from_fn(|i| word(&regulatory[i * 4..])),
        })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct MemoryChunk {
    pub id: u32,
    pub address: u64,
    pub bytes: u32,
}
pub fn initialize(ready: &ServiceReady, chunks: &[MemoryChunk]) -> Result<Vec<u8>, Error> {
    if chunks.len() > 16 {
        return Err(Error::Malformed);
    }
    let mut out = 1u32.to_le_bytes().to_vec();
    put(
        &mut out,
        74,
        &words(&[
            0x01000000,
            53,
            0x5f414351,
            0x4c4d,
            0,
            0,
            chunks.len() as u32,
        ]),
    )?;
    let mut cfg = vec![0u32; 44];
    cfg[0] = 4;
    cfg[1] = 33;
    if ready.service(65) {
        cfg[2] = 4;
        cfg[3] = 4;
    }
    cfg[4] = 2;
    cfg[5] = 66;
    cfg[6] = 16;
    cfg[7] = (1 << ready.chains) - 1;
    cfg[8] = cfg[7];
    cfg[9..13].copy_from_slice(&[100, 100, 100, 40]);
    // WCN3990 metadata does not advertise raw mode. Match Linux Native Wi-Fi
    // decapsulation instead of assuming arbitrary raw-mode support.
    cfg[13] = 1;
    cfg[14] = 4;
    cfg[15] = 4;
    cfg[16] = 4;
    cfg[17] = 8;
    cfg[21] = 1024;
    cfg[22] = 2;
    cfg[27] = 2;
    cfg[28] = 1024;
    cfg[29] = 2;
    cfg[30] = 1;
    cfg[31] = 32;
    cfg[32] = 2;
    cfg[33] = 5;
    cfg[34] = 22;
    cfg[35] = 6;
    cfg[37] = 1;
    cfg[38] = 1;
    put(&mut out, 75, &words(&cfg))?;
    let mut records = Vec::new();
    for chunk in chunks {
        if chunk.bytes == 0
            || chunk
                .address
                .checked_add(u64::from(chunk.bytes))
                .is_none_or(|end| end > 1u64 << 35)
        {
            return Err(Error::Malformed);
        }
        put(
            &mut records,
            76,
            &words(&[
                chunk.id,
                chunk.address as u32,
                chunk.bytes,
                (chunk.address >> 32) as u32,
            ]),
        )?;
    }
    put(&mut out, 18, &records)?;
    Ok(out)
}
pub fn ready(event: &[u8]) -> Result<[u8; 6], Error> {
    if event.len() < 4 || word(event) & 0xffffff != 2 {
        return Err(Error::Malformed);
    }
    let records = tlvs(&event[4..])?;
    let bytes = one(&records, 35)?;
    if bytes.len() < 36 || word(&bytes[32..]) != 0 {
        return Err(Error::Malformed);
    }
    let mut mac = [0; 6];
    mac.copy_from_slice(&bytes[24..30]);
    Ok(mac)
}

pub fn command(id: u32, tag: u16, body: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = id.to_le_bytes().to_vec();
    put(&mut out, tag, body)?;
    Ok(out)
}
pub fn vdev_create(mac: [u8; 6]) -> Result<Vec<u8>, Error> {
    if mac == [0; 6] || mac[0] & 1 != 0 {
        return Err(Error::Malformed);
    }
    let mut body = words(&[0, 2, 0]);
    body.extend_from_slice(&mac);
    body.extend_from_slice(&[0; 2]);
    command(0x5001, 86, &body)
}
/// Passive world-SKU discovery on both bands, including DFS channels.
pub fn passive_channels() -> Result<Vec<u8>, Error> {
    let list = crate::channel::channels().collect::<Vec<_>>();
    let mut out = command(0x3003, 79, &words(&[list.len() as u32]))?;
    let mut records = Vec::new();
    for c in list {
        put(&mut records, 80, &channel(c, true)?)?;
    }
    put(&mut out, 18, &records)?;
    Ok(out)
}
pub fn passive_scan(serial: u16) -> Result<Vec<u8>, Error> {
    if serial > 0xfff {
        return Err(Error::Malformed);
    }
    let mut fields = vec![0u32; 25];
    fields[..15].copy_from_slice(&[
        0xa000 | u32::from(serial),
        0xa001,
        0,
        1,
        0x15f,
        50,
        150,
        50,
        500,
        0,
        0,
        0,
        20000,
        5,
        1 | 0x10 | 0x20,
    ]);
    fields[16] = crate::channel::channels().count() as u32;
    fields[17] = 1; // channels, broadcast BSSID
    fields[20] = 3;
    let mut out = command(0x3001, 77, &words(&fields))?;
    put(
        &mut out,
        16,
        &words(
            &crate::channel::channels()
                .filter_map(crate::channel::frequency)
                .collect::<Vec<_>>(),
        ),
    )?;
    put(&mut out, 19, &[])?;
    put(&mut out, 19, &[255, 255, 255, 255, 255, 255, 0, 0])?;
    put(&mut out, 17, &[])?;
    Ok(out)
}
pub struct Management<'a> {
    pub channel: u32,
    pub signal_dbm: i32,
    pub frame: &'a [u8],
}
pub fn management(event: &[u8]) -> Result<Management<'_>, Error> {
    if event.len() < 4 || word(event) & 0xffffff != 0x7001 {
        return Err(Error::Malformed);
    }
    let records = tlvs(&event[4..])?;
    let hdr = one(&records, 44)?;
    let body = one(&records, 17)?;
    if hdr.len() < 40 {
        return Err(Error::Malformed);
    }
    let len = word(&hdr[16..]) as usize;
    if word(&hdr[20..]) != 0 || len < 24 || len > body.len() {
        return Err(Error::Malformed);
    }
    Ok(Management {
        channel: word(hdr),
        signal_dbm: word(&hdr[4..]) as i32 - 95,
        frame: &body[..len],
    })
}
pub fn scan_event(event: &[u8]) -> Result<[u32; 6], Error> {
    if event.len() < 4 || word(event) & 0xffffff != 0x3001 {
        return Err(Error::Malformed);
    }
    let records = tlvs(&event[4..])?;
    let body = one(&records, 36)?;
    if body.len() < 24 {
        return Err(Error::Malformed);
    }
    Ok(core::array::from_fn(|i| word(&body[i * 4..])))
}

pub fn pdev_parameter(id: u32, value: u32) -> Result<Vec<u8>, Error> {
    command(0x4003, 82, &words(&[0, id, value]))
}
pub fn channel(channel: u8, passive: bool) -> Result<Vec<u8>, Error> {
    let mhz = crate::channel::frequency(channel).ok_or(Error::Unsupported)?;
    let flags = crate::channel::phy(channel)
        | if passive {
            0x80 | if crate::channel::dfs(channel) {
                1 << 10
            } else {
                0
            }
        } else {
            0
        };
    Ok(words(&[mhz, mhz, 0, flags, 20 << 8 | 20 << 16, 20 << 8]))
}
pub fn vdev_start(channel_number: u8, interval: u16) -> Result<Vec<u8>, Error> {
    let mut body = vec![0u32; 18];
    body[2] = u32::from(interval.max(1));
    body[3] = 1;
    let mut out = command(0x5003, 88, &words(&body))?;
    put(&mut out, 80, &channel(channel_number, false)?)?;
    put(&mut out, 18, &[])?;
    Ok(out)
}
pub fn peer_command(id: u32, tag: u16, peer: [u8; 6], tail: &[u32]) -> Result<Vec<u8>, Error> {
    let mut body = vec![0; 12];
    body[4..10].copy_from_slice(&peer);
    body.extend_from_slice(&words(tail));
    command(id, tag, &body)
}
pub fn peer_create(peer: [u8; 6]) -> Result<Vec<u8>, Error> {
    peer_command(0x6001, 97, peer, &[1])
}
pub fn peer_delete(peer: [u8; 6]) -> Result<Vec<u8>, Error> {
    peer_command(0x6002, 98, peer, &[])
}
pub fn peer_authorize(peer: [u8; 6]) -> Result<Vec<u8>, Error> {
    peer_command(0x6004, 100, peer, &[3, 1])
}
pub fn peer_associate(ap: &crate::sta::Bss, aid: u16) -> Result<Vec<u8>, Error> {
    let rates: Vec<u8> = ap
        .rates
        .iter()
        .map(|v| v & 0x7f)
        .filter(|v| crate::channel::rate_supported(ap.channel, *v))
        .collect();
    let mut fields = vec![0u32; 17];
    fields[1] = 1;
    fields[2] = u32::from(aid);
    // AUTH describes successful 802.11 authentication, not the host's RSN
    // controlled port. ath10k sets it for every associated peer. WPA2 needs
    // PTK four-way handling; NEED_GTK_2_WAY is only for the separate WPA1
    // group handshake, not the GTK carried in WPA2 message 3.
    fields[3] = 1 | if ap.rsn.is_empty() { 0 } else { 4 };
    fields[4] = u32::from(ap.capabilities);
    fields[5] = 10;
    fields[7] = 2346;
    fields[10] = 1;
    fields[12] = crate::channel::phy(ap.channel);
    fields[15] = rates.len() as u32;
    let mut body = vec![0; 8];
    body[..6].copy_from_slice(&ap.bssid);
    body.extend_from_slice(&words(&fields));
    let mut out = command(0x6005, 101, &body)?;
    let mut padded = rates;
    padded.resize(padded.len().next_multiple_of(4), 0);
    put(&mut out, 17, &padded)?;
    put(&mut out, 17, &[])?;
    put(&mut out, 102, &[0; 16])?;
    Ok(out)
}
pub fn vdev_up(peer: [u8; 6], aid: u16) -> Result<Vec<u8>, Error> {
    let mut body = words(&[0, u32::from(aid)]);
    body.extend_from_slice(&peer);
    body.extend_from_slice(&[0; 2]);
    command(0x5005, 92, &body)
}
pub fn management_tx(id: u32, address: u64, frame: &[u8]) -> Result<Vec<u8>, Error> {
    if frame.len() < 24 || frame.len() > 2048 || address >= 1 << 35 {
        return Err(Error::Malformed);
    }
    let n = frame.len().min(64).next_multiple_of(4);
    let mut body = words(&[
        0,
        id,
        0,
        address as u32,
        (address >> 32) as u32,
        frame.len() as u32,
        n as u32,
    ]);
    let mut out = command(0x7008, 422, &body)?;
    body.clear();
    body.extend_from_slice(&frame[..frame.len().min(n)]);
    body.resize(n, 0);
    put(&mut out, 17, &body)?;
    Ok(out)
}
pub fn fixed_event(event: &[u8], tag: u16, min: usize) -> Result<Vec<u32>, Error> {
    if event.len() < 4 {
        return Err(Error::Malformed);
    }
    let records = tlvs(&event[4..])?;
    let body = one(&records, tag)?;
    if body.len() < min || body.len() % 4 != 0 {
        return Err(Error::Malformed);
    }
    Ok(body.chunks_exact(4).map(word).collect())
}

/// Preserve the EEPROM world SKU, stripping only its roaming flag. Country
/// overrides are not guessed from locale or from a neighbouring AP.
pub fn world_regdomain(ready: &ServiceReady) -> Result<Vec<u8>, Error> {
    let rd = ready.regulatory[0] & !0x4000;
    if !(0x60..=0x6c).contains(&rd) {
        return Err(Error::Unsupported);
    }
    command(0x4001, 81, &words(&[0, rd, rd, rd, 255, 255]))
}

/// The key buffer contains exactly one CCMP-128 temporal key, or zeros for
/// pairwise deletion. GTK deletion follows Linux's random invalidation quirk.
pub fn install_key(
    peer: [u8; 6],
    index: u32,
    group: bool,
    cipher: u32,
    key: &[u8; 16],
    rsc: &[u8; 8],
) -> Result<Vec<u8>, Error> {
    if index > 3 || !matches!(cipher, 0 | 4) || rsc[6..] != [0; 2] {
        return Err(Error::Malformed);
    }
    let mut body = vec![0u8; 92];
    body[4..10].copy_from_slice(&peer);
    body[12..16].copy_from_slice(&index.to_le_bytes());
    body[16..20].copy_from_slice(&u32::from(group).to_le_bytes());
    body[20..24].copy_from_slice(&cipher.to_le_bytes());
    body[24..32].copy_from_slice(rsc);
    body[80..84].copy_from_slice(&16u32.to_le_bytes());
    let mut out = command(0x5009, 96, &body)?;
    put(&mut out, 17, key)?;
    Ok(out)
}

pub fn management_rate(ap: &crate::sta::Bss) -> Result<Vec<u8>, Error> {
    let rate = crate::channel::management_rate(ap.channel, &ap.rates).ok_or(Error::Unsupported)?;
    command(0x5008, 95, &words(&[0, 25, rate]))
}
pub fn flush_peer(peer: [u8; 6]) -> Result<Vec<u8>, Error> {
    peer_command(0x6003, 99, peer, &[u32::MAX])
}
