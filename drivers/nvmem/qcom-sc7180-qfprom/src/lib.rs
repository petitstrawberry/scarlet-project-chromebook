// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Read-only SC7180 QFPROM NVMEM provider.
//!
//! QFPROM exposes several address windows, but Linux permits a read-only OS
//! image to map only the corrected-fuse window.  This driver follows that
//! model: it maps resource zero, never attempts fuse programming, and leaves
//! the firmware-owned security-control clock and rail untouched.
//!
//! Child NVMEM cells are resolved by Scarlet from their device-tree `reg`
//! property.  Scarlet's cell interface hands the provider the byte range, so
//! this provider additionally records direct-child `bits = <offset width>`
//! declarations and packs an exact child-cell read into its low-order bits.
//! In particular, SC7180's `hstx-trim-primary@25b` cell (`bits = <1 3>`) is
//! returned as a value in the range 0..=7, as expected by the QUSB2 PHY.
//!
//! # Provenance
//!
//! The corrected-region selection, SC7180 keepouts, and read-only operating
//! model follow Linux `drivers/nvmem/qfprom.c` and
//! `Documentation/devicetree/bindings/nvmem/qcom,qfprom.yaml`.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::mmio,
    device::{
        fdt::FdtManager,
        manager::{DeviceManager, DriverPriority},
        nvmem::{NvmemError, NvmemProvider},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    println, vm,
};

const DRIVER_NAME: &str = "qcom-sc7180-qfprom";

// These ranges are inaccessible in SC7180's corrected-fuse window.  Linux
// registers them as NVMEM keepouts rather than attempting an unsafe read.
const SC7180_KEEPOUTS: &[(usize, usize)] = &[(0x128, 0x148), (0x220, 0x228)];

#[derive(Clone, Copy)]
struct BitField {
    offset: usize,
    size: usize,
    bit_offset: usize,
    bit_size: usize,
}

/// Read-only corrected-fuse window and its device-tree cell metadata.
pub struct Sc7180Qfprom {
    base: usize,
    size: usize,
    bitfields: Vec<BitField>,
}

impl Sc7180Qfprom {
    fn new(base: usize, size: usize, bitfields: Vec<BitField>) -> Self {
        Self {
            base,
            size,
            bitfields,
        }
    }

    fn range_is_readable(&self, offset: usize, end: usize) -> bool {
        SC7180_KEEPOUTS
            .iter()
            .all(|&(keepout_start, keepout_end)| end <= keepout_start || offset >= keepout_end)
    }

    fn bitfield_for_exact_read(&self, offset: usize, size: usize) -> Option<BitField> {
        self.bitfields
            .iter()
            .copied()
            .find(|field| field.offset == offset && field.size == size)
    }

    fn pack_bitfield(field: BitField, bytes: &mut [u8]) -> Result<(), NvmemError> {
        let last_bit = field
            .bit_offset
            .checked_add(field.bit_size)
            .ok_or(NvmemError::OutOfRange)?;
        if last_bit > bytes.len().saturating_mul(8) {
            return Err(NvmemError::OutOfRange);
        }

        let mut packed = vec![0u8; bytes.len()];
        for bit in 0..field.bit_size {
            let source = field.bit_offset + bit;
            if bytes[source / 8] & (1 << (source % 8)) != 0 {
                packed[bit / 8] |= 1 << (bit % 8);
            }
        }
        bytes.copy_from_slice(&packed);
        Ok(())
    }
}

impl NvmemProvider for Sc7180Qfprom {
    fn name(&self) -> &'static str {
        DRIVER_NAME
    }

    fn size(&self) -> usize {
        self.size
    }

    fn read(&self, offset: usize, bytes: &mut [u8]) -> Result<(), NvmemError> {
        let end = offset
            .checked_add(bytes.len())
            .ok_or(NvmemError::OutOfRange)?;
        if end > self.size || !self.range_is_readable(offset, end) {
            return Err(NvmemError::OutOfRange);
        }

        for (index, byte) in bytes.iter_mut().enumerate() {
            // SAFETY: `base` maps the corrected QFPROM region and bounds were
            // checked against that region above.
            *byte = unsafe { mmio::read8(self.base + offset + index) };
        }

        if let Some(field) = self.bitfield_for_exact_read(offset, bytes.len()) {
            Self::pack_bitfield(field, bytes)?;
        }
        Ok(())
    }

    fn write(&self, _offset: usize, _bytes: &[u8]) -> Result<(), NvmemError> {
        // Fuse programming is intentionally out of scope.  It requires raw,
        // configuration, and security windows plus an explicitly sequenced
        // rail and core clock, none of which are needed for USB calibration.
        Err(NvmemError::NotSupported)
    }
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn be_u32_pair(bytes: &[u8]) -> Option<(u32, u32)> {
    let first = be_u32(bytes)?;
    let second = be_u32(bytes.get(4..)?)?;
    Some((first, second))
}

fn node_phandle(node: &fdt::node::FdtNode<'_, '_>) -> Option<u32> {
    node.property("phandle")
        .or_else(|| node.property("linux,phandle"))
        .and_then(|property| be_u32(property.value))
}

fn direct_child_bitfields(provider_phandle: u32) -> Vec<BitField> {
    let Some(fdt) = FdtManager::get_manager().get_fdt() else {
        return Vec::new();
    };
    let Some(root) = fdt.find_node("/") else {
        return Vec::new();
    };

    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node_phandle(&node) == Some(provider_phandle) {
            return node
                .children()
                .filter_map(|child| {
                    let (offset, size) = be_u32_pair(child.property("reg")?.value)?;
                    let (bit_offset, bit_size) = be_u32_pair(child.property("bits")?.value)?;
                    let offset = usize::try_from(offset).ok()?;
                    let size = usize::try_from(size).ok()?;
                    let bit_offset = usize::try_from(bit_offset).ok()?;
                    let bit_size = usize::try_from(bit_size).ok()?;
                    if size == 0 || bit_size == 0 {
                        return None;
                    }
                    Some(BitField {
                        offset,
                        size,
                        bit_offset,
                        bit_size,
                    })
                })
                .collect();
        }

        for child in node.children() {
            stack.push(child);
        }
    }

    Vec::new()
}

fn device_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-sc7180-qfprom: missing phandle")
}

fn corrected_resource(device: &PlatformDeviceInfo) -> Result<(usize, usize), &'static str> {
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .ok_or("qcom-sc7180-qfprom: corrected fuse resource missing")?;
    let size = resource
        .end
        .checked_sub(resource.start)
        .and_then(|size| size.checked_add(1))
        .ok_or("qcom-sc7180-qfprom: invalid corrected fuse resource")?;
    Ok((resource.start, size))
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let (paddr, size) = corrected_resource(device)?;
    let base = vm::ioremap(paddr, size).map_err(|_| "qcom-sc7180-qfprom: ioremap failed")?;
    let phandle = device_phandle(device)?;
    let bitfields = direct_child_bitfields(phandle);

    let provider = Arc::new(Sc7180Qfprom::new(base, size, bitfields));
    DeviceManager::get_manager().register_nvmem_provider(phandle, provider);
    println!(
        "[{}] registered corrected fuse window paddr={:#x} size={:#x}; firmware security clock retained",
        DRIVER_NAME, paddr, size
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        DRIVER_NAME,
        probe_fn,
        remove_fn,
        vec!["qcom,sc7180-qfprom", "qcom,qfprom"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Critical);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_QFPROM_ANCHOR: fn() = force_link;

/// Force the linker to retain this driver crate.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::{BitField, Sc7180Qfprom};

    #[test_case]
    fn sc7180_hstx_trim_bits_are_packed() {
        let field = BitField {
            offset: 0x25b,
            size: 1,
            bit_offset: 1,
            bit_size: 3,
        };
        let mut bytes = [0b0011_1010];
        Sc7180Qfprom::pack_bitfield(field, &mut bytes).unwrap();
        assert_eq!(bytes, [0b101]);
    }
}
