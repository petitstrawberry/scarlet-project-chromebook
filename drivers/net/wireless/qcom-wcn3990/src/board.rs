// SPDX-License-Identifier: GPL-2.0-only
//! Checked ath10k board API 2 container selection; no cross-board guessing.
use alloc::format;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    NotFound,
    Duplicate,
}
fn next<'a>(tail: &mut &'a [u8]) -> Result<Option<(u32, &'a [u8])>, Error> {
    if tail.is_empty() {
        return Ok(None);
    }
    if tail.len() < 8 {
        return Err(Error::Malformed);
    }
    let id = u32::from_le_bytes(tail[..4].try_into().unwrap());
    let n = u32::from_le_bytes(tail[4..8].try_into().unwrap()) as usize;
    let padded = n.checked_add(3).ok_or(Error::Malformed)? & !3;
    if padded > tail.len() - 8 {
        return Err(Error::Malformed);
    }
    let value = &tail[8..8 + n];
    *tail = &tail[8 + padded..];
    Ok(Some((id, value)))
}
pub fn select<'a>(
    b: &'a [u8],
    chip: u32,
    board: u32,
    variant: Option<&str>,
) -> Result<&'a [u8], Error> {
    if b.len() < 20 || &b[..17] != b"QCA-ATH10K-BOARD\0" {
        return Err(Error::Malformed);
    }
    let exact = match variant {
        Some(v) => format!("bus=snoc,qmi-board-id={board:x},qmi-chip-id={chip:x},variant={v}"),
        None => format!("bus=snoc,qmi-board-id={board:x},qmi-chip-id={chip:x}"),
    };
    let fallback = format!("bus=snoc,qmi-board-id={board:x}");
    let mut preferred = None;
    let mut generic = None;
    let mut tail = &b[20..];
    while let Some((id, body)) = next(&mut tail)? {
        if id != 0 {
            continue;
        }
        let mut inner = body;
        let mut name = None;
        while let Some((id, value)) = next(&mut inner)? {
            match id {
                0 => {
                    name = Some(value);
                }
                1 => {
                    if value.is_empty() || value.len() > 65536 {
                        return Err(Error::Malformed);
                    }
                    let slot = if name == Some(exact.as_bytes()) {
                        Some(&mut preferred)
                    } else if name == Some(fallback.as_bytes()) {
                        Some(&mut generic)
                    } else {
                        None
                    };
                    if let Some(slot) = slot {
                        if slot.replace(value).is_some() {
                            return Err(Error::Duplicate);
                        }
                    }
                    name = None;
                }
                _ => {}
            }
        }
    }
    preferred.or(generic).ok_or(Error::NotFound)
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    fn ie(out: &mut Vec<u8>, id: u32, b: &[u8]) {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
        out.resize((out.len() + 3) & !3, 0);
    }
    #[test]
    fn exact_board_wins_and_bad_tail_is_rejected() {
        let mut b = b"QCA-ATH10K-BOARD\0\0\0\0".to_vec();
        for (name, data) in [
            ("bus=snoc,qmi-board-id=67", &[1u8][..]),
            ("bus=snoc,qmi-board-id=67,qmi-chip-id=320", &[2u8][..]),
        ] {
            let mut inner = Vec::new();
            ie(&mut inner, 0, name.as_bytes());
            ie(&mut inner, 1, data);
            ie(&mut b, 0, &inner);
        }
        assert_eq!(select(&b, 0x320, 0x67, None).unwrap(), &[2]);
        assert_eq!(select(&b, 0x320, 0xff, None), Err(Error::NotFound));
        b.pop();
        assert_eq!(select(&b, 0x320, 0x67, None), Err(Error::Malformed));
    }
}
