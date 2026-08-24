// SPDX-License-Identifier: GPL-2.0-only
//! Qualcomm Adreno 618 and legacy GMU driver for SC7180.
//!
//! The low-level GMU/HFI implementation is shared by the platform GPU backend
//! in this crate. The old compositor-only R2D byte protocol is deliberately
//! not part of the module graph; userspace submits the versioned native A6xx
//! PM4/relocation dialect through Scarlet's generic GPU queue ABI.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, vec};
use scarlet::device::{
    manager::{DeviceManager, DriverPriority},
    platform::{PlatformDeviceDriver, PlatformDeviceInfo},
};

mod backend;
mod firmware;
mod gmu;
mod hfi;
mod hfi_abi;
mod memory;
mod registers;
mod submit;

fn remove_gmu(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    gmu::remove(device)
}

fn register_gpu_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-adreno-a618",
        backend::probe,
        backend::remove,
        vec!["qcom,adreno-618.0", "qcom,adreno"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

fn register_drivers() {
    register_gmu_driver();
    register_gpu_driver();
}

fn register_gmu_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-adreno-a618-gmu",
        gmu::probe,
        remove_gmu,
        vec!["qcom,adreno-gmu-618.0", "qcom,adreno-gmu"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_drivers);
#[used]
static SCARLET_DRIVER_QCOM_ADRENO_A618_ANCHOR: fn() = force_link;

/// Force this external driver crate into the final kernel link graph.
///
/// Calling this function is unnecessary; referencing it retains the initcall.
#[inline(never)]
pub fn force_link() {}
