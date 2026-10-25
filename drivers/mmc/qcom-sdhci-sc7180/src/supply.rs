//! CoachZ's PM6150L SD rails, resolved from the consumer's DT phandles.
//! RPMh VRM register layout follows Linux qcom-rpmh-regulator.c.
use alloc::sync::Arc;
use qcom_rpmh_rsc::RpmhRsc;
use scarlet::device::{fdt::FdtManager, manager::PROBE_DEFER, platform::PlatformDeviceInfo};

pub(super) struct SdSupplies {
    rsc: Arc<RpmhRsc>,
    vmmc: u32,
    vqmmc: u32,
}

impl SdSupplies {
    pub(super) fn resolve(device: &PlatformDeviceInfo) -> Result<Self, &'static str> {
        let fdt = FdtManager::get_manager()
            .get_fdt()
            .ok_or("SD supplies: missing FDT")?;
        let supply_id = |name| {
            device
                .property(name)
                .and_then(|p| p.as_usize())
                .map(|v| v as u32)
        };
        let vmmc_id = supply_id("vmmc-supply").ok_or("SD supplies: missing vmmc")?;
        let vqmmc_id = supply_id("vqmmc-supply").ok_or("SD supplies: missing vqmmc")?;
        let cell = |value: &[u8]| {
            value
                .get(..4)
                .and_then(|v| <[u8; 4]>::try_from(v).ok())
                .map(u32::from_be_bytes)
        };
        for rsc_node in fdt.all_nodes() {
            for regulators in rsc_node.children() {
                if !regulators
                    .compatible()
                    .is_some_and(|c| c.all().any(|s| s == "qcom,pm6150l-rpmh-regulators"))
                    || regulators.property("qcom,pmic-id").and_then(|p| p.as_str()) != Some("c")
                {
                    continue;
                }
                let mut card = false;
                let mut io = false;
                for rail in regulators.children() {
                    let id = rail
                        .property("phandle")
                        .or_else(|| rail.property("linux,phandle"))
                        .and_then(|p| cell(p.value));
                    // These two PMIC5 PLDOs are wired to SDCC2 on CoachZ.
                    if rail.name == "ldo9" && id == Some(vmmc_id) {
                        card = rail
                            .property("regulator-min-microvolt")
                            .and_then(|p| cell(p.value))
                            == Some(2_952_000)
                            && rail
                                .property("regulator-max-microvolt")
                                .and_then(|p| cell(p.value))
                                == Some(2_952_000);
                    }
                    if rail.name == "ldo6" && id == Some(vqmmc_id) {
                        io = rail
                            .property("regulator-min-microvolt")
                            .and_then(|p| cell(p.value))
                            == Some(1_800_000)
                            && rail
                                .property("regulator-max-microvolt")
                                .and_then(|p| cell(p.value))
                                == Some(2_952_000);
                    }
                }
                if !card || !io {
                    continue;
                }
                let phandle = scarlet::device::manager::DeviceManager::get_manager()
                    .phandle_for_fdt_node(&rsc_node);
                let rsc = qcom_rpmh_rsc::controller(phandle).ok_or(PROBE_DEFER)?;
                let vmmc = qcom_cmd_db::read_address("ldoc9")
                    .filter(|v| *v != 0)
                    .ok_or(PROBE_DEFER)?;
                let vqmmc = qcom_cmd_db::read_address("ldoc6")
                    .filter(|v| *v != 0)
                    .ok_or(PROBE_DEFER)?;
                return Ok(Self { rsc, vmmc, vqmmc });
            }
        }
        Err("SD supplies: unsupported rail wiring")
    }

    pub(super) fn set_io(&self, low: bool) -> Result<(), &'static str> {
        self.rsc
            .write_active(self.vqmmc, if low { 1800 } else { 2952 })?;
        scarlet::time::udelay(5_000);
        Ok(())
    }

    pub(super) fn power_cycle(&self) -> Result<(), &'static str> {
        self.rsc.write_active(self.vmmc + 4, 0)?;
        scarlet::time::udelay(10_000);
        self.set_io(false)?;
        self.rsc.write_active(self.vmmc, 2952)?;
        self.rsc.write_active(self.vmmc + 4, 1)?;
        scarlet::time::udelay(10_000);
        Ok(())
    }
}
