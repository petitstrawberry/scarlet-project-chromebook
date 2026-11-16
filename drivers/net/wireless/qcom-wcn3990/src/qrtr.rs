// SPDX-License-Identifier: GPL-2.0-only
//! QRTR v1/v3 wire boundary. Linux v6.12 net/qrtr/af_qrtr.c.
use alloc::vec::Vec;
pub const CTRL: u32 = 0xffff_fffe;
pub const BROADCAST: u32 = u32::MAX;
pub const LOCAL: u32 = 1;
pub const CLIENT: u32 = 0x4000;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Malformed,
    Unsupported,
}
fn word(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn half(b: &[u8]) -> u32 {
    u16::from_le_bytes(b[..2].try_into().unwrap()) as u32
}
fn port(port: u32) -> u32 {
    if port == 0xfffe || port == 0xffff {
        CTRL
    } else {
        port
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub node: u32,
    pub port: u32,
}
#[derive(Debug)]
pub struct Packet<'a> {
    pub kind: u32,
    pub source: Address,
    pub destination: Address,
    pub confirm: bool,
    pub data: &'a [u8],
}
pub fn parse(b: &[u8]) -> Result<Packet<'_>, Error> {
    if b.len() % 4 != 0 {
        return Err(Error::Malformed);
    }
    let (header, size, kind, source, destination, confirm) = match b.first() {
        Some(1) => {
            if b.len() < 32 {
                return Err(Error::Truncated);
            }
            if word(b) != 1 || word(&b[16..]) > 1 {
                return Err(Error::Malformed);
            }
            (
                32,
                word(&b[20..]) as usize,
                word(&b[4..]),
                Address {
                    node: word(&b[8..]),
                    port: port(word(&b[12..])),
                },
                Address {
                    node: word(&b[24..]),
                    port: port(word(&b[28..])),
                },
                word(&b[16..]) != 0,
            )
        }
        Some(3) => {
            if b.len() < 16 {
                return Err(Error::Truncated);
            }
            if b[2] & !1 != 0 {
                return Err(Error::Unsupported);
            }
            (
                16 + b[3] as usize,
                word(&b[4..]) as usize,
                u32::from(b[1]),
                Address {
                    node: half(&b[8..]),
                    port: port(half(&b[10..])),
                },
                Address {
                    node: half(&b[12..]),
                    port: port(half(&b[14..])),
                },
                b[2] & 1 != 0,
            )
        }
        Some(_) => return Err(Error::Unsupported),
        None => return Err(Error::Truncated),
    };
    let padded = size.checked_add(3).ok_or(Error::Malformed)? & !3;
    if size == 0 || header.checked_add(padded) != Some(b.len()) || !(1..=11).contains(&kind) {
        return Err(Error::Malformed);
    }
    if destination.node != LOCAL && destination.node != BROADCAST {
        return Err(Error::Unsupported);
    }
    if destination.port != CTRL && kind != 1 && kind != 7 {
        return Err(Error::Malformed);
    }
    let minimum = match kind {
        1 => 1,
        2 | 8 | 9 => 4,
        3 | 6 => 12,
        _ => 20,
    };
    if size < minimum {
        return Err(Error::Truncated);
    }
    Ok(Packet {
        kind,
        source,
        destination,
        confirm,
        data: &b[header..header + size],
    })
}
pub fn encode(
    kind: u32,
    source: Address,
    destination: Address,
    confirm: bool,
    data: &[u8],
) -> Vec<u8> {
    let mut b = Vec::with_capacity(32 + ((data.len() + 3) & !3));
    for v in [
        1,
        kind,
        source.node,
        source.port,
        confirm as u32,
        data.len() as u32,
        destination.node,
        destination.port,
    ] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(data);
    b.resize((b.len() + 3) & !3, 0);
    b
}
pub fn control(kind: u32, destination: Address, fields: [u32; 4]) -> Vec<u8> {
    let mut body = Vec::with_capacity(20);
    for word in [kind, fields[0], fields[1], fields[2], fields[3]] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    encode(
        kind,
        Address {
            node: LOCAL,
            port: CTRL,
        },
        destination,
        false,
        &body,
    )
}
impl Packet<'_> {
    pub fn control_valid(&self) -> bool {
        self.kind != 1 && self.data.len() >= 4 && word(self.data) == self.kind
    }
    pub fn fields(&self) -> Result<[u32; 4], Error> {
        if self.data.len() < 20 || word(self.data) != self.kind {
            return Err(Error::Malformed);
        }
        Ok([
            word(&self.data[4..]),
            word(&self.data[8..]),
            word(&self.data[12..]),
            word(&self.data[16..]),
        ])
    }
    pub fn resume(&self) -> Option<Vec<u8>> {
        if !self.confirm {
            return None;
        }
        let mut body = Vec::new();
        for v in [7, self.destination.node, self.destination.port, 0, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        Some(encode(7, self.destination, self.source, false, &body))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_lengths_and_accepts_padding() {
        let b = encode(
            1,
            Address { node: 0, port: 7 },
            Address {
                node: LOCAL,
                port: CLIENT,
            },
            true,
            &[1, 2, 3],
        );
        let p = parse(&b).unwrap();
        assert_eq!(p.data, &[1, 2, 3]);
        assert!(p.resume().is_some());
        for n in 0..b.len() {
            assert!(parse(&b[..n]).is_err());
        }
    }
    #[test]
    fn accepts_v3_with_optional_header_and_legacy_control_port() {
        let mut b = alloc::vec![3, 2, 0, 4];
        b.extend_from_slice(&20u32.to_le_bytes());
        for n in [0u16, 0xfffe, 1, 0xfffe] {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&[0; 16]);
        let p = parse(&b).unwrap();
        assert_eq!(p.source.port, CTRL);
        assert_eq!(p.fields().unwrap(), [0; 4]);
    }
}
