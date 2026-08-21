// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! CoachZ wall-clock initialization with a read-only PM6150 fallback.
//!
//! # Provenance
//!
//! ChromeOS exposes the CoachZ wall clock through the primary Chrome EC. The EC
//! command and feature detection follow Linux `drivers/rtc/rtc-cros-ec.c` and
//! `drivers/mfd/cros_ec_dev.c`. The PMIC register layout and rollover-safe read
//! sequence follow Linux `drivers/rtc/rtc-pm8xxx.c`.
//!
//! The PM6150 RTC node is disabled in upstream DT and commonly contains an
//! uninitialized counter on CoachZ. It is therefore only a read-only fallback,
//! and every source is range checked before Scarlet's wall clock is seeded.

extern crate alloc;

use scarlet::{device::fdt::FdtManager, early_println, time};
use scarlet_driver_cros_ec_spi::get_primary_cros_ec_spi;
use scarlet_driver_qcom_spmi_pmic_arb::{QcomSpmiPmicArb, get_controller};

const EC_COMMAND_GET_FEATURES: u16 = 0x000d;
const EC_COMMAND_RTC_GET_VALUE: u16 = 0x0044;
const EC_FEATURE_RTC: u32 = 27;
const RTC_READ_OFFSET: u16 = 0x48;
const RTC_BYTES: usize = 4;
const NANOS_PER_SECOND: u64 = 1_000_000_000;
const MINIMUM_PLAUSIBLE_EPOCH: u32 = 946_684_800; // 2000-01-01 UTC
const MAXIMUM_PLAUSIBLE_EPOCH: u32 = 4_102_444_800; // 2100-01-01 UTC

fn node_has_compatible(node: &fdt::node::FdtNode<'_, '_>, compatible: &str) -> bool {
    node.compatible()
        .is_some_and(|entries| entries.all().any(|entry| entry == compatible))
}

fn first_be_u32_property(node: &fdt::node::FdtNode<'_, '_>, name: &str) -> Option<u32> {
    let bytes = node.property(name)?.value;
    if bytes.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn find_pm6150_rtc(fdt: &fdt::Fdt<'_>) -> Option<(u8, u16)> {
    let root = fdt.find_node("/")?;
    let mut stack = alloc::vec![root];

    while let Some(node) = stack.pop() {
        if node_has_compatible(&node, "qcom,spmi-pmic-arb") {
            for pmic in node.children() {
                if !node_has_compatible(&pmic, "qcom,pm6150") {
                    continue;
                }
                let sid = u8::try_from(first_be_u32_property(&pmic, "reg")?).ok()?;
                for child in pmic.children() {
                    if node_has_compatible(&child, "qcom,pm8941-rtc") {
                        let base = u16::try_from(first_be_u32_property(&child, "reg")?).ok()?;
                        return Some((sid, base));
                    }
                }
            }
        }

        for child in node.children() {
            stack.push(child);
        }
    }
    None
}

fn read_raw_seconds(
    controller: &QcomSpmiPmicArb,
    sid: u8,
    rtc_base: u16,
) -> Result<u32, &'static str> {
    let address = rtc_base
        .checked_add(RTC_READ_OFFSET)
        .ok_or("qcom-pm8941-rtc: register address overflow")?;
    let mut bytes = [0u8; RTC_BYTES];
    controller.read(sid, address, &mut bytes)?;

    // Re-read the LSB. If it wrapped, the upper bytes belong to the preceding
    // second and the complete value must be sampled again.
    let mut lsb = [0u8; 1];
    controller.read(sid, address, &mut lsb)?;
    if lsb[0] < bytes[0] {
        controller.read(sid, address, &mut bytes)?;
    }

    Ok(u32::from_le_bytes(bytes))
}

fn validate_epoch(seconds: u32) -> Result<u32, &'static str> {
    if !(MINIMUM_PLAUSIBLE_EPOCH..MAXIMUM_PLAUSIBLE_EPOCH).contains(&seconds) {
        return Err("RTC epoch is outside the plausible 2000..2100 range");
    }
    Ok(seconds)
}

fn initialize_from_sample(
    source: &'static str,
    seconds: u32,
    mono_before: u64,
    mono_after: u64,
) -> Result<(), &'static str> {
    let seconds = validate_epoch(seconds)?;
    let epoch_ns = (seconds as u64)
        .checked_mul(NANOS_PER_SECOND)
        .ok_or("RTC epoch overflow")?;

    match time::initialize_wall_clock_from_rtc_sample(epoch_ns, mono_before, mono_after) {
        Ok(()) => {
            early_println!(
                "[coachz-rtc] seeded wall clock from {}: epoch={}",
                source,
                seconds
            );
            Ok(())
        }
        Err("wall clock already initialized") => {
            early_println!("[coachz-rtc] wall clock already initialized");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn seed_from_cros_ec() -> Result<(), &'static str> {
    let ec = get_primary_cros_ec_spi().ok_or("primary Chrome EC unavailable")?;

    let features = ec
        .command(EC_COMMAND_GET_FEATURES, 0, &[])
        .map_err(|_| "Chrome EC GET_FEATURES failed")?;
    if features.len() != 8 {
        return Err("Chrome EC returned a malformed feature bitmap");
    }
    let feature_word = u32::from_le_bytes([features[0], features[1], features[2], features[3]]);
    if feature_word & (1 << EC_FEATURE_RTC) == 0 {
        return Err("Chrome EC does not advertise RTC support");
    }

    let mono_before = time::current_time_ns();
    let response = ec
        .command(EC_COMMAND_RTC_GET_VALUE, 0, &[])
        .map_err(|_| "Chrome EC RTC_GET_VALUE failed")?;
    let mono_after = time::current_time_ns();
    if response.len() != RTC_BYTES {
        return Err("Chrome EC returned a malformed RTC value");
    }
    let seconds = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
    initialize_from_sample("Chrome EC", seconds, mono_before, mono_after)
}

fn seed_from_pm6150() -> Result<(), &'static str> {
    let controller = get_controller().ok_or("qcom-pm8941-rtc: SPMI controller unavailable")?;
    let fdt = FdtManager::get_manager()
        .get_fdt()
        .ok_or("qcom-pm8941-rtc: FDT unavailable")?;
    let (sid, rtc_base) =
        find_pm6150_rtc(fdt).ok_or("qcom-pm8941-rtc: PM6150 RTC node not found")?;

    let mono_before = time::current_time_ns();
    let seconds = read_raw_seconds(&controller, sid, rtc_base)?;
    let mono_after = time::current_time_ns();
    validate_epoch(seconds).map_err(|_| "PM6150 RTC is uninitialized or implausible")?;
    early_println!(
        "[coachz-rtc] using PM6150 fallback: sid={} base={:#x}",
        sid,
        rtc_base
    );
    initialize_from_sample("PM6150", seconds, mono_before, mono_after)
}

fn initialize_wall_clock() {
    if let Err(ec_error) = seed_from_cros_ec() {
        early_println!("[coachz-rtc] Chrome EC unavailable: {}", ec_error);
        if let Err(error) = seed_from_pm6150() {
            early_println!("[coachz-rtc] no usable wall-clock source: {}", error);
        }
    }
}

scarlet::late_initcall!(initialize_wall_clock);

#[used]
static SCARLET_DRIVER_QCOM_PM8941_RTC_ANCHOR: fn() = force_link;

/// Force the linker to retain this driver crate.
#[inline(never)]
pub fn force_link() {}
