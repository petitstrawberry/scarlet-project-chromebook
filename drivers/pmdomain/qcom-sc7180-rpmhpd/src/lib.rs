// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! SC7180 RPMh power-domain provider.
//!
//! Domain resource addresses and ARC level counts come from the firmware
//! Command DB.  Before Scarlet has a genpd-style late sync phase, enable votes
//! remain clamped to the highest firmware-supported corner, matching Linux's
//! pre-`sync_state` safety behavior.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use qcom_cmd_db::{read_address, read_aux_u16};
use qcom_rpmh_rsc::{RpmhRsc, controller};
use scarlet::{
    device::{
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
        power::{PowerDomain, PowerDomainProvider, PowerManager},
    },
    early_println,
    sync::IrqSpinLock,
};

const SC7180_CX: usize = 0;
const SC7180_CX_AO: usize = 1;
const SC7180_GFX: usize = 2;
const SC7180_MX: usize = 3;
const SC7180_MX_AO: usize = 4;
const SC7180_LMX: usize = 5;
const SC7180_LCX: usize = 6;
const SC7180_MSS: usize = 7;
const SC7180_DOMAIN_COUNT: usize = 8;
const RPMH_ARC_MAX_LEVELS: usize = 32;

struct RpmhPowerDomain {
    rsc: Arc<RpmhRsc>,
    label: &'static str,
    address: u32,
    maximum_corner: u32,
    parent: Option<Arc<RpmhPowerDomain>>,
    enabled: IrqSpinLock<bool>,
}

impl RpmhPowerDomain {
    fn new(
        rsc: &Arc<RpmhRsc>,
        label: &'static str,
        resource_name: &'static str,
        parent: Option<Arc<RpmhPowerDomain>>,
    ) -> Result<Arc<Self>, &'static str> {
        let address = read_address(resource_name)
            .ok_or("qcom-sc7180-rpmhpd: Command DB resource is missing")?;
        let levels = read_aux_u16(resource_name)
            .ok_or("qcom-sc7180-rpmhpd: Command DB ARC levels are missing")?;
        let maximum_corner = maximum_corner(&levels)?;
        Ok(Arc::new(Self {
            rsc: Arc::clone(rsc),
            label,
            address,
            maximum_corner,
            parent,
            enabled: IrqSpinLock::new(false),
        }))
    }
}

impl PowerDomain for RpmhPowerDomain {
    fn enable(&self) -> Result<(), &'static str> {
        if *self.enabled.lock() {
            return Ok(());
        }
        if let Some(parent) = &self.parent {
            parent.enable()?;
        }

        let mut enabled = self.enabled.lock();
        if *enabled {
            return Ok(());
        }
        early_println!(
            "[qcom-sc7180-rpmhpd] voting {} address={:#x} corner={}",
            self.label,
            self.address,
            self.maximum_corner,
        );
        self.rsc.write_active(self.address, self.maximum_corner)?;
        *enabled = true;
        early_println!("[qcom-sc7180-rpmhpd] domain {} enabled", self.label);
        Ok(())
    }

    fn disable(&self) -> Result<(), &'static str> {
        let mut enabled = self.enabled.lock();
        if !*enabled {
            return Ok(());
        }
        self.rsc.write_active(self.address, 0)?;
        *enabled = false;
        Ok(())
    }

    fn is_enabled(&self) -> bool {
        *self.enabled.lock()
    }

    fn label(&self) -> &str {
        self.label
    }
}

struct Sc7180RpmhPdProvider {
    domains: Vec<Option<Arc<RpmhPowerDomain>>>,
}

impl Sc7180RpmhPdProvider {
    fn new(rsc: &Arc<RpmhRsc>) -> Result<Self, &'static str> {
        let mx = RpmhPowerDomain::new(rsc, "mx", "mx.lvl", None)?;
        let mx_ao = RpmhPowerDomain::new(rsc, "mx_ao", "mx.lvl", None)?;
        let cx = RpmhPowerDomain::new(rsc, "cx", "cx.lvl", Some(Arc::clone(&mx)))?;
        let cx_ao = RpmhPowerDomain::new(rsc, "cx_ao", "cx.lvl", Some(Arc::clone(&mx_ao)))?;
        let gfx = RpmhPowerDomain::new(rsc, "gfx", "gfx.lvl", None)?;
        let lmx = RpmhPowerDomain::new(rsc, "lmx", "lmx.lvl", None)?;
        let lcx = RpmhPowerDomain::new(rsc, "lcx", "lcx.lvl", None)?;
        let mss = RpmhPowerDomain::new(rsc, "mss", "mss.lvl", None)?;

        let mut domains = vec![None; SC7180_DOMAIN_COUNT];
        domains[SC7180_CX] = Some(cx);
        domains[SC7180_CX_AO] = Some(cx_ao);
        domains[SC7180_GFX] = Some(gfx);
        domains[SC7180_MX] = Some(mx);
        domains[SC7180_MX_AO] = Some(mx_ao);
        domains[SC7180_LMX] = Some(lmx);
        domains[SC7180_LCX] = Some(lcx);
        domains[SC7180_MSS] = Some(mss);
        Ok(Self { domains })
    }
}

impl PowerDomainProvider for Sc7180RpmhPdProvider {
    fn power_domain_cells(&self) -> usize {
        1
    }

    fn get_domain(&self, specifier: &[u32]) -> Result<Arc<dyn PowerDomain>, &'static str> {
        let [domain_id] = specifier else {
            return Err("qcom-sc7180-rpmhpd: expected one domain cell");
        };
        let index = usize::try_from(*domain_id)
            .map_err(|_| "qcom-sc7180-rpmhpd: domain id does not fit usize")?;
        self.domains
            .get(index)
            .and_then(Option::as_ref)
            .cloned()
            .map(|domain| domain as Arc<dyn PowerDomain>)
            .ok_or("qcom-sc7180-rpmhpd: unsupported domain id")
    }
}

fn maximum_corner(levels: &[u16]) -> Result<u32, &'static str> {
    if levels.is_empty() || levels.len() > RPMH_ARC_MAX_LEVELS {
        return Err("qcom-sc7180-rpmhpd: invalid ARC level count");
    }
    let level_count = levels
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, level)| **level == 0)
        .map(|(index, _)| index)
        .unwrap_or(levels.len());
    if level_count == 0 {
        return Err("qcom-sc7180-rpmhpd: ARC level table is empty");
    }
    u32::try_from(level_count - 1).map_err(|_| "qcom-sc7180-rpmhpd: ARC corner overflow")
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    for name in ["phandle", "linux,phandle"] {
        let Some(property) = device.property(name) else {
            continue;
        };
        let [a, b, c, d] = property.value() else {
            return Err("qcom-sc7180-rpmhpd: malformed phandle");
        };
        return Ok(u32::from_be_bytes([*a, *b, *c, *d]));
    }
    Err("qcom-sc7180-rpmhpd: missing phandle")
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let parent_phandle = device
        .parent_phandle()
        .ok_or("qcom-sc7180-rpmhpd: parent RSC phandle is missing")?;
    let Some(rsc) = controller(parent_phandle) else {
        return probe_defer();
    };
    let phandle = read_phandle(device)?;
    let provider = Arc::new(Sc7180RpmhPdProvider::new(&rsc)?);
    PowerManager::init();
    PowerManager::register_provider(phandle, provider);
    early_println!(
        "[qcom-sc7180-rpmhpd] registered phandle={:#x} parent={:#x} domains={}",
        phandle,
        parent_phandle,
        SC7180_DOMAIN_COUNT,
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "qcom-sc7180-rpmhpd",
            probe,
            remove,
            vec!["qcom,sc7180-rpmhpd"],
        )),
        DriverPriority::Critical,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_RPMHPD_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::maximum_corner;

    #[test_case]
    fn maximum_corner_ignores_zero_padding() {
        assert_eq!(maximum_corner(&[0, 16, 48, 64, 0, 0]), Ok(3));
    }
}
