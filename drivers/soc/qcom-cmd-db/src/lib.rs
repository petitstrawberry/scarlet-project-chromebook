// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Read-only Qualcomm AOP Command DB access for Chromebook platform drivers.
//!
//! The database lives in firmware-owned reserved RAM.  It is intentionally
//! mapped through Scarlet's device-memory `ioremap` path: old CoachZ firmware
//! can leave the region write protected, so a write-back alias must never be
//! created merely to inspect its little-endian tables.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::mmio,
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    early_println,
    sync::IrqSpinLock,
    vm,
};

const MAGIC: [u8; 4] = [0xdb, 0x30, 0x03, 0x0c];
const MAX_SLAVES: usize = 8;
const COMMAND_DB_HEADER_SIZE: usize = 4 + 4 + MAX_SLAVES * RESOURCE_HEADER_SIZE + 4 + 4;
const RESOURCE_HEADER_SIZE: usize = 16;
const ENTRY_HEADER_SIZE: usize = 24;
const ENTRY_ID_SIZE: usize = 8;

#[derive(Clone, Copy)]
struct ResourceHeader {
    header_offset: usize,
    data_offset: usize,
    count: usize,
}

#[derive(Clone, Copy)]
struct EntryHeader {
    address: u32,
    data_length: usize,
    data_offset: usize,
}

struct CommandDb {
    base: usize,
    size: usize,
}

impl CommandDb {
    fn checked_add(&self, offset: usize, len: usize) -> Option<usize> {
        offset.checked_add(len).filter(|end| *end <= self.size)
    }

    fn checked_offset(&self, offset: usize, len: usize) -> Option<usize> {
        self.checked_add(offset, len)
            .and_then(|_| self.base.checked_add(offset))
    }

    fn read_u8(&self, offset: usize) -> Option<u8> {
        let address = self.checked_offset(offset, 1)?;
        // SAFETY: `address` is inside the complete read-only ioremap mapping.
        Some(unsafe { mmio::read8(address) })
    }

    fn read_u16(&self, offset: usize) -> Option<u16> {
        let next = offset.checked_add(1)?;
        Some(u16::from_le_bytes([
            self.read_u8(offset)?,
            self.read_u8(next)?,
        ]))
    }

    fn read_u32(&self, offset: usize) -> Option<u32> {
        let one = offset.checked_add(1)?;
        let two = offset.checked_add(2)?;
        let three = offset.checked_add(3)?;
        Some(u32::from_le_bytes([
            self.read_u8(offset)?,
            self.read_u8(one)?,
            self.read_u8(two)?,
            self.read_u8(three)?,
        ]))
    }

    fn resource_header(&self, index: usize) -> Option<ResourceHeader> {
        if index >= MAX_SLAVES {
            return None;
        }
        let offset = 8 + index * RESOURCE_HEADER_SIZE;
        let slave_id = self.read_u16(offset)?;
        if slave_id == 0 {
            return None;
        }
        Some(ResourceHeader {
            header_offset: usize::from(self.read_u16(offset + 2)?),
            data_offset: usize::from(self.read_u16(offset + 4)?),
            count: usize::from(self.read_u16(offset + 6)?),
        })
    }

    fn id_matches(&self, offset: usize, query: &[u8]) -> bool {
        if query.len() > ENTRY_ID_SIZE {
            return false;
        }
        (0..ENTRY_ID_SIZE).all(|index| {
            let expected = query.get(index).copied().unwrap_or(0);
            offset
                .checked_add(index)
                .and_then(|entry_offset| self.read_u8(entry_offset))
                == Some(expected)
        })
    }

    fn find(&self, id: &str) -> Option<EntryHeader> {
        let query = id.as_bytes();
        if query.is_empty() || query.len() > ENTRY_ID_SIZE {
            return None;
        }
        for resource_index in 0..MAX_SLAVES {
            let Some(resource) = self.resource_header(resource_index) else {
                break;
            };
            let entries_base = self.checked_add(COMMAND_DB_HEADER_SIZE, resource.header_offset)?;
            for entry_index in 0..resource.count {
                let entry_offset =
                    entries_base.checked_add(entry_index.checked_mul(ENTRY_HEADER_SIZE)?)?;
                self.checked_offset(entry_offset, ENTRY_HEADER_SIZE)?;
                if !self.id_matches(entry_offset, query) {
                    continue;
                }
                let data_length_offset = entry_offset.checked_add(20)?;
                let entry_data_offset = entry_offset.checked_add(22)?;
                let data_length = usize::from(self.read_u16(data_length_offset)?);
                let data_offset = self
                    .checked_add(COMMAND_DB_HEADER_SIZE, resource.data_offset)?
                    .checked_add(usize::from(self.read_u16(entry_data_offset)?))?;
                self.checked_offset(data_offset, data_length)?;
                return Some(EntryHeader {
                    address: self.read_u32(entry_offset + 16)?,
                    data_length,
                    data_offset,
                });
            }
        }
        None
    }

    fn has_valid_magic(&self) -> bool {
        MAGIC
            .iter()
            .enumerate()
            .all(|(index, byte)| self.read_u8(4 + index) == Some(*byte))
    }
}

impl Drop for CommandDb {
    fn drop(&mut self) {
        vm::iounmap(self.base);
    }
}

static COMMAND_DB: IrqSpinLock<Option<Arc<CommandDb>>> = IrqSpinLock::new(None);

/// Return the RPMh resource address associated with a Command DB identifier.
///
/// # Arguments
///
/// * `id` - At most eight ASCII bytes, such as `"gfx.lvl"`.
///
/// # Returns
///
/// The little-endian firmware address, or `None` before probe/when absent.
pub fn read_address(id: &str) -> Option<u32> {
    let database = COMMAND_DB.lock().as_ref()?.clone();
    database.find(id).map(|entry| entry.address)
}

/// Copy the opaque auxiliary payload associated with a Command DB resource.
///
/// Command DB clients define the layout of this payload.  ARC resources use
/// an array of little-endian `u16` levels, while RPMh interconnect BCMs use a
/// packed eight-byte descriptor.  Returning an owned copy keeps the read-only
/// device mapping and its lock out of the caller's lifetime.
pub fn read_aux_data(id: &str) -> Option<Vec<u8>> {
    let database = COMMAND_DB.lock().as_ref()?.clone();
    let entry = database.find(id)?;
    let mut bytes = Vec::with_capacity(entry.data_length);
    for offset in 0..entry.data_length {
        bytes.push(database.read_u8(entry.data_offset.checked_add(offset)?)?);
    }
    Some(bytes)
}

/// Copy a Command DB auxiliary table interpreted as little-endian `u16`s.
///
/// # Arguments
///
/// * `id` - At most eight ASCII bytes identifying the resource.
///
/// # Returns
///
/// A private copy of the table, or `None` when the table is absent/malformed.
pub fn read_aux_u16(id: &str) -> Option<Vec<u16>> {
    let database = COMMAND_DB.lock().as_ref()?.clone();
    let entry = database.find(id)?;
    if entry.data_length == 0 || entry.data_length % 2 != 0 {
        return None;
    }
    let mut values = Vec::with_capacity(entry.data_length / 2);
    for offset in (0..entry.data_length).step_by(2) {
        values.push(database.read_u16(entry.data_offset.checked_add(offset)?)?);
    }
    Some(values)
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-cmd-db: missing reserved-memory resource")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|value| value.checked_add(1))
        .ok_or("qcom-cmd-db: invalid reserved-memory resource")?;
    if size < COMMAND_DB_HEADER_SIZE {
        return Err("qcom-cmd-db: reserved-memory resource is too small");
    }
    let base = vm::ioremap(resource.start, size).map_err(|_| "qcom-cmd-db: ioremap failed")?;
    let database = Arc::new(CommandDb { base, size });
    if !database.has_valid_magic() {
        return Err("qcom-cmd-db: invalid database magic");
    }
    let previous = {
        let mut guard = COMMAND_DB.lock();
        core::mem::replace(&mut *guard, Some(database))
    };
    drop(previous);
    early_println!(
        "[qcom-cmd-db] registered read-only database paddr={:#x} size={:#x}",
        resource.start,
        size,
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let previous = {
        let mut guard = COMMAND_DB.lock();
        guard.take()
    };
    drop(previous);
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-cmd-db",
            probe,
            remove,
            vec!["qcom,cmd-db"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_CMD_DB_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
