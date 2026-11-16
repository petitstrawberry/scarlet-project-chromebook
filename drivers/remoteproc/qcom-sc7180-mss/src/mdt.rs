// SPDX-License-Identifier: GPL-2.0-only
//! Checked Qualcomm ELF32/MDT planning. MBA performs signature authentication;
//! host checks here bound every file copy, relocation and metadata allocation.
//! Wire layout follows Linux qcom_q6v5_mss.c and soc/qcom/mdt_loader.c.
use alloc::vec::Vec;
use core::ops::Range;

const HEADER: usize = 52;
const PHDR: usize = 32;
const HASH_MASK: u32 = 7 << 24;
const HASH: u32 = 2 << 24;
const RELOCATABLE: u32 = 1 << 27;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Header,
    Bounds,
    Metadata,
    Overlap,
    Address,
    NoSegments,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub source: Range<usize>,
    pub offset: usize,
    pub memory_size: usize,
}

#[derive(Debug)]
pub struct Image {
    pub metadata: Vec<u8>,
    pub segments: Vec<Segment>,
    pub boot_address: u32,
    pub code_length: u32,
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(Error::Bounds)?
            .try_into()
            .map_err(|_| Error::Bounds)?,
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(Error::Bounds)?
            .try_into()
            .map_err(|_| Error::Bounds)?,
    ))
}
fn file_range(offset: u32, size: u32, len: usize) -> Result<Range<usize>, Error> {
    if size == 0 {
        return Ok(0..0);
    }
    let start = offset as usize;
    let end = start
        .checked_add(size as usize)
        .filter(|end| *end <= len)
        .ok_or(Error::Bounds)?;
    Ok(start..end)
}
struct Program {
    kind: u32,
    source: Range<usize>,
    paddr: u32,
    memory_size: usize,
    flags: u32,
}

impl Image {
    /// Parse a complete (non-split) modem MBN. The pinned SC7180 non-LTE input
    /// is one file; missing split segments must never be treated as zero data.
    pub fn parse(bytes: &[u8], physical: u64, capacity: usize) -> Result<Self, Error> {
        if bytes.len() < HEADER
            || bytes.get(..7) != Some(b"\x7fELF\x01\x01\x01")
            || u16_at(bytes, 18)? != 164
            || u16_at(bytes, 40)? as usize != HEADER
            || u16_at(bytes, 42)? as usize != PHDR
        {
            return Err(Error::Header);
        }
        let count = u16_at(bytes, 44)? as usize;
        let table = u32_at(bytes, 28)? as usize;
        let table_end = table
            .checked_add(count.checked_mul(PHDR).ok_or(Error::Bounds)?)
            .ok_or(Error::Bounds)?;
        if !(2..=128).contains(&count)
            || table < HEADER
            || table % 4 != 0
            || table_end > bytes.len()
        {
            return Err(Error::Header);
        }
        let region_end = physical
            .checked_add(capacity as u64)
            .ok_or(Error::Address)?;
        if physical % 4096 != 0 || capacity == 0 || capacity % 4096 != 0 || region_end > 1u64 << 32
        {
            return Err(Error::Address);
        }
        let mut programs = Vec::with_capacity(count);
        for index in 0..count {
            let at = table + index * PHDR;
            let size = u32_at(bytes, at + 16)?;
            programs.push(Program {
                kind: u32_at(bytes, at)?,
                source: file_range(u32_at(bytes, at + 4)?, size, bytes.len())?,
                paddr: u32_at(bytes, at + 12)?,
                memory_size: u32_at(bytes, at + 20)? as usize,
                flags: u32_at(bytes, at + 24)?,
            });
        }
        let header = &programs[0];
        if header.kind == 1 || header.source.start != 0 || header.source.end < table_end {
            return Err(Error::Metadata);
        }
        let mut hashes = programs
            .iter()
            .skip(1)
            .filter(|program| program.flags & HASH_MASK == HASH);
        let hash = hashes.next().ok_or(Error::Metadata)?;
        if hashes.next().is_some() || hash.source.is_empty() {
            return Err(Error::Metadata);
        }
        let metadata_size = header
            .source
            .len()
            .checked_add(hash.source.len())
            .ok_or(Error::Bounds)?;
        if metadata_size > 16 * 1024 {
            return Err(Error::Metadata);
        }
        let mut metadata = Vec::with_capacity(metadata_size);
        metadata.extend_from_slice(&bytes[header.source.clone()]);
        metadata.extend_from_slice(&bytes[hash.source.clone()]);
        let loadable = |program: &&Program| {
            program.kind == 1 && program.flags & HASH_MASK != HASH && program.memory_size != 0
        };
        let min = programs
            .iter()
            .filter(loadable)
            .map(|program| program.paddr)
            .min()
            .ok_or(Error::NoSegments)?;
        let relocated = programs
            .iter()
            .filter(loadable)
            .any(|program| program.flags & RELOCATABLE != 0);
        let relocation = if relocated { u64::from(min) } else { physical };
        let boot_address = if relocated { physical } else { u64::from(min) };
        let mut code_length = 0u32;
        let mut segments: Vec<Segment> = Vec::new();
        for program in programs.iter().filter(loadable) {
            if program.source.len() > program.memory_size {
                return Err(Error::Bounds);
            }
            let offset = u64::from(program.paddr)
                .checked_sub(relocation)
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or(Error::Address)?;
            let end = offset
                .checked_add(program.memory_size)
                .filter(|end| *end <= capacity)
                .ok_or(Error::Address)?;
            if segments.iter().any(|previous| {
                offset < previous.offset + previous.memory_size && previous.offset < end
            }) {
                return Err(Error::Overlap);
            }
            code_length = code_length
                .checked_add(u32::try_from(program.memory_size).map_err(|_| Error::Bounds)?)
                .ok_or(Error::Bounds)?;
            segments.push(Segment {
                source: program.source.clone(),
                offset,
                memory_size: program.memory_size,
            });
        }
        Ok(Self {
            metadata,
            segments,
            boot_address: boot_address as u32,
            code_length,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;
    fn image() -> Vec<u8> {
        let mut bytes = vec![0; 4096];
        bytes[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        for (offset, value) in [(18, 164u16), (40, 52), (42, 32), (44, 3)] {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        bytes[28..32].copy_from_slice(&52u32.to_le_bytes());
        for (index, words) in [
            [0, 0, 0, 0, 148, 0, 7 << 24, 0],
            [0, 256, 0, 0, 64, 64, HASH, 0],
            [1, 512, 0, 0x86000000, 16, 4096, RELOCATABLE | 5, 4096],
        ]
        .iter()
        .enumerate()
        {
            for (word, value) in words.iter().enumerate() {
                let at = 52 + index * 32 + word * 4;
                bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }
    #[test]
    fn preserves_authentication_metadata_and_bss_plan() {
        let bytes = image();
        let plan = Image::parse(&bytes, 0x86000000, 0x2000000).unwrap();
        assert_eq!(plan.metadata.len(), 212);
        assert_eq!(&plan.metadata[..148], &bytes[..148]);
        assert_eq!(plan.segments[0].source, 512..528);
        assert_eq!(plan.segments[0].memory_size, 4096);
        assert_eq!(plan.code_length, 4096);
        assert_eq!(plan.boot_address, 0x86000000);
    }
    #[test]
    fn rejects_bad_metadata_and_device_bounds() {
        let mut bytes = image();
        bytes[52 + 32 + 24..52 + 32 + 28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes, 0x86000000, 0x2000000).unwrap_err(),
            Error::Metadata
        );
        assert_eq!(
            Image::parse(&image(), 0xfffff000, 8192).unwrap_err(),
            Error::Address
        );
        let mut bytes = image();
        bytes[52 + 64 + 20..52 + 64 + 24].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(
            Image::parse(&bytes, 0x86000000, 0x2000000).unwrap_err(),
            Error::Bounds
        );
    }
}
