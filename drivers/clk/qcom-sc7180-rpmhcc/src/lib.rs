// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! SC7180 RPMh clock provider.
//!
//! The current CoachZ BSP consumes the two CXO clock IDs. Both IDs vote the
//! shared Command DB `xo.lvl` ARC resource and differ only in their future
//! sleep-state policy. Scarlet does not enter RPMh-managed system suspend yet,
//! so this driver sends the required synchronous active vote and aggregates the
//! normal and always-on users before changing hardware state.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};

use qcom_cmd_db::read_address;
use qcom_rpmh_rsc::{RpmhRsc, controller};
use scarlet::{
    device::{
        clk::{Clk, ClkError, ClkHandle, ClkProvider},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
    sync::IrqSpinLock,
};

// include/dt-bindings/clock/qcom,rpmh.h
const RPMH_CXO_CLK: u32 = 0;
const RPMH_CXO_CLK_A: u32 = 1;
const CXO_CLOCK_COUNT: usize = 2;

// Linux DEFINE_CLK_RPMH_ARC(bi_tcxo, "xo.lvl", 0x3, 2).
const XO_RESOURCE_NAME: &str = "xo.lvl";
const XO_ENABLE_LEVEL: u32 = 0x3;
const XO_DIVIDER: u64 = 2;

struct CxoResource {
    rsc: Arc<RpmhRsc>,
    parent: ClkHandle,
    address: u32,
    enabled: IrqSpinLock<[bool; CXO_CLOCK_COUNT]>,
}

impl CxoResource {
    fn set_enabled(&self, index: usize, enable: bool) -> Result<(), &'static str> {
        let mut enabled = self.enabled.lock();
        if enabled[index] == enable {
            return Ok(());
        }

        let aggregate_before = enabled.iter().any(|value| *value);
        enabled[index] = enable;
        let aggregate_after = enabled.iter().any(|value| *value);
        if aggregate_before == aggregate_after {
            return Ok(());
        }

        let level = if aggregate_after { XO_ENABLE_LEVEL } else { 0 };
        early_println!(
            "[qcom-sc7180-rpmhcc] voting {} address={:#x} level={}",
            XO_RESOURCE_NAME,
            self.address,
            level,
        );
        if let Err(error) = self.rsc.write_active(self.address, level) {
            enabled[index] = !enable;
            return Err(error);
        }
        early_println!(
            "[qcom-sc7180-rpmhcc] {} active level={}",
            XO_RESOURCE_NAME,
            level,
        );
        Ok(())
    }
}

struct RpmhCxoClock {
    resource: Arc<CxoResource>,
    id: u32,
}

impl RpmhCxoClock {
    fn index(&self) -> usize {
        usize::try_from(self.id).unwrap_or(0)
    }
}

impl Clk for RpmhCxoClock {
    fn name(&self) -> &'static str {
        match self.id {
            RPMH_CXO_CLK => "bi_tcxo",
            RPMH_CXO_CLK_A => "bi_tcxo_ao",
            _ => "bi_tcxo_invalid",
        }
    }

    fn prepare(&self) -> Result<(), ClkError> {
        self.resource.parent.prepare_enable()
    }

    fn unprepare(&self) {
        self.resource.parent.disable_unprepare();
    }

    fn enable(&self) -> Result<(), ClkError> {
        self.resource
            .set_enabled(self.index(), true)
            .map_err(|_| ClkError::HardwareError)
    }

    fn disable(&self) {
        if let Err(error) = self.resource.set_enabled(self.index(), false) {
            early_println!(
                "[qcom-sc7180-rpmhcc] failed to release {}: {}",
                self.name(),
                error,
            );
        }
    }

    fn is_enabled(&self) -> bool {
        self.resource.enabled.lock()[self.index()]
    }

    fn recalc_rate(&self, parent_rate: u64) -> u64 {
        parent_rate / XO_DIVIDER
    }

    fn round_rate(&self, rate: u64, parent_rate: u64) -> Result<u64, ClkError> {
        let fixed_rate = self.recalc_rate(parent_rate);
        if rate == fixed_rate {
            Ok(fixed_rate)
        } else {
            Err(ClkError::InvalidRate)
        }
    }

    fn parent(&self) -> Option<ClkHandle> {
        Some(self.resource.parent.clone())
    }
}

struct Sc7180RpmhClockProvider {
    clocks: [ClkHandle; CXO_CLOCK_COUNT],
}

impl Sc7180RpmhClockProvider {
    fn new(resource: &Arc<CxoResource>) -> Self {
        Self {
            clocks: core::array::from_fn(|index| {
                ClkHandle::new(Arc::new(RpmhCxoClock {
                    resource: Arc::clone(resource),
                    id: u32::try_from(index).unwrap_or(RPMH_CXO_CLK),
                }))
            }),
        }
    }
}

impl ClkProvider for Sc7180RpmhClockProvider {
    fn name(&self) -> &'static str {
        "qcom-sc7180-rpmhcc"
    }

    fn clock_cells(&self) -> usize {
        1
    }

    fn get_clk(&self, spec: &[u32]) -> Result<ClkHandle, ClkError> {
        let [id @ (RPMH_CXO_CLK | RPMH_CXO_CLK_A)] = spec else {
            return Err(ClkError::ClockNotFound);
        };
        self.clocks
            .get(usize::try_from(*id).map_err(|_| ClkError::ClockNotFound)?)
            .cloned()
            .ok_or(ClkError::ClockNotFound)
    }
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    for name in ["phandle", "linux,phandle"] {
        let Some(property) = device.property(name) else {
            continue;
        };
        let [a, b, c, d] = property.value() else {
            return Err("qcom-sc7180-rpmhcc: malformed phandle");
        };
        return Ok(u32::from_be_bytes([*a, *b, *c, *d]));
    }
    Err("qcom-sc7180-rpmhcc: missing phandle")
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let parent_phandle = device
        .parent_phandle()
        .ok_or("qcom-sc7180-rpmhcc: parent RSC phandle is missing")?;
    let Some(rsc) = controller(parent_phandle) else {
        return probe_defer();
    };
    let parent = match DeviceManager::get_manager().resolve_clk(device, "xo") {
        Ok(parent) => parent,
        Err("clk: provider not found" | "clk: clock not found") => return probe_defer(),
        Err(error) => return Err(error),
    };
    let address = read_address(XO_RESOURCE_NAME)
        .ok_or("qcom-sc7180-rpmhcc: xo.lvl is missing from Command DB")?;
    let phandle = read_phandle(device)?;
    let resource = Arc::new(CxoResource {
        rsc,
        parent,
        address,
        enabled: IrqSpinLock::new([false; CXO_CLOCK_COUNT]),
    });
    DeviceManager::get_manager()
        .register_clk_provider(phandle, Arc::new(Sc7180RpmhClockProvider::new(&resource)));
    early_println!(
        "[qcom-sc7180-rpmhcc] registered phandle={:#x} parent={:#x} xo-address={:#x}",
        phandle,
        parent_phandle,
        address,
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-rpmhcc",
            probe,
            remove,
            vec!["qcom,sc7180-rpmh-clk"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_RPMHCC_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cxo_ids_match_qcom_rpmh_binding() {
        assert_eq!(RPMH_CXO_CLK, 0);
        assert_eq!(RPMH_CXO_CLK_A, 1);
    }

    #[test]
    fn sc7180_cxo_rate_is_parent_divided_by_two() {
        assert_eq!(38_400_000 / XO_DIVIDER, 19_200_000);
    }
}
