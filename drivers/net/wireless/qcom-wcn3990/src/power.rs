// SPDX-License-Identifier: GPL-2.0-only
//! Active WLAN LDO votes. RPMh aggregates APSS and remote processor votes.
//! Keep the AP vote while the radio exists; disconnecting a STA is not poweroff.
use alloc::{format, sync::Arc, vec::Vec};
use qcom_rpmh_rsc::{ActiveCommand, RpmhRsc};
use scarlet::device::{fdt::FdtManager, manager::PROBE_DEFER, platform::PlatformDeviceInfo};
pub struct Power {
    rsc: Arc<RpmhRsc>,
    rails: Vec<(u32, u32)>,
}
impl Power {
    pub fn prepare(device: &PlatformDeviceInfo) -> Result<Self, &'static str> {
        let fdt = FdtManager::get_manager().get_fdt().ok_or(PROBE_DEFER)?;
        let rsc = fdt
            .all_nodes()
            .find(|n| {
                n.compatible()
                    .is_some_and(|c| c.all().any(|s| s == "qcom,sc7180-rpmh-apps-rsc"))
            })
            .ok_or("wcn3990: apps RSC missing")?;
        let driver = rsc
            .property("qcom,drv-id")
            .and_then(|p| p.as_usize())
            .ok_or("wcn3990: apps RSC driver ID missing")?;
        let resource = rsc
            .reg()
            .and_then(|mut regions| regions.nth(driver))
            .ok_or("wcn3990: RSC register window missing")?;
        let rsc =
            qcom_rpmh_rsc::controller_at(resource.starting_address as u64).ok_or(PROBE_DEFER)?;
        let mut rails = Vec::new();
        for name in [
            "vdd-0.8-cx-mx-supply",
            "vdd-1.8-xo-supply",
            "vdd-1.3-rfa-supply",
            "vdd-3.3-ch0-supply",
            "vdd-3.3-ch1-supply",
        ] {
            let phandle = device
                .property(name)
                .and_then(|p| p.as_usize())
                .ok_or("wcn3990: WLAN supply missing")?;
            let parent = fdt
                .all_nodes()
                .find(|n| {
                    n.children()
                        .any(|c| c.property("phandle").and_then(|p| p.as_usize()) == Some(phandle))
                })
                .ok_or("wcn3990: regulator parent missing")?;
            let child = parent
                .children()
                .find(|c| c.property("phandle").and_then(|p| p.as_usize()) == Some(phandle))
                .unwrap();
            let pmic = parent
                .property("qcom,pmic-id")
                .and_then(|p| p.as_str())
                .ok_or("wcn3990: PMIC identity missing")?;
            if !matches!(pmic, "a" | "c") {
                return Err("wcn3990: unsupported PMIC");
            }
            let ldo = child
                .name
                .strip_prefix("ldo")
                .and_then(|n| n.parse::<u32>().ok())
                .filter(|n| *n > 0 && *n <= 19)
                .ok_or("wcn3990: WLAN supply is not an LDO")?;
            let minimum = child
                .property("regulator-min-microvolt")
                .and_then(|p| p.as_usize())
                .ok_or("wcn3990: regulator minimum missing")?;
            let maximum = child
                .property("regulator-max-microvolt")
                .and_then(|p| p.as_usize())
                .ok_or("wcn3990: regulator maximum missing")?;
            let mode = child
                .property("regulator-initial-mode")
                .and_then(|p| p.as_usize());
            if minimum == 0 || minimum > maximum || maximum > 4000000 || mode != Some(3) {
                return Err("wcn3990: unexpected WLAN LDO constraints");
            }
            let address = qcom_cmd_db::read_address(&format!("ldo{pmic}{ldo}"))
                .filter(|a| *a != 0 && *a % 4 == 0)
                .ok_or("wcn3990: WLAN voltage resource absent")?;
            rails.push((address, minimum.div_ceil(1000) as u32));
        }
        Ok(Self { rsc, rails })
    }
    pub fn enable(&self) -> Result<(), &'static str> {
        for (address, millivolts) in &self.rails {
            self.rsc.write_active_batch(&[
                ActiveCommand {
                    address: *address,
                    data: *millivolts,
                    wait_for_completion: true,
                },
                ActiveCommand {
                    address: *address + 8,
                    data: 7,
                    wait_for_completion: true,
                },
                ActiveCommand {
                    address: *address + 4,
                    data: 1,
                    wait_for_completion: true,
                },
            ])?;
        }
        Ok(())
    }
}
