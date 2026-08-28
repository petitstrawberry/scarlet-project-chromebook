// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 operating-point discovery for the legacy A618 GMU.
//!
//! Linux builds the GMU performance table from the GPU and GMU OPP tables,
//! prepends the mandatory off level, and filters GPU OPPs with the QFPROM
//! speed-bin. Keep that firmware contract in one place so the HFI transport
//! never has to guess frequencies or voltage levels.

use alloc::{vec, vec::Vec};

use fdt::node::FdtNode;
use scarlet::{
    device::{fdt::FdtManager, manager::DeviceManager, platform::PlatformDeviceInfo},
    early_println,
};

use crate::hfi_abi::{MAX_GMU_LEVELS, MAX_GPU_LEVELS};

const A618_SPEED_BIN_DEFAULT_MASK: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OperatingPoint {
    pub(crate) frequency_khz: u32,
    pub(crate) level: u16,
    pub(crate) peak_kbps: Option<u32>,
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn node_phandle(node: &FdtNode<'_, '_>) -> Option<u32> {
    node.property("phandle")
        .or_else(|| node.property("linux,phandle"))
        .and_then(|property| be_u32(property.value))
}

fn find_node_by_phandle<'a>(fdt: &'a fdt::Fdt<'a>, phandle: u32) -> Option<FdtNode<'a, 'a>> {
    let mut stack = vec![fdt.find_node("/")?];
    while let Some(node) = stack.pop() {
        if node_phandle(&node) == Some(phandle) {
            return Some(node);
        }
        stack.extend(node.children());
    }
    None
}

fn node_is_available(node: &FdtNode<'_, '_>) -> bool {
    node.property("status").is_none_or(|property| {
        let status = property
            .value
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or(&[]);
        matches!(status, b"ok" | b"okay")
    })
}

fn operating_point_from_node(
    node: &FdtNode<'_, '_>,
    supported_hardware: u32,
) -> Result<Option<OperatingPoint>, &'static str> {
    if !node_is_available(node) {
        return Ok(None);
    }
    if let Some(property) = node.property("opp-supported-hw") {
        let mask = be_u32(property.value)
            .ok_or("qcom-adreno-a618: malformed OPP supported-hardware mask")?;
        if mask & supported_hardware == 0 {
            return Ok(None);
        }
    }

    let frequency_hz = node
        .property("opp-hz")
        .and_then(|property| be_u64(property.value))
        .ok_or("qcom-adreno-a618: OPP is missing a 64-bit frequency")?;
    if frequency_hz == 0 || frequency_hz % 1_000 != 0 {
        return Err("qcom-adreno-a618: OPP frequency is not an integral kHz value");
    }
    let frequency_khz = u32::try_from(frequency_hz / 1_000)
        .map_err(|_| "qcom-adreno-a618: OPP frequency exceeds HFI v1 range")?;
    let level = node
        .property("opp-level")
        .and_then(|property| be_u32(property.value))
        .and_then(|level| u16::try_from(level).ok())
        .ok_or("qcom-adreno-a618: OPP is missing a valid RPMh level")?;
    if level == 0 {
        return Err("qcom-adreno-a618: non-zero OPP has an off RPMh level");
    }
    let peak_kbps = node
        .property("opp-peak-kBps")
        .map(|property| {
            be_u32(property.value).ok_or("qcom-adreno-a618: malformed OPP peak bandwidth")
        })
        .transpose()?;

    Ok(Some(OperatingPoint {
        frequency_khz,
        level,
        peak_kbps,
    }))
}

fn finalize_operating_points(
    mut points: Vec<OperatingPoint>,
    max_levels_including_off: usize,
) -> Result<Vec<OperatingPoint>, &'static str> {
    if points.is_empty() {
        return Err("qcom-adreno-a618: no supported operating points");
    }
    if points.len() >= max_levels_including_off {
        return Err("qcom-adreno-a618: OPP table exceeds HFI v1 capacity");
    }
    points.sort_unstable_by_key(|point| point.frequency_khz);
    for pair in points.windows(2) {
        if pair[0].frequency_khz == pair[1].frequency_khz {
            return Err("qcom-adreno-a618: OPP table has duplicate frequencies");
        }
        if pair[0].level > pair[1].level {
            return Err("qcom-adreno-a618: OPP voltage level decreases with frequency");
        }
    }
    Ok(points)
}

fn read_operating_points(
    device: &PlatformDeviceInfo,
    supported_hardware: u32,
    max_levels_including_off: usize,
) -> Result<Vec<OperatingPoint>, &'static str> {
    let table_phandle = device
        .property("operating-points-v2")
        .and_then(|property| be_u32(property.value()))
        .filter(|phandle| *phandle != 0)
        .ok_or("qcom-adreno-a618: device is missing its OPP table")?;
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("qcom-adreno-a618: FDT is unavailable while parsing OPPs")?;
    let table = find_node_by_phandle(fdt, table_phandle)
        .ok_or("qcom-adreno-a618: OPP table phandle was not found")?;
    let mut points = Vec::new();
    for child in table.children() {
        if let Some(point) = operating_point_from_node(&child, supported_hardware)? {
            points.push(point);
        }
    }
    finalize_operating_points(points, max_levels_including_off)
}

/// Map the SC7180 A618 QFPROM value to the `opp-supported-hw` bit used by Linux.
fn supported_hardware_from_fuse(fuse: u32) -> (u32, bool) {
    match fuse {
        0 => (1 << 0, true),
        169 => (1 << 1, true),
        174 => (1 << 2, true),
        _ => (A618_SPEED_BIN_DEFAULT_MASK, false),
    }
}

fn read_gpu_supported_hardware(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    if device.property("nvmem-cells").is_none() {
        return Ok(A618_SPEED_BIN_DEFAULT_MASK);
    }
    let cell = DeviceManager::get_manager().resolve_nvmem_cell(device, "speed_bin")?;
    if cell.size() == 0 || cell.size() > core::mem::size_of::<u32>() {
        return Err("qcom-adreno-a618: GPU speed-bin cell has an invalid size");
    }
    let mut bytes = [0u8; core::mem::size_of::<u32>()];
    cell.read(&mut bytes[..cell.size()])
        .map_err(|_| "qcom-adreno-a618: failed to read GPU speed-bin fuse")?;
    let fuse = u32::from_le_bytes(bytes);
    let (supported_hardware, known) = supported_hardware_from_fuse(fuse);
    if known {
        early_println!(
            "[qcom-adreno-a618] speed-bin fuse={} supported-hw={:#x}",
            fuse,
            supported_hardware,
        );
    } else {
        early_println!(
            "[qcom-adreno-a618] unknown speed-bin fuse={}; using safe bin 0",
            fuse,
        );
    }
    Ok(supported_hardware)
}

pub(crate) fn read_gmu_operating_points(
    device: &PlatformDeviceInfo,
) -> Result<Vec<OperatingPoint>, &'static str> {
    read_operating_points(device, u32::MAX, MAX_GMU_LEVELS)
}

pub(crate) fn read_gpu_operating_points(
    device: &PlatformDeviceInfo,
) -> Result<Vec<OperatingPoint>, &'static str> {
    let supported_hardware = read_gpu_supported_hardware(device)?;
    read_operating_points(device, supported_hardware, MAX_GPU_LEVELS)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn point(frequency_khz: u32, level: u16) -> OperatingPoint {
        OperatingPoint {
            frequency_khz,
            level,
            peak_kbps: None,
        }
    }

    #[test]
    fn a618_speed_bin_mapping_matches_linux() {
        assert_eq!(supported_hardware_from_fuse(0), (1, true));
        assert_eq!(supported_hardware_from_fuse(169), (2, true));
        assert_eq!(supported_hardware_from_fuse(174), (4, true));
        assert_eq!(supported_hardware_from_fuse(0xffff), (1, false));
    }

    #[test]
    fn operating_points_are_sorted_for_hfi_indices() {
        let points = finalize_operating_points(
            std::vec![point(800_000, 384), point(180_000, 48), point(430_000, 192)],
            MAX_GPU_LEVELS,
        )
        .unwrap();
        assert_eq!(
            points
                .iter()
                .map(|point| point.frequency_khz)
                .collect::<std::vec::Vec<_>>(),
            std::vec![180_000, 430_000, 800_000]
        );
    }

    #[test]
    fn unsafe_or_ambiguous_opp_tables_are_rejected() {
        assert_eq!(
            finalize_operating_points(std::vec![point(180_000, 48), point(180_000, 64)], 4),
            Err("qcom-adreno-a618: OPP table has duplicate frequencies")
        );
        assert_eq!(
            finalize_operating_points(std::vec![point(180_000, 64), point(267_000, 48)], 4),
            Err("qcom-adreno-a618: OPP voltage level decreases with frequency")
        );
        assert_eq!(
            finalize_operating_points(std::vec![point(180_000, 48); 4], 4),
            Err("qcom-adreno-a618: OPP table exceeds HFI v1 capacity")
        );
    }
}
