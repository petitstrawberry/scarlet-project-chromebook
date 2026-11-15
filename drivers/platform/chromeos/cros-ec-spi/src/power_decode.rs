// SPDX-License-Identifier: GPL-2.0-only
//! Read-only Chromium EC host-command responses. Units and field offsets follow
//! ec_commands.h at ab6941cd8b5b973152d6ad947daa43160d8e9d2b.
use scarlet::device::power_supply::{ChargeState, PowerSupplyState};

const AC_PRESENT: u16 = 1;
const BATT_PRESENT: u16 = 2;
const DISCHARGING: u16 = 4;
const CHARGING: u16 = 8;
const INVALID_DATA: u16 = 0x20;

fn battery_state(flags: u16) -> PowerSupplyState {
    let present = flags & BATT_PRESENT != 0;
    PowerSupplyState {
        present: Some(present),
        // EC has no "fully charged" bit here. Idle can also mean a charge
        // threshold or a disconnected charger, so never infer Full from idle.
        charge_state: (present && flags & INVALID_DATA == 0)
            .then(|| match flags & 12 {
                CHARGING => Some(ChargeState::Charging),
                DISCHARGING => Some(ChargeState::Discharging),
                0 => Some(ChargeState::NotCharging),
                _ => None,
            })
            .flatten(),
        ..Default::default()
    }
}

fn capacity(remaining: u32, full: u32) -> Option<u32> {
    if full == 0 || full == u32::MAX || remaining == u32::MAX || remaining > full {
        return None;
    }
    Some((u64::from(remaining) * 1000 / u64::from(full)) as u32)
}

pub(crate) fn dynamic(bytes: &[u8], version: u8) -> Result<PowerSupplyState, &'static str> {
    if !matches!(version, 0 | 1) || bytes.len() != if version == 1 { 16 } else { 14 } {
        return Err("cros-ec: invalid battery response length");
    }
    let word = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    let flags = word(8);
    let mut state = battery_state(flags);
    if flags & (BATT_PRESENT | INVALID_DATA) != BATT_PRESENT {
        return Ok(state);
    }
    let voltage = word(0) as i16;
    state.voltage_uv = (voltage > 0).then(|| voltage as u32 * 1000);
    state.current_ua = Some(i32::from(word(2) as i16) * 1000);
    let remaining = word(4) as i16;
    let full = word(6) as i16;
    if remaining >= 0 && full > 0 {
        state.capacity_permille = capacity(remaining as u32, full as u32);
    }
    if version == 1 {
        let temperature = word(14);
        if temperature != 0 && temperature != u16::MAX {
            state.temperature_mc = Some(i32::from(temperature) * 100 - 273_150);
        }
    }
    Ok(state)
}

/// Legacy READ_MEMMAP range 0x40..0x5c, fetched in one host command. The
/// current is an unsigned magnitude, requiring an unambiguous direction bit.
pub(crate) fn memmap(bytes: &[u8]) -> Result<PowerSupplyState, &'static str> {
    if bytes.len() != 28 {
        return Err("cros-ec: invalid battery memory map length");
    }
    let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    let flags = u16::from(bytes[12]);
    let mut state = battery_state(flags);
    if flags & (BATT_PRESENT | INVALID_DATA) != BATT_PRESENT {
        return Ok(state);
    }
    state.voltage_uv = (word(0) != 0).then(|| word(0).checked_mul(1000)).flatten();
    state.capacity_permille = capacity(word(8), word(24));
    if let Some(magnitude) = word(4)
        .checked_mul(1000)
        .and_then(|v| i32::try_from(v).ok())
    {
        state.current_ua = match flags & 12 {
            CHARGING => Some(magnitude),
            DISCHARGING => Some(-magnitude),
            _ if magnitude == 0 => Some(0),
            _ => None,
        };
    }
    Ok(state)
}

/// USB_PD_POWER_INFO v0. Only SINK supplies power to the AP; a connected
/// SOURCE or SINK_NOT_CHARGING port must not be reported online.
pub(crate) fn usb_pd(bytes: &[u8]) -> Result<PowerSupplyState, &'static str> {
    if bytes.len() != 16 || bytes[0] > 3 {
        return Err("cros-ec: invalid USB PD power response");
    }
    let word = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
    Ok(PowerSupplyState {
        present: Some(bytes[0] != 0),
        online: Some(bytes[0] == 2),
        voltage_uv: (bytes[0] != 0 && word(6) != 0).then(|| u32::from(word(6)) * 1000),
        // current_max is a negotiated ceiling; current_lim is the configured
        // input limit. Neither is an actual input-current measurement.
        input_current_limit_ua: (bytes[0] == 2).then(|| u32::from(word(10)) * 1000),
        ..Default::default()
    })
}

pub(crate) fn external_from_flags(flags: u8) -> PowerSupplyState {
    PowerSupplyState {
        online: Some(u16::from(flags) & AC_PRESENT != 0),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(flags: u16) -> [u8; 16] {
        let mut b = [0; 16];
        for (offset, value) in [
            (0, 7600u16),
            (2, (-250i16) as u16),
            (4, 1500),
            (6, 6000),
            (8, flags),
            (14, 2981),
        ] {
            b[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        b
    }
    #[test]
    fn decodes_signed_current_capacity_and_temperature() {
        let state = dynamic(&sample(BATT_PRESENT | DISCHARGING), 1).unwrap();
        assert_eq!(state.capacity_permille, Some(250));
        assert_eq!(state.current_ua, Some(-250000));
        assert_eq!(state.voltage_uv, Some(7600000));
        assert_eq!(state.temperature_mc, Some(24950));
        assert_eq!(state.charge_state, Some(ChargeState::Discharging));
        assert_eq!(dynamic(&sample(2)[..14], 0).unwrap().temperature_mc, None);
        assert!(dynamic(&sample(2)[..15], 1).is_err());
    }
    #[test]
    fn invalid_or_absent_battery_does_not_publish_stale_values() {
        for flags in [0, AC_PRESENT, BATT_PRESENT | INVALID_DATA] {
            let state = dynamic(&sample(flags), 1).unwrap();
            assert_eq!(state.capacity_permille, None);
            assert_eq!(state.current_ua, None);
            assert_eq!(state.charge_state, None);
            assert_eq!(state.temperature_mc, None);
        }
        assert_eq!(
            dynamic(&sample(2), 1).unwrap().charge_state,
            Some(ChargeState::NotCharging)
        );
        assert_eq!(capacity(500, 0), None);
        assert_eq!(capacity(500, 400), None);
        assert_eq!(capacity(0, 400), Some(0));
    }
    #[test]
    fn legacy_unknown_values_and_ambiguous_direction_remain_absent() {
        let mut b = [255; 28];
        b[12] = 2;
        assert_eq!(memmap(&b).unwrap().voltage_uv, None);
        assert_eq!(memmap(&b).unwrap().capacity_permille, None);
        assert_eq!(memmap(&b).unwrap().current_ua, None);
        b[4..8].copy_from_slice(&250u32.to_le_bytes());
        assert_eq!(memmap(&b).unwrap().current_ua, None);
        b[12] |= 4;
        assert_eq!(memmap(&b).unwrap().current_ua, Some(-250000));
        assert!(memmap(&b[..27]).is_err());
    }
    #[test]
    fn usb_supply_distinguishes_connection_from_input_power() {
        let mut b = [0; 16];
        b[6..8].copy_from_slice(&9000u16.to_le_bytes());
        b[10..12].copy_from_slice(&2000u16.to_le_bytes());
        for role in 0..=3 {
            b[0] = role;
            let s = usb_pd(&b).unwrap();
            assert_eq!(s.online, Some(role == 2));
            assert_eq!(s.present, Some(role != 0));
            assert_eq!(s.current_ua, None);
            assert_eq!(s.input_current_limit_ua, (role == 2).then_some(2000000));
        }
        b[0] = 4;
        assert!(usb_pd(&b).is_err());
    }
}
