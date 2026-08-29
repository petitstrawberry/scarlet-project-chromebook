// SPDX-License-Identifier: GPL-2.0-only

//! Qualcomm MDT firmware loader for the no-TrustZone Venus path.
//!
//! The loader mirrors `qcom_mdt_load_no_init`: loadable non-hash ELF32
//! segments are relocated into the reserved Venus region and BSS tails are
//! cleared before the firmware context bank maps that region at IOVA zero.

use alloc::{vec, vec::Vec};
use core::ptr;

use scarlet::{fs::manager::get_global_vfs_manager_safe, object::KernelObject};

pub(crate) const VENUS_FIRMWARE_PATH: &str =
    "/system/scarlet/lib/firmware/qcom/venus-5.4/venus.mbn";

const ELF32_HEADER_SIZE: usize = 52;
const ELF32_PROGRAM_HEADER_SIZE: usize = 32;
const PT_LOAD: u32 = 1;
const QCOM_MDT_TYPE_MASK: u32 = 7 << 24;
const QCOM_MDT_TYPE_HASH: u32 = 2 << 24;
const QCOM_MDT_RELOCATABLE: u32 = 1 << 27;

#[derive(Clone, Copy)]
struct ProgramHeader {
    p_type: u32,
    offset: u32,
    paddr: u32,
    file_size: u32,
    mem_size: u32,
    flags: u32,
}

impl ProgramHeader {
    fn loadable(self) -> bool {
        self.p_type == PT_LOAD
            && self.flags & QCOM_MDT_TYPE_MASK != QCOM_MDT_TYPE_HASH
            && self.mem_size != 0
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}

fn load_file(path: &str, maximum_size: usize) -> Result<Vec<u8>, &'static str> {
    let vfs = get_global_vfs_manager_safe().ok_or("qcom-venus-sc7180: VFS is not ready")?;
    let object = vfs
        .open(path, 0)
        .map_err(|_| "qcom-venus-sc7180: firmware file is unavailable")?;
    let KernelObject::File(file) = object else {
        return Err("qcom-venus-sc7180: firmware path is not a file");
    };
    let size = file
        .metadata()
        .map_err(|_| "qcom-venus-sc7180: firmware metadata read failed")?
        .size;
    if size == 0 || size > maximum_size {
        return Err("qcom-venus-sc7180: firmware file size is invalid");
    }
    let mut bytes = vec![0; size];
    let mut offset = 0usize;
    while offset < size {
        let read = file
            .read(&mut bytes[offset..])
            .map_err(|_| "qcom-venus-sc7180: firmware read failed")?;
        if read == 0 {
            return Err("qcom-venus-sc7180: firmware ended early");
        }
        offset = offset
            .checked_add(read)
            .ok_or("qcom-venus-sc7180: firmware read offset overflows")?;
    }
    Ok(bytes)
}

fn program_headers(bytes: &[u8]) -> Result<Vec<ProgramHeader>, &'static str> {
    if bytes.len() < ELF32_HEADER_SIZE
        || bytes.get(0..4) != Some(b"\x7fELF")
        || bytes.get(4) != Some(&1)
        || bytes.get(5) != Some(&1)
    {
        return Err("qcom-venus-sc7180: firmware is not little-endian ELF32");
    }
    let phoff = read_u32(bytes, 28)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("qcom-venus-sc7180: malformed ELF program-header offset")?;
    let phentsize = read_u16(bytes, 42)
        .map(usize::from)
        .ok_or("qcom-venus-sc7180: malformed ELF program-header size")?;
    let phnum = read_u16(bytes, 44)
        .map(usize::from)
        .ok_or("qcom-venus-sc7180: malformed ELF program-header count")?;
    if phentsize != ELF32_PROGRAM_HEADER_SIZE || phnum == 0 {
        return Err("qcom-venus-sc7180: unsupported ELF program-header table");
    }
    let table_end = phoff
        .checked_add(
            phentsize
                .checked_mul(phnum)
                .ok_or("qcom-venus-sc7180: ELF program-header table overflows")?,
        )
        .ok_or("qcom-venus-sc7180: ELF program-header table overflows")?;
    if table_end > bytes.len() {
        return Err("qcom-venus-sc7180: truncated ELF program-header table");
    }

    let mut headers = Vec::with_capacity(phnum);
    for index in 0..phnum {
        let base = phoff + index * phentsize;
        headers.push(ProgramHeader {
            p_type: read_u32(bytes, base)
                .ok_or("qcom-venus-sc7180: truncated ELF program header")?,
            offset: read_u32(bytes, base + 4)
                .ok_or("qcom-venus-sc7180: truncated ELF program header")?,
            paddr: read_u32(bytes, base + 12)
                .ok_or("qcom-venus-sc7180: truncated ELF program header")?,
            file_size: read_u32(bytes, base + 16)
                .ok_or("qcom-venus-sc7180: truncated ELF program header")?,
            mem_size: read_u32(bytes, base + 20)
                .ok_or("qcom-venus-sc7180: truncated ELF program header")?,
            flags: read_u32(bytes, base + 24)
                .ok_or("qcom-venus-sc7180: truncated ELF program header")?,
        });
    }
    Ok(headers)
}

/// Load the packaged Venus MDT image into reserved physical RAM.
///
/// # Arguments
///
/// * `region_vaddr` - Normal-memory kernel mapping of the reserved region.
/// * `region_paddr` - Physical base of the reserved region.
/// * `region_size` - Byte length of the reserved region.
///
/// # Returns
///
/// Number of bytes spanned by loadable firmware segments.
pub(crate) fn load_into_reserved_region(
    region_vaddr: usize,
    region_paddr: usize,
    region_size: usize,
) -> Result<usize, &'static str> {
    let bytes = load_file(VENUS_FIRMWARE_PATH, region_size)?;
    let headers = program_headers(&bytes)?;
    let mut minimum = u32::MAX;
    let mut maximum = 0u32;
    let mut relocatable = false;
    for header in headers.iter().copied().filter(|header| header.loadable()) {
        minimum = minimum.min(header.paddr);
        maximum = maximum.max(
            header
                .paddr
                .checked_add(header.mem_size)
                .ok_or("qcom-venus-sc7180: firmware segment address overflows")?,
        );
        relocatable |= header.flags & QCOM_MDT_RELOCATABLE != 0;
    }
    if minimum >= maximum {
        return Err("qcom-venus-sc7180: firmware has no loadable segments");
    }
    let relocation_base = if relocatable {
        usize::try_from(minimum)
            .map_err(|_| "qcom-venus-sc7180: relocation base is out of range")?
    } else {
        region_paddr
    };
    let image_span = usize::try_from(maximum - minimum)
        .map_err(|_| "qcom-venus-sc7180: firmware span is out of range")?;
    if image_span > region_size {
        return Err("qcom-venus-sc7180: reserved firmware region is too small");
    }

    // SAFETY: the caller provides an exclusive normal-memory mapping for the
    // complete reserved Venus region. It is not part of the page allocator.
    unsafe { ptr::write_bytes(region_vaddr as *mut u8, 0, region_size) };

    for header in headers.into_iter().filter(|header| header.loadable()) {
        if header.file_size > header.mem_size {
            return Err("qcom-venus-sc7180: firmware segment file size exceeds memory size");
        }
        let paddr = usize::try_from(header.paddr)
            .map_err(|_| "qcom-venus-sc7180: firmware segment address is out of range")?;
        let destination_offset = paddr
            .checked_sub(relocation_base)
            .ok_or("qcom-venus-sc7180: firmware segment precedes relocation base")?;
        let mem_size = usize::try_from(header.mem_size)
            .map_err(|_| "qcom-venus-sc7180: firmware segment size is out of range")?;
        let file_size = usize::try_from(header.file_size)
            .map_err(|_| "qcom-venus-sc7180: firmware segment size is out of range")?;
        let destination_end = destination_offset
            .checked_add(mem_size)
            .ok_or("qcom-venus-sc7180: firmware destination range overflows")?;
        if destination_end > region_size {
            return Err("qcom-venus-sc7180: firmware segment is outside reserved memory");
        }
        let source_offset = usize::try_from(header.offset)
            .map_err(|_| "qcom-venus-sc7180: firmware file offset is out of range")?;
        let source_end = source_offset
            .checked_add(file_size)
            .ok_or("qcom-venus-sc7180: firmware source range overflows")?;
        let source = bytes
            .get(source_offset..source_end)
            .ok_or("qcom-venus-sc7180: split or truncated MDT image is unsupported")?;
        // SAFETY: source bounds were checked above and destination bounds are
        // contained by the exclusive reserved-memory mapping.
        unsafe {
            ptr::copy_nonoverlapping(
                source.as_ptr(),
                (region_vaddr + destination_offset) as *mut u8,
                file_size,
            )
        };
    }

    Ok(image_span)
}
