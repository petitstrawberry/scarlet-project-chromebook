// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Read-only Qualcomm PMIC Arbiter v5 transport for SC7180.
//!
//! # Provenance
//!
//! The channel layout, APID mapping, command encoding, and completion status
//! follow Linux `drivers/spmi/spmi-pmic-arb.c`. This deliberately implements
//! only the v5 observer path needed to read the CoachZ PM6150 RTC.

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
    time, vm,
};

const PMIC_ARB_VERSION: usize = 0x0000;
const PMIC_ARB_FEATURES: usize = 0x0004;
const PMIC_ARB_VERSION_V5_MIN: u32 = 0x5000_0000;
const PMIC_ARB_VERSION_V7_MIN: u32 = 0x7000_0000;
const PMIC_ARB_FEATURES_PERIPH_MASK: u32 = 0x7ff;

const APID_MAP_BASE: usize = 0x0900;
const APID_OWNER_BASE: usize = 0x0700;
const APID_MAP_STRIDE: usize = 4;
const MAX_APIDS: usize = 512;
const MAX_PPIDS: usize = 4096;
const APID_INVALID: u16 = u16::MAX;

const CHANNEL_STRIDE: usize = 0x80;
const EE_OBSERVER_STRIDE: usize = 0x1_0000;
const CHANNEL_CMD: usize = 0x00;
const CHANNEL_STATUS: usize = 0x08;
const CHANNEL_RDATA0: usize = 0x18;
const CHANNEL_RDATA1: usize = 0x1c;

const STATUS_DONE: u32 = 1 << 0;
const STATUS_FAILURE: u32 = 1 << 1;
const STATUS_DENIED: u32 = 1 << 2;
const STATUS_DROPPED: u32 = 1 << 3;
const STATUS_ERROR: u32 = STATUS_FAILURE | STATUS_DENIED | STATUS_DROPPED;
const OP_EXT_READ_LONG: u32 = 1;
const MAX_TRANSFER: usize = 8;
const TRANSACTION_TIMEOUT_US: u64 = 1_000;

/// SC7180 PMIC Arbiter observer transport.
pub struct QcomSpmiPmicArb {
    observer_base: usize,
    ee: u8,
    ppid_to_apid: Vec<u16>,
    transaction_lock: IrqSpinLock<()>,
}

impl QcomSpmiPmicArb {
    fn observer_offset(&self, sid: u8, address: u16) -> Result<usize, &'static str> {
        if sid > 0xf {
            return Err("qcom-spmi-pmic-arb: invalid slave ID");
        }

        let ppid = ((sid as usize) << 8) | ((address as usize) >> 8);
        let apid = *self
            .ppid_to_apid
            .get(ppid)
            .ok_or("qcom-spmi-pmic-arb: PPID out of range")?;
        if apid == APID_INVALID {
            return Err("qcom-spmi-pmic-arb: no APID for peripheral");
        }

        Ok(EE_OBSERVER_STRIDE * self.ee as usize + CHANNEL_STRIDE * apid as usize)
    }

    fn wait_for_done(&self, channel: usize) -> Result<(), &'static str> {
        for _ in 0..TRANSACTION_TIMEOUT_US {
            // SAFETY: `observer_base` is the mapped PMIC arbiter observer window.
            let status = unsafe { mmio::read32(self.observer_base + channel + CHANNEL_STATUS) };
            if status & STATUS_DONE != 0 {
                return if status & STATUS_ERROR == 0 {
                    Ok(())
                } else if status & STATUS_DENIED != 0 {
                    Err("qcom-spmi-pmic-arb: transaction denied")
                } else if status & STATUS_DROPPED != 0 {
                    Err("qcom-spmi-pmic-arb: transaction dropped")
                } else {
                    Err("qcom-spmi-pmic-arb: transaction failed")
                };
            }
            time::udelay(1);
        }

        Err("qcom-spmi-pmic-arb: transaction timed out")
    }

    /// Read one to eight bytes with an SPMI extended-read-long transaction.
    pub fn read(&self, sid: u8, address: u16, output: &mut [u8]) -> Result<(), &'static str> {
        if output.is_empty() || output.len() > MAX_TRANSFER {
            return Err("qcom-spmi-pmic-arb: read length must be 1..=8 bytes");
        }

        let channel = self.observer_offset(sid, address)?;
        let byte_count = (output.len() - 1) as u32;
        let command =
            (OP_EXT_READ_LONG << 27) | (((address as u32) & 0xff) << 4) | (byte_count & 0x7);
        let _guard = self.transaction_lock.lock();

        // SAFETY: the channel was derived from the firmware APID map and lies
        // in the mapped observer window.
        unsafe { mmio::write32(self.observer_base + channel + CHANNEL_CMD, command) };
        self.wait_for_done(channel)?;

        // SAFETY: the response registers belong to the selected observer channel.
        let low = unsafe { mmio::read32(self.observer_base + channel + CHANNEL_RDATA0) };
        let high = if output.len() > 4 {
            // SAFETY: same mapped observer channel as above.
            unsafe { mmio::read32(self.observer_base + channel + CHANNEL_RDATA1) }
        } else {
            0
        };
        let response = (low as u64 | ((high as u64) << 32)).to_le_bytes();
        output.copy_from_slice(&response[..output.len()]);
        Ok(())
    }
}

static CONTROLLER: IrqSpinLock<Option<Arc<QcomSpmiPmicArb>>> = IrqSpinLock::new(None);

/// Return the registered SC7180 PMIC Arbiter controller.
pub fn get_controller() -> Option<Arc<QcomSpmiPmicArb>> {
    CONTROLLER.lock().as_ref().map(Arc::clone)
}

fn be_u32_property(device: &PlatformDeviceInfo, name: &str) -> Option<u32> {
    let bytes = device.property(name)?.value();
    let bytes: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn memory_resources(device: &PlatformDeviceInfo) -> Vec<(usize, usize)> {
    device
        .get_resources()
        .iter()
        .filter(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .filter_map(|resource| {
            let size = resource.end.checked_sub(resource.start)?.checked_add(1)?;
            Some((resource.start, size))
        })
        .collect()
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resources = memory_resources(device);
    if resources.len() < 5 {
        return Err("qcom-spmi-pmic-arb: expected five MMIO resources");
    }

    let (core_paddr, core_size) = resources[0];
    let (observer_paddr, observer_size) = resources[2];
    let (config_paddr, config_size) = resources[4];
    let core =
        vm::ioremap(core_paddr, core_size).map_err(|_| "qcom-spmi-pmic-arb: failed to map core")?;
    let observer = vm::ioremap(observer_paddr, observer_size)
        .map_err(|_| "qcom-spmi-pmic-arb: failed to map observer channels")?;
    let config = vm::ioremap(config_paddr, config_size)
        .map_err(|_| "qcom-spmi-pmic-arb: failed to map configuration")?;

    // SAFETY: all offsets are within the mapped core window described by DT.
    let version = unsafe { mmio::read32(core + PMIC_ARB_VERSION) };
    if !(PMIC_ARB_VERSION_V5_MIN..PMIC_ARB_VERSION_V7_MIN).contains(&version) {
        return Err("qcom-spmi-pmic-arb: SC7180 requires PMIC arbiter v5");
    }

    let ee = be_u32_property(device, "qcom,ee").ok_or("qcom-spmi-pmic-arb: qcom,ee missing")?;
    if ee > 5 {
        return Err("qcom-spmi-pmic-arb: invalid execution environment");
    }

    // SAFETY: PMIC_ARB_FEATURES is in the mapped core window.
    let apid_count = (unsafe { mmio::read32(core + PMIC_ARB_FEATURES) }
        & PMIC_ARB_FEATURES_PERIPH_MASK) as usize;
    if apid_count == 0 || apid_count > MAX_APIDS {
        return Err("qcom-spmi-pmic-arb: invalid APID count");
    }

    let mut ppid_to_apid = vec![APID_INVALID; MAX_PPIDS];
    for apid in 0..apid_count {
        let map_offset = APID_MAP_BASE + APID_MAP_STRIDE * apid;
        if map_offset + 4 > core_size {
            break;
        }

        // SAFETY: bounds checked against the mapped core window.
        let mapping = unsafe { mmio::read32(core + map_offset) };
        if mapping == 0 {
            continue;
        }

        let ppid = ((mapping >> 8) & 0xfff) as usize;
        // Linux prefers the duplicate APID owned by the current EE. Reads use
        // the selected APID's observer channel even though no writes are made.
        // SAFETY: config has at least the DT-provided window; reject truncated maps.
        let owner_offset = APID_OWNER_BASE + APID_MAP_STRIDE * apid;
        if owner_offset + 4 > config_size {
            return Err("qcom-spmi-pmic-arb: truncated ownership table");
        }
        let owner = unsafe { mmio::read32(config + owner_offset) } & 0x7;
        if ppid_to_apid[ppid] == APID_INVALID || owner == ee {
            ppid_to_apid[ppid] = apid as u16;
        }
    }

    let controller = Arc::new(QcomSpmiPmicArb {
        observer_base: observer,
        ee: ee as u8,
        ppid_to_apid,
        transaction_lock: IrqSpinLock::new(()),
    });
    *CONTROLLER.lock() = Some(controller);

    early_println!(
        "[qcom-spmi-pmic-arb] registered v{:#x} EE={} APIDs={} paddr={:#x}",
        version,
        ee,
        apid_count,
        core_paddr
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    *CONTROLLER.lock() = None;
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-spmi-pmic-arb",
        probe,
        remove,
        vec!["qcom,spmi-pmic-arb"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Core);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SPMI_PMIC_ARB_ANCHOR: fn() = force_link;

/// Force the linker to retain this driver crate.
#[inline(never)]
pub fn force_link() {}
