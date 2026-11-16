// SPDX-License-Identifier: GPL-2.0-only
//! Checked SMEM v12 metadata operations. Callers serialize these operations
//! with both a local lock and SMEM's remote hardware lock (normally TCSR 3).
//! Read/write methods access little-endian aligned words in noncached memory.

const HOSTS: usize = 20;
const GLOBAL: u16 = 0xfffe;
const TOC_SIZE: usize = 4096;
const TABLE_HEADER: usize = 32;
const TABLE_ENTRY: usize = 48;
const PART_HEADER: usize = 32;
const ITEM_HEADER: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Bounds,
    Uninitialized,
    UnsupportedVersion,
    Corrupt,
    MissingPartition,
    InvalidItem,
    NotFound,
    SizeMismatch,
    NoSpace,
}

/// Implementations must use volatile/noncached accesses, not ordinary slices
/// shared with another processor. The layout checks every access beforehand.
pub trait Memory {
    fn len(&self) -> usize;
    fn read32(&self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
    /// Publish data/header writes before publishing a new allocation frontier.
    fn publish(&self);
}

#[derive(Clone, Copy, Debug)]
struct Partition {
    base: usize,
    size: usize,
    cacheline: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub offset: usize,
    pub len: usize,
}

pub struct Layout {
    global: Partition,
    private: [Option<Partition>; HOSTS],
    item_count: u32,
    memory_size: usize,
}

fn range(offset: usize, len: usize, end: usize) -> Result<(), Error> {
    if offset.checked_add(len).is_some_and(|next| next <= end) {
        Ok(())
    } else {
        Err(Error::Bounds)
    }
}

fn word(memory: &impl Memory, offset: usize) -> Result<u32, Error> {
    range(offset, 4, memory.len())?;
    if offset % 4 != 0 {
        return Err(Error::Corrupt);
    }
    Ok(memory.read32(offset))
}

fn aligned(size: usize, alignment: usize) -> Result<usize, Error> {
    size.checked_add(alignment - 1)
        .map(|size| size & !(alignment - 1))
        .ok_or(Error::Bounds)
}

impl Layout {
    pub fn discover(memory: &impl Memory) -> Result<Self, Error> {
        if memory.len() < TOC_SIZE * 2 || memory.len() % 4096 != 0 {
            return Err(Error::Bounds);
        }
        if word(memory, 192)? != 1 || word(memory, 204)? != 0 {
            return Err(Error::Uninitialized);
        }
        if word(memory, 64 + 7 * 4)? >> 16 != 12 {
            return Err(Error::UnsupportedVersion);
        }
        let table = memory.len() - TOC_SIZE;
        if word(memory, table)? != u32::from_le_bytes(*b"$TOC") || word(memory, table + 4)? != 1 {
            return Err(Error::Corrupt);
        }
        let count = word(memory, table + 8)? as usize;
        if count > (TOC_SIZE - TABLE_HEADER) / TABLE_ENTRY {
            return Err(Error::Bounds);
        }
        let mut private = [None; HOSTS];
        let mut global = None;
        // Validate even non-AP entries: their regions must not overlap an AP
        // partition or the TOC. A corrupt peer partition is not free storage.
        for index in 0..count {
            let entry = table + TABLE_HEADER + index * TABLE_ENTRY;
            let base = word(memory, entry)? as usize;
            let size = word(memory, entry + 4)? as usize;
            let hosts = word(memory, entry + 12)?;
            if base == 0 && size == 0 {
                continue;
            }
            // SBL describes the leading metadata and trailing TOC pages as
            // reserved host ffff:ffff entries. These are not item partitions.
            if hosts == u32::MAX {
                if (base != 0 && base != table) || size != TOC_SIZE {
                    return Err(Error::Corrupt);
                }
                continue;
            }
            if base < TOC_SIZE || base % 8 != 0 || size < PART_HEADER || size % 8 != 0 {
                return Err(Error::Corrupt);
            }
            range(base, size, table)?;
            for previous in 0..index {
                let previous = table + TABLE_HEADER + previous * TABLE_ENTRY;
                let other_base = word(memory, previous)? as usize;
                let other_size = word(memory, previous + 4)? as usize;
                if other_size != 0 && base < other_base + other_size && other_base < base + size {
                    return Err(Error::Corrupt);
                }
            }
            let host0 = hosts as u16;
            let host1 = (hosts >> 16) as u16;
            if hosts != u32::from(GLOBAL) * 0x1_0001 && host0 != 0 && host1 != 0 {
                continue;
            }
            let cacheline = word(memory, entry + 16)? as usize;
            // Global partitions can legitimately have no cached allocator.
            if cacheline != 0 && (!(8..=4096).contains(&cacheline) || !cacheline.is_power_of_two())
            {
                return Err(Error::Corrupt);
            }
            if word(memory, base)? != u32::from_le_bytes(*b"$PRT")
                || word(memory, base + 4)? != hosts
                || word(memory, base + 8)? as usize != size
            {
                return Err(Error::Corrupt);
            }
            let partition = Partition {
                base,
                size,
                cacheline,
            };
            Self::frontiers(memory, partition)?;
            let destination = if host0 == GLOBAL && host1 == GLOBAL {
                &mut global
            } else {
                let remote = if host0 == 0 { host1 } else { host0 } as usize;
                private.get_mut(remote).ok_or(Error::Corrupt)?
            };
            if destination.replace(partition).is_some() {
                return Err(Error::Corrupt);
            }
        }
        let info = table + TABLE_HEADER + count * TABLE_ENTRY;
        let item_count =
            if info + 20 <= memory.len() && word(memory, info)? == u32::from_le_bytes(*b"SIII") {
                word(memory, info + 16)? & 0xffff
            } else {
                512
            };
        if item_count < 8 {
            return Err(Error::Corrupt);
        }
        Ok(Self {
            global: global.ok_or(Error::MissingPartition)?,
            private,
            item_count,
            memory_size: memory.len(),
        })
    }

    pub fn has_private_partition(&self, host: u16) -> bool {
        self.private.get(host as usize).is_some_and(Option::is_some)
    }

    fn partition(&self, host: u16, item: u16, memory: &impl Memory) -> Result<Partition, Error> {
        if memory.len() != self.memory_size {
            return Err(Error::Bounds);
        }
        if u32::from(item) >= self.item_count {
            return Err(Error::InvalidItem);
        }
        // HOST_ANY is explicitly global. Unknown host IDs are rejected rather
        // than silently disclosing the global partition to a typoed peer.
        if host == u16::MAX {
            return Ok(self.global);
        }
        Ok(self
            .private
            .get(host as usize)
            .ok_or(Error::InvalidItem)?
            .unwrap_or(self.global))
    }

    fn frontiers(memory: &impl Memory, part: Partition) -> Result<(usize, usize), Error> {
        let uncached = word(memory, part.base + 12)? as usize;
        let cached = word(memory, part.base + 16)? as usize;
        if uncached < PART_HEADER
            || uncached % 8 != 0
            || uncached > cached
            || cached > part.size
            || cached % 8 != 0
            || (part.cacheline == 0 && cached != part.size)
        {
            return Err(Error::Corrupt);
        }
        Ok((uncached, cached))
    }

    fn entry(
        memory: &impl Memory,
        part: Partition,
        offset: usize,
    ) -> Result<(u16, usize, usize, usize), Error> {
        range(offset, ITEM_HEADER, part.size)?;
        let at = part.base + offset;
        let tag = word(memory, at)?;
        if tag as u16 != 0xa5a5 {
            return Err(Error::Corrupt);
        }
        let size = word(memory, at + 4)? as usize;
        let padding = word(memory, at + 8)?;
        let data_padding = (padding & 0xffff) as usize;
        let header_padding = (padding >> 16) as usize;
        if data_padding > size || size % 8 != 0 {
            return Err(Error::Corrupt);
        }
        Ok(((tag >> 16) as u16, size, data_padding, header_padding))
    }

    pub fn get(&self, memory: &impl Memory, host: u16, item: u16) -> Result<Region, Error> {
        let part = self.partition(host, item, memory)?;
        let (uncached, cached) = Self::frontiers(memory, part)?;
        let mut offset = PART_HEADER;
        while offset < uncached {
            let (id, size, padding, header_padding) = Self::entry(memory, part, offset)?;
            let data = offset
                .checked_add(ITEM_HEADER + header_padding)
                .ok_or(Error::Bounds)?;
            range(data, size, uncached)?;
            if data % 8 != 0 {
                return Err(Error::Corrupt);
            }
            if id == item {
                return Ok(Region {
                    offset: part.base + data,
                    len: size - padding,
                });
            }
            offset = data + size;
        }
        if cached == part.size {
            return Err(Error::NotFound);
        }
        let header_size = aligned(ITEM_HEADER, part.cacheline)?;
        let mut cursor = part.size;
        while cursor > cached {
            let offset = cursor.checked_sub(header_size).ok_or(Error::Corrupt)?;
            if offset < cached {
                return Err(Error::Corrupt);
            }
            let (id, size, padding, _) = Self::entry(memory, part, offset)?;
            let data = offset
                .checked_sub(size)
                .filter(|data| *data >= cached)
                .ok_or(Error::Corrupt)?;
            if id == item {
                return Ok(Region {
                    offset: part.base + data,
                    len: size - padding,
                });
            }
            cursor = data;
        }
        Err(Error::NotFound)
    }

    /// Return an existing exact-size item, or zero and publish a new uncached
    /// allocation. Never reclaim or resize memory potentially owned by firmware.
    pub fn allocate(
        &self,
        memory: &mut impl Memory,
        host: u16,
        item: u16,
        len: usize,
    ) -> Result<Region, Error> {
        if item < 8 || len == 0 || len > u32::MAX as usize - 7 {
            return Err(Error::InvalidItem);
        }
        match self.get(memory, host, item) {
            Ok(region) if region.len == len => return Ok(region),
            Ok(_) => return Err(Error::SizeMismatch),
            Err(Error::NotFound) => {}
            Err(error) => return Err(error),
        }
        let part = self.partition(host, item, memory)?;
        let (uncached, cached) = Self::frontiers(memory, part)?;
        let size = aligned(len, 8)?;
        let next = uncached
            .checked_add(ITEM_HEADER)
            .and_then(|next| next.checked_add(size))
            .ok_or(Error::Bounds)?;
        if next > cached {
            return Err(Error::NoSpace);
        }
        let header = part.base + uncached;
        for offset in (header..part.base + next).step_by(4) {
            memory.write32(offset, 0);
        }
        memory.write32(header, (u32::from(item) << 16) | 0xa5a5);
        memory.write32(header + 4, size as u32);
        memory.write32(header + 8, (size - len) as u32);
        memory.publish();
        memory.write32(part.base + 12, next as u32);
        memory.publish();
        Ok(Region {
            offset: header + ITEM_HEADER,
            len,
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{vec, vec::Vec};

    struct Ram {
        words: Vec<u32>,
        published: core::cell::Cell<usize>,
        writes: usize,
    }
    impl Memory for Ram {
        fn len(&self) -> usize {
            self.words.len() * 4
        }
        fn read32(&self, offset: usize) -> u32 {
            assert_eq!(offset % 4, 0);
            self.words[offset / 4]
        }
        fn write32(&mut self, offset: usize, value: u32) {
            assert_eq!(offset % 4, 0);
            self.words[offset / 4] = value;
            self.writes += 1;
            // Publishing a frontier must follow a complete initialized entry.
            if offset == 4096 + 12 || offset == 8192 + 12 {
                let base = offset - 12;
                let end = value as usize;
                if end > PART_HEADER {
                    assert_eq!(self.published.get(), self.writes - 1);
                    assert_eq!(self.read32(base + PART_HEADER) & 0xffff, 0xa5a5);
                }
            }
        }
        fn publish(&self) {
            self.published.set(self.writes);
        }
    }
    fn fixture() -> Ram {
        let mut ram = Ram {
            words: vec![0; 16384 / 4],
            published: core::cell::Cell::new(0),
            writes: 0,
        };
        ram.words[192 / 4] = 1;
        ram.words[92 / 4] = 12 << 16;
        let table = 12288 / 4;
        ram.words[table] = u32::from_le_bytes(*b"$TOC");
        ram.words[table + 1] = 1;
        ram.words[table + 2] = 4;
        for (index, hosts) in [0xfffe_fffe, 1 << 16].into_iter().enumerate() {
            let base = 4096 * (index + 1);
            let entry = table + (TABLE_HEADER + index * TABLE_ENTRY) / 4;
            ram.words[entry] = base as u32;
            ram.words[entry + 1] = 4096;
            ram.words[entry + 3] = hosts;
            ram.words[entry + 4] = 64;
            ram.words[base / 4] = u32::from_le_bytes(*b"$PRT");
            ram.words[base / 4 + 1] = hosts;
            ram.words[base / 4 + 2] = 4096;
            ram.words[base / 4 + 3] = 32;
            ram.words[base / 4 + 4] = 4096;
        }
        for (index, offset) in [(2, 0), (3, 12288)] {
            let entry = table + (TABLE_HEADER + index * TABLE_ENTRY) / 4;
            ram.words[entry] = offset;
            ram.words[entry + 1] = 4096;
            ram.words[entry + 3] = u32::MAX;
        }
        ram
    }
    #[test]
    fn allocation_preserves_existing_and_isolates_peer_partition() {
        let mut ram = fixture();
        let layout = Layout::discover(&ram).unwrap();
        let private = layout.allocate(&mut ram, 1, 435, 21).unwrap();
        assert_eq!(
            private,
            Region {
                offset: 8192 + 48,
                len: 21
            }
        );
        ram.write32(private.offset, 0x7654_3210);
        let writes = ram.writes;
        assert_eq!(layout.allocate(&mut ram, 1, 435, 21), Ok(private));
        assert_eq!(ram.writes, writes);
        assert_eq!(ram.read32(private.offset), 0x7654_3210);
        assert_eq!(
            layout.allocate(&mut ram, 1, 435, 20),
            Err(Error::SizeMismatch)
        );
        assert_eq!(layout.get(&ram, u16::MAX, 435), Err(Error::NotFound));
        let global = layout.allocate(&mut ram, u16::MAX, 435, 21).unwrap();
        assert_ne!(global.offset, private.offset);
        assert_eq!(layout.get(&ram, 2, 435), Ok(global));
    }
    #[test]
    fn rejects_corrupt_metadata_without_writes() {
        for (offset, value) in [
            (12288 + 8, u32::MAX),
            (12288 + 32 + 4, u32::MAX),
            (12288 + 32 + 48, 4096),
            (8192 + 16, 24),
            (8192 + 12, 8192),
        ] {
            let mut ram = fixture();
            ram.words[offset / 4] = value;
            assert!(Layout::discover(&ram).is_err());
            assert_eq!(ram.writes, 0);
        }
    }
    #[test]
    fn rejects_truncated_entries_and_exhaustion() {
        let mut ram = fixture();
        let layout = Layout::discover(&ram).unwrap();
        assert_eq!(
            layout.allocate(&mut ram, 1, 435, 4048),
            Ok(Region {
                offset: 8240,
                len: 4048
            })
        );
        let writes = ram.writes;
        assert_eq!(layout.allocate(&mut ram, 1, 428, 1), Err(Error::NoSpace));
        assert_eq!(ram.writes, writes);
        ram.words[(8192 + 32 + 4) / 4] = u32::MAX - 7;
        assert_eq!(layout.get(&ram, 1, 435), Err(Error::Bounds));
    }
    #[test]
    fn reads_cached_items_with_header_after_payload() {
        let mut ram = fixture();
        let base = 8192;
        let data = base + 4096 - 64 - 64;
        ram.words[(base + 16) / 4] = 4096 - 128;
        ram.words[(base + 4096 - 64) / 4] = (428 << 16) | 0xa5a5;
        ram.words[(base + 4096 - 64 + 4) / 4] = 64;
        ram.words[(base + 4096 - 64 + 8) / 4] = 4;
        let layout = Layout::discover(&ram).unwrap();
        assert_eq!(
            layout.get(&ram, 1, 428),
            Ok(Region {
                offset: data,
                len: 60
            })
        );
        ram.words[(base + 4096 - 64 + 4) / 4] = 8192;
        assert_eq!(layout.get(&ram, 1, 428), Err(Error::Corrupt));
    }
}
