// SPDX-License-Identifier: GPL-2.0-only
use super::{CrosEcError, CrosEcSpi, get_primary_cros_ec_spi, power_decode};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use scarlet::device::power_supply::{self, PowerSupply, PowerSupplyState, SupplyKind};

const BATTERY_DYNAMIC: u16 = 0x0601;
const READ_MEMMAP: u16 = 0x0007;
const USB_PD_POWER_INFO: u16 = 0x0103;
const CHARGE_PORT_COUNT: u16 = 0x0105;
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

fn external(
    mut command: impl FnMut(u16, &[u8]) -> Result<Vec<u8>, CrosEcError>,
) -> Result<PowerSupplyState, &'static str> {
    // Never use PD_POWER_CHARGING_PORT (0xff): older EC firmware maps it
    // to -1 when unplugged, then reports SINK because charge_port is also -1.
    // Query only real ports, as cros_usbpd-charger does in Linux.
    let ports = match command(CHARGE_PORT_COUNT, &[]) {
        Ok(bytes) if bytes.len() == 1 && bytes[0] <= 8 => bytes[0],
        Ok(_) => return Err("cros-ec: invalid charge port count"),
        Err(error) if unsupported(error) => return external_flags(&mut command),
        Err(_) => return Err("cros-ec: charge port count read failed"),
    };
    let mut present = false;
    for port in 0..ports {
        let bytes = match command(USB_PD_POWER_INFO, &[port]) {
            Ok(bytes) => bytes,
            Err(error) if unsupported(error) => return external_flags(&mut command),
            Err(_) => return Err("cros-ec: USB PD power read failed"),
        };
        let state = power_decode::usb_pd(&bytes)?;
        if state.online == Some(true) {
            return Ok(state);
        }
        present |= state.present == Some(true);
    }
    Ok(PowerSupplyState {
        present: Some(present),
        online: Some(false),
        ..Default::default()
    })
}

fn external_flags(
    command: &mut impl FnMut(u16, &[u8]) -> Result<Vec<u8>, CrosEcError>,
) -> Result<PowerSupplyState, &'static str> {
    let bytes =
        command(READ_MEMMAP, &[0x4c, 1]).map_err(|_| "cros-ec: external supply read failed")?;
    if bytes.len() != 1 {
        return Err("cros-ec: invalid supply flags");
    }
    Ok(power_decode::external_from_flags(bytes[0]))
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
        external(|command, payload| ec.command(command, 0, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn unplugged_and_source_ports_are_not_external_input() {
        for role in [0, 1, 3] {
            let mut queried = Vec::new();
            let state = external(|command, payload| {
                if command == CHARGE_PORT_COUNT {
                    assert!(payload.is_empty());
                    return Ok(vec![2]);
                }
                assert_eq!(command, USB_PD_POWER_INFO);
                queried.push(payload[0]);
                let mut bytes = vec![0; 16];
                bytes[0] = role;
                Ok(bytes)
            })
            .unwrap();
            assert_eq!(queried, [0, 1]);
            assert_eq!(state.online, Some(false));
            assert_eq!(state.present, Some(role != 0));
            assert_eq!(state.voltage_uv, None);
        }
    }

    #[test]
    fn finds_input_on_the_second_port() {
        let state = external(|command, payload| {
            if command == CHARGE_PORT_COUNT {
                return Ok(vec![2]);
            }
            assert_eq!(command, USB_PD_POWER_INFO);
            assert!(payload[0] < 2);
            let mut bytes = vec![0; 16];
            if payload[0] == 1 {
                bytes[0] = 2;
                bytes[6..8].copy_from_slice(&9000u16.to_le_bytes());
            }
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(state.online, Some(true));
        assert_eq!(state.voltage_uv, Some(9000000));
    }

    #[test]
    fn unsupported_commands_use_ac_flags_without_the_charging_port_sentinel() {
        for unsupported_command in [CHARGE_PORT_COUNT, USB_PD_POWER_INFO] {
            for flags in [0, 1] {
                let state = external(|command, payload| {
                    if command == unsupported_command {
                        return Err(CrosEcError::EcResult(1));
                    }
                    if command == CHARGE_PORT_COUNT {
                        return Ok(vec![2]);
                    }
                    assert_eq!(command, READ_MEMMAP);
                    assert_eq!(payload, [0x4c, 1]);
                    Ok(vec![flags])
                })
                .unwrap();
                assert_eq!(state.online, Some(flags == 1));
            }
        }
    }

    #[test]
    fn failed_or_invalid_queries_do_not_report_offline() {
        assert!(external(|_, _| Err(CrosEcError::NoResponse)).is_err());
        assert!(external(|_, _| Ok(vec![9])).is_err());
        assert!(
            external(|command, _| {
                if command == CHARGE_PORT_COUNT {
                    Ok(vec![2])
                } else {
                    Err(CrosEcError::NoResponse)
                }
            })
            .is_err()
        );
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
