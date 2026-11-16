// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
//! AOSS MSS_RESTART and PDC MODEM_SYNC_RESET for SC7180 MSS PIL.
//! Register/DT IDs and six-sleep-cycle delays follow Linux's
//! drivers/reset/reset-qcom-{aoss,pdc}.c. Other subsystem reset IDs are not
//! exposed by this provider. Registration itself never toggles a reset line.
extern crate alloc;
use alloc::{boxed::Box, sync::Arc, vec};
use scarlet::{
    arch::{self, mmio},
    device::{
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
        reset::ResetController,
    },
    sync::IrqSpinLock,
    time, vm,
};

#[derive(Clone, Copy)]
enum Kind {
    Aoss,
    Pdc,
}
struct MssReset {
    base: usize,
    kind: Kind,
    lock: IrqSpinLock<()>,
}
impl MssReset {
    fn set(&self, spec: &[u32], asserted: bool) -> Result<(), &'static str> {
        let (id, offset) = match self.kind {
            Kind::Aoss => (0, 0x10000),
            Kind::Pdc => (9, 0x100),
        };
        if spec != [id] {
            return Err("qcom-mss-reset: unsupported reset line");
        }
        {
            let _guard = self.lock.lock();
            // SAFETY: probe validated/mapped the complete register window.
            unsafe {
                match self.kind {
                    Kind::Aoss => mmio::write32(self.base + offset, u32::from(asserted)),
                    Kind::Pdc => {
                        let old = mmio::read32(self.base + offset);
                        let mask = 1 << 9;
                        mmio::write32(
                            self.base + offset,
                            if asserted { old | mask } else { old & !mask },
                        );
                    }
                }
                arch::io_wmb();
                let _ = mmio::read32(self.base + offset);
            }
        }
        if matches!(self.kind, Kind::Aoss) {
            // Six 32-kHz sleep cycles must elapse after either transition.
            time::udelay(200);
        }
        Ok(())
    }
}
impl ResetController for MssReset {
    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Aoss => "sc7180-aoss-mss",
            Kind::Pdc => "sc7180-pdc-modem",
        }
    }
    fn reset_cells(&self) -> usize {
        1
    }
    fn assert_reset(&self, spec: &[u32]) -> Result<(), &'static str> {
        self.set(spec, true)
    }
    fn deassert_reset(&self, spec: &[u32]) -> Result<(), &'static str> {
        self.set(spec, false)
    }
}

fn probe_kind(device: &PlatformDeviceInfo, kind: Kind) -> Result<(), &'static str> {
    let phandle = device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("qcom-mss-reset: missing phandle")?;
    let manager = DeviceManager::get_manager();
    if manager.get_reset_controller_by_phandle(phandle).is_some() {
        return Err("qcom-mss-reset: duplicate provider");
    }
    let resource = device
        .get_resources()
        .iter()
        .find(|resource| resource.res_type == PlatformDeviceResourceType::MEM)
        .ok_or("qcom-mss-reset: missing registers")?;
    let size = resource.size()?;
    let required = match kind {
        Kind::Aoss => 0x10004,
        Kind::Pdc => 0x104,
    };
    if size < required || resource.start % 4 != 0 {
        return Err("qcom-mss-reset: register window too small or unaligned");
    }
    let base = vm::ioremap(resource.start, size)?;
    manager.register_reset_controller(
        phandle,
        Arc::new(MssReset {
            base,
            kind,
            lock: IrqSpinLock::new(()),
        }),
    );
    Ok(())
}
fn probe_aoss(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    probe_kind(device, Kind::Aoss)
}
fn probe_pdc(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    probe_kind(device, Kind::Pdc)
}
fn remove(_: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Err("qcom-mss-reset: active provider is permanent")
}
fn register_driver() {
    let manager = DeviceManager::get_manager();
    manager.register_driver(
        Box::new(PlatformDeviceDriver::new(
            "sc7180-aoss-mss-reset",
            probe_aoss,
            remove,
            vec!["qcom,sc7180-aoss-cc"],
        )),
        DriverPriority::Critical,
    );
    manager.register_driver(
        Box::new(PlatformDeviceDriver::new(
            "sc7180-pdc-modem-reset",
            probe_pdc,
            remove,
            vec!["qcom,sc7180-pdc-global"],
        )),
        DriverPriority::Critical,
    );
}
scarlet::driver_initcall!(register_driver);
#[used]
static SCARLET_DRIVER_QCOM_MSS_RESET_ANCHOR: fn() = force_link;
pub fn force_link() {}
