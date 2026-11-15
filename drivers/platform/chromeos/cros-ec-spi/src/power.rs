// SPDX-License-Identifier: GPL-2.0-only
use super::{CrosEcError, CrosEcSpi, get_primary_cros_ec_spi, power_decode};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use scarlet::device::power_supply::{self, PowerSupply, PowerSupplyState, SupplyKind};

const BATTERY_DYNAMIC: u16 = 0x0601;
const READ_MEMMAP: u16 = 0x0007;
const USB_PD_POWER_INFO: u16 = 0x0103;
static REGISTERED: AtomicBool = AtomicBool::new(false);

fn unsupported(error: CrosEcError) -> bool {
    matches!(error, CrosEcError::EcResult(1 | 6))
}

fn battery(ec: &CrosEcSpi) -> Result<PowerSupplyState, &'static str> {
    match ec.command(BATTERY_DYNAMIC, 1, &[0]) {
        Ok(bytes) => return power_decode::dynamic(&bytes, 1),
        Err(error) if unsupported(error) => {}
        Err(_) => return Err("cros-ec: battery read failed"),
    }
    match ec.command(BATTERY_DYNAMIC, 0, &[0]) {
        Ok(bytes) => return power_decode::dynamic(&bytes, 0),
        Err(error) if unsupported(error) => {}
        Err(_) => return Err("cros-ec: battery read failed"),
    }
    let version = ec
        .command(READ_MEMMAP, 0, &[0x24, 1])
        .map_err(|_| "cros-ec: battery map version read failed")?;
    if version.len() != 1 || version[0] == 0 {
        return Err("cros-ec: battery memory map unavailable");
    }
    let bytes = ec
        .command(READ_MEMMAP, 0, &[0x40, 28])
        .map_err(|_| "cros-ec: battery map read failed")?;
    power_decode::memmap(&bytes)
}

struct EcSupply {
    battery: bool,
}
impl PowerSupply for EcSupply {
    fn name(&self) -> &'static str {
        if self.battery {
            "cros-ec-battery"
        } else {
            "cros-ec-usb"
        }
    }
    fn kind(&self) -> SupplyKind {
        if self.battery {
            SupplyKind::Battery
        } else {
            SupplyKind::Usb
        }
    }
    fn read(&self) -> Result<PowerSupplyState, &'static str> {
        let ec = get_primary_cros_ec_spi().ok_or("cros-ec: controller unavailable")?;
        if self.battery {
            return battery(&ec);
        }
        // 0xff queries the EC-selected charging port. No role or charger
        // policy is modified; a source-only connection is not input power.
        match ec.command(USB_PD_POWER_INFO, 0, &[0xff]) {
            Ok(bytes) => power_decode::usb_pd(&bytes),
            Err(error) if unsupported(error) => {
                let bytes = ec
                    .command(READ_MEMMAP, 0, &[0x4c, 1])
                    .map_err(|_| "cros-ec: external supply read failed")?;
                if bytes.len() != 1 {
                    return Err("cros-ec: invalid supply flags");
                }
                Ok(power_decode::external_from_flags(bytes[0]))
            }
            Err(_) => Err("cros-ec: USB PD power read failed"),
        }
    }
}

pub(super) fn register() {
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    for battery in [true, false] {
        if let Err(error) = power_supply::register(Arc::new(EcSupply { battery })) {
            scarlet::println!("[cros-ec-spi] power supply registration failed: {}", error);
        }
    }
}
