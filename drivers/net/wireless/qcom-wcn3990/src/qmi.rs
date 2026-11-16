// SPDX-License-Identifier: GPL-2.0-only
use alloc::vec::Vec;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Duplicate,
    Missing,
    Rejected(u16),
}
pub struct Message<'a> {
    pub kind: u8,
    pub transaction: u16,
    pub id: u16,
    body: &'a [u8],
}
impl<'a> Message<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Self, Error> {
        if b.len() < 7 || !matches!(b[0], 0 | 2 | 4) {
            return Err(Error::Malformed);
        }
        let len = u16::from_le_bytes(b[5..7].try_into().unwrap()) as usize;
        if len + 7 != b.len() {
            return Err(Error::Malformed);
        }
        let message = Self {
            kind: b[0],
            transaction: u16::from_le_bytes(b[1..3].try_into().unwrap()),
            id: u16::from_le_bytes(b[3..5].try_into().unwrap()),
            body: &b[7..],
        };
        let mut seen = [false; 256];
        let mut tail = message.body;
        while !tail.is_empty() {
            if tail.len() < 3 {
                return Err(Error::Malformed);
            }
            let tag = tail[0] as usize;
            if seen[tag] {
                return Err(Error::Duplicate);
            }
            seen[tag] = true;
            let n = u16::from_le_bytes(tail[1..3].try_into().unwrap()) as usize;
            tail = tail.get(3 + n..).ok_or(Error::Malformed)?;
        }
        Ok(message)
    }
    pub fn tlv(&self, tag: u8) -> Option<&'a [u8]> {
        let mut tail = self.body;
        while !tail.is_empty() {
            let n = u16::from_le_bytes(tail[1..3].try_into().unwrap()) as usize;
            if tail[0] == tag {
                return Some(&tail[3..3 + n]);
            }
            tail = &tail[3 + n..];
        }
        None
    }
    pub fn result(&self) -> Result<(), Error> {
        let value = self.tlv(2).ok_or(Error::Missing)?;
        if value.len() != 4 {
            return Err(Error::Malformed);
        }
        let result = u16::from_le_bytes(value[..2].try_into().unwrap());
        let error = u16::from_le_bytes(value[2..].try_into().unwrap());
        match result {
            0 => Ok(()),
            1 => Err(Error::Rejected(error)),
            _ => Err(Error::Malformed),
        }
    }
}
pub fn tlv(body: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), Error> {
    let len = u16::try_from(value.len()).map_err(|_| Error::Malformed)?;
    body.push(tag);
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(value);
    Ok(())
}
pub fn request(transaction: u16, id: u16, body: &[u8]) -> Result<Vec<u8>, Error> {
    let n = u16::try_from(body.len()).map_err(|_| Error::Malformed)?;
    if transaction == 0 {
        return Err(Error::Malformed);
    }
    let mut b = Vec::with_capacity(7 + body.len());
    b.push(0);
    b.extend_from_slice(&transaction.to_le_bytes());
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(body);
    Ok(b)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_each_tlv_before_lookup() {
        let mut body = Vec::new();
        tlv(&mut body, 2, &[0, 0, 0, 0]).unwrap();
        let mut b = request(1, 0x20, &body).unwrap();
        b[0] = 2;
        assert!(Message::parse(&b).unwrap().result().is_ok());
        b[8] = 255;
        assert!(Message::parse(&b).is_err());
    }
    #[test]
    fn rejects_duplicate_results() {
        let mut body = Vec::new();
        for _ in 0..2 {
            tlv(&mut body, 2, &[0, 0, 0, 0]).unwrap();
        }
        let b = request(1, 0x20, &body).unwrap();
        assert!(matches!(Message::parse(&b), Err(Error::Duplicate)));
    }
}
