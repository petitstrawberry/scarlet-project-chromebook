//! Pure SC7180 SDHCI binding data and property helpers.

pub(crate) const EMMC_BUS_WIDTH: usize = 8;
pub(crate) const SD_BUS_WIDTH: usize = 4;
pub(crate) const SD_VMMC_MIN_UV: u32 = 2_700_000;
pub(crate) const SD_VMMC_MAX_UV: u32 = 3_600_000;

// Qualcomm SDCC5 vendor registers are relative to the first ("hc") resource.
pub(crate) const CORE_DLL_CONFIG: usize = 0x200;
const CORE_CDR_EN: u32 = 1 << 17;
const CORE_CDR_EXT_EN: u32 = 1 << 19;

/// SDCC clock recovery follows ordinary reads, but must use external feedback
/// for writes, command-only operations, and tuning (Linux sdhci-msm.c).
pub(crate) fn command_cdr_config(config: u32, command: u8, read: bool) -> u32 {
    if read && !matches!(command, 19 | 21) {
        (config | CORE_CDR_EN) & !CORE_CDR_EXT_EN
    } else {
        (config & !CORE_CDR_EN) | CORE_CDR_EXT_EN
    }
}

#[cfg(test)]
mod cdr_tests {
    use super::*;

    #[test]
    fn feedback_tracks_command_direction_and_preserves_dll_phase() {
        let other = 0xa5f5_1234 & !(CORE_CDR_EN | CORE_CDR_EXT_EN);
        for previous in [
            0,
            CORE_CDR_EN,
            CORE_CDR_EXT_EN,
            CORE_CDR_EN | CORE_CDR_EXT_EN,
        ] {
            for (command, read, internal) in [
                (17, true, true),
                (18, true, true),
                (24, false, false),
                (25, false, false),
                (12, false, false),
                (13, false, false),
                (19, true, false),
                (21, true, false),
            ] {
                let expected = other
                    | if internal {
                        CORE_CDR_EN
                    } else {
                        CORE_CDR_EXT_EN
                    };
                assert_eq!(
                    command_cdr_config(other | previous, command, read),
                    expected
                );
            }
        }
    }
}
pub(crate) const CORE_DLL_RST: u32 = 1 << 30;
pub(crate) const CORE_DLL_PDN: u32 = 1 << 29;
pub(crate) const CORE_VENDOR_SPEC: usize = 0x20c;
pub(crate) const CORE_VENDOR_SPEC_POR_VAL: u32 = 0x0a9c;
pub(crate) const CORE_IO_PAD_PWR_SWITCH_EN: u32 = 1 << 15;
pub(crate) const CORE_IO_PAD_PWR_SWITCH: u32 = 1 << 16;
pub(crate) const CORE_IO_PAD_PWR_SWITCH_MASK: u32 =
    CORE_IO_PAD_PWR_SWITCH_EN | CORE_IO_PAD_PWR_SWITCH;
pub(crate) const EMMC_IO_VOLTAGE_UV: u32 = 1_800_000;
pub(crate) const CORE_PWRCTL_STATUS: usize = 0x240;
pub(crate) const CORE_PWRCTL_MASK: usize = 0x244;
pub(crate) const CORE_PWRCTL_CLEAR: usize = 0x248;
pub(crate) const CORE_PWRCTL_CTL: usize = 0x24c;
pub(crate) const CORE_PWRCTL_BUS_ON: u32 = 1 << 1;
pub(crate) const CORE_PWRCTL_BUS_SUCCESS: u32 = 1 << 0;
pub(crate) const CORE_PWRCTL_REQUEST_MASK: u32 = 0x0f;
pub(crate) const CORE_PWRCTL_INTERRUPTS_DISABLED: u32 = 0;
pub(crate) const CORE_MCI_VERSION: usize = 0x318;
pub(crate) const CORE_MCI_FIFO_CNT: usize = 0x308;
pub(crate) const CORE_MCI_STATUS: usize = 0x324;
pub(crate) const CORE_SDCC_DEBUG_REG: usize = 0x358;
pub(crate) const CORE_MCI_DATA_CNT: usize = 0x35c;
pub(crate) const HC_VENDOR_SPECIFIC_FUNC4: usize = 0x260;
pub(crate) const HC_DISABLE_CRYPTO: u32 = 1 << 15;
// SDCC v5.1 DLL user control at 0x388 is the highest accessed register.
pub(crate) const REQUIRED_MMIO_SIZE: usize = 0x388 + core::mem::size_of::<u32>();
pub(crate) const CQHCI_CFG: usize = 0x08;
pub(crate) const CQHCI_CTL: usize = 0x0c;
pub(crate) const CQHCI_ENABLE: u32 = 1 << 0;
pub(crate) const CQHCI_CRYPTO_GENERAL_ENABLE: u32 = 1 << 1;
pub(crate) const NONCQ_CRYPTO_PARM: usize = 0x70;
pub(crate) const REQUIRED_CQHCI_MMIO_SIZE: usize = NONCQ_CRYPTO_PARM + core::mem::size_of::<u32>();
pub(crate) const SDHCI_POWER_CONTROL: usize = 0x29;
const SDHCI_POWER_ON: u8 = 1 << 0;
const SDHCI_POWER_VOLTAGE_MASK: u8 = 0x0e;
const SDHCI_POWER_1V8: u8 = 0x0a;
const SDHCI_POWER_3V0: u8 = 0x0c;
const SDHCI_POWER_3V3: u8 = 0x0e;

/// Card protocol selected by the controller's firmware description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaKind {
    Emmc,
    Sd,
}

/// Classify an SC7180 SDCC node from its physical wiring properties.
///
/// CoachZ uses SDCC1 for soldered eight-bit eMMC and SDCC2 for a removable
/// four-bit SD slot with a card-detect GPIO. Rejecting other combinations
/// keeps unsupported wiring from being driven with a guessed bus width.
pub(crate) fn classify_media(
    non_removable: bool,
    bus_width: Option<usize>,
    has_card_detect: bool,
) -> Option<MediaKind> {
    match (non_removable, bus_width, has_card_detect) {
        (true, Some(EMMC_BUS_WIDTH), _) => Some(MediaKind::Emmc),
        (false, Some(SD_BUS_WIDTH), true) => Some(MediaKind::Sd),
        _ => None,
    }
}

pub(crate) const fn sd_supply_voltage_supported(minimum_uv: u32, maximum_uv: u32) -> bool {
    maximum_uv >= SD_VMMC_MIN_UV && minimum_uv <= SD_VMMC_MAX_UV
}

pub(crate) const fn vendor_spec_for_media(
    kind: MediaKind,
    vendor_spec: u32,
    minimum_uv: u32,
    maximum_uv: u32,
) -> Option<u32> {
    match kind {
        MediaKind::Emmc if minimum_uv == EMMC_IO_VOLTAGE_UV && maximum_uv == EMMC_IO_VOLTAGE_UV => {
            Some(vendor_spec | CORE_IO_PAD_PWR_SWITCH_MASK)
        }
        MediaKind::Sd if sd_supply_voltage_supported(minimum_uv, maximum_uv) => Some(vendor_spec),
        _ => None,
    }
}

/// Decode one two-cell GPIO specifier plus flags from a device-tree property.
pub(crate) fn gpio_spec(value: &[u8]) -> Option<(u32, u32, bool)> {
    let controller = u32::from_be_bytes(value.get(..4)?.try_into().ok()?);
    let pin = u32::from_be_bytes(value.get(4..8)?.try_into().ok()?);
    let flags = u32::from_be_bytes(value.get(8..12)?.try_into().ok()?);
    Some((controller, pin, flags & 1 != 0))
}

/// Select the SDHCI host aperture from the device's memory resources.
///
/// SDCC1 names two windows (`hc`, `cqhci`). SDCC2 describes its single host
/// window without `reg-names`, so the sole memory resource is unambiguous.
pub(crate) fn primary_memory_index(
    has_reg_names: bool,
    named_hc: Option<usize>,
    memory_count: usize,
) -> Option<usize> {
    if has_reg_names {
        named_hc.filter(|index| *index < memory_count)
    } else if memory_count == 1 {
        Some(0)
    } else {
        None
    }
}

pub(crate) const fn legacy_pio_cqhci_config(inherited: u32) -> u32 {
    inherited & !(CQHCI_ENABLE | CQHCI_CRYPTO_GENERAL_ENABLE)
}

pub(crate) const fn legacy_pio_func4(inherited: u32) -> u32 {
    inherited | HC_DISABLE_CRYPTO
}

pub(crate) fn string_list_index(value: &[u8], wanted: &str) -> Option<usize> {
    value
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .position(|entry| core::str::from_utf8(entry).ok() == Some(wanted))
}

pub(crate) const fn inherited_power_control_is_usable(value: u8) -> bool {
    value & SDHCI_POWER_ON != 0
        && matches!(
            value & SDHCI_POWER_VOLTAGE_MASK,
            SDHCI_POWER_1V8 | SDHCI_POWER_3V0 | SDHCI_POWER_3V3
        )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandoffPowerAction {
    None,
    AcknowledgeBusOn,
    Reject,
}

pub(crate) const fn handoff_power_action(requests: u32, power_control: u8) -> HandoffPowerAction {
    if !inherited_power_control_is_usable(power_control) {
        HandoffPowerAction::Reject
    } else if requests == 0 {
        HandoffPowerAction::None
    } else if requests == CORE_PWRCTL_BUS_ON {
        HandoffPowerAction::AcknowledgeBusOn
    } else {
        HandoffPowerAction::Reject
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_coachz_emmc_and_sd_wiring_only() {
        assert_eq!(classify_media(true, Some(8), false), Some(MediaKind::Emmc));
        assert_eq!(classify_media(false, Some(4), true), Some(MediaKind::Sd));
        assert_eq!(classify_media(false, Some(8), true), None);
        assert_eq!(classify_media(true, Some(4), true), None);
        assert_eq!(classify_media(false, Some(4), false), None);
        assert_eq!(classify_media(true, None, false), None);
    }

    #[test]
    fn vendor_offsets_fit_the_required_hc_window() {
        assert_eq!(CORE_DLL_CONFIG, 0x200);
        assert_eq!(CORE_DLL_RST, 1 << 30);
        assert_eq!(CORE_DLL_PDN, 1 << 29);
        assert_eq!(CORE_VENDOR_SPEC, 0x20c);
        assert_eq!(CORE_MCI_VERSION, 0x318);
        assert_eq!(CORE_MCI_FIFO_CNT, 0x308);
        assert_eq!(CORE_MCI_STATUS, 0x324);
        assert_eq!(CORE_SDCC_DEBUG_REG, 0x358);
        assert_eq!(CORE_MCI_DATA_CNT, 0x35c);
        assert_eq!(HC_VENDOR_SPECIFIC_FUNC4, 0x260);
        assert_eq!(HC_DISABLE_CRYPTO, 1 << 15);
        assert_eq!(CQHCI_CFG, 0x08);
        assert_eq!(CQHCI_CTL, 0x0c);
        assert_eq!(CQHCI_ENABLE, 1 << 0);
        assert_eq!(CQHCI_CRYPTO_GENERAL_ENABLE, 1 << 1);
        assert_eq!(NONCQ_CRYPTO_PARM, 0x70);
        assert_eq!(CORE_VENDOR_SPEC_POR_VAL, 0x0a9c);
        assert_eq!(CORE_IO_PAD_PWR_SWITCH_MASK, 0x0001_8000);
        assert_eq!(CORE_PWRCTL_STATUS, 0x240);
        assert_eq!(CORE_PWRCTL_MASK, 0x244);
        assert_eq!(CORE_PWRCTL_CLEAR, 0x248);
        assert_eq!(CORE_PWRCTL_CTL, 0x24c);
        assert_eq!(CORE_PWRCTL_BUS_SUCCESS, 1 << 0);
        assert_eq!(CORE_PWRCTL_REQUEST_MASK, 0x0f);
        assert_eq!(CORE_PWRCTL_INTERRUPTS_DISABLED, 0);
        assert_eq!(SDHCI_POWER_CONTROL, 0x29);
        assert!(REQUIRED_MMIO_SIZE <= 0x1000);
        assert!(REQUIRED_CQHCI_MMIO_SIZE <= 0x1000);
    }

    #[test]
    fn media_supply_selects_the_matching_io_pad_voltage() {
        assert_eq!(
            vendor_spec_for_media(
                MediaKind::Emmc,
                CORE_VENDOR_SPEC_POR_VAL,
                EMMC_IO_VOLTAGE_UV,
                EMMC_IO_VOLTAGE_UV,
            ),
            Some(0x0001_8a9c)
        );
        assert_eq!(
            vendor_spec_for_media(
                MediaKind::Emmc,
                CORE_VENDOR_SPEC_POR_VAL,
                1_700_000,
                1_950_000,
            ),
            None
        );
        assert_eq!(
            vendor_spec_for_media(
                MediaKind::Emmc,
                CORE_VENDOR_SPEC_POR_VAL,
                3_000_000,
                3_000_000,
            ),
            None
        );
        assert_eq!(
            vendor_spec_for_media(
                MediaKind::Sd,
                CORE_VENDOR_SPEC_POR_VAL,
                2_952_000,
                2_952_000,
            ),
            Some(CORE_VENDOR_SPEC_POR_VAL)
        );
        assert_eq!(
            vendor_spec_for_media(
                MediaKind::Sd,
                CORE_VENDOR_SPEC_POR_VAL,
                1_800_000,
                1_800_000,
            ),
            None
        );
    }

    #[test]
    fn decodes_active_low_card_detect_specifier() {
        let mut value = [0u8; 12];
        value[..4].copy_from_slice(&0x36u32.to_be_bytes());
        value[4..8].copy_from_slice(&69u32.to_be_bytes());
        value[8..12].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(gpio_spec(&value), Some((0x36, 69, true)));
        assert_eq!(gpio_spec(&value[..8]), None);
    }

    #[test]
    fn sd_single_unnamed_window_and_named_emmc_window_are_selected() {
        assert_eq!(primary_memory_index(false, None, 1), Some(0));
        assert_eq!(primary_memory_index(true, Some(0), 2), Some(0));
        assert_eq!(primary_memory_index(true, Some(1), 2), Some(1));
        assert_eq!(primary_memory_index(true, None, 2), None);
        assert_eq!(primary_memory_index(true, Some(2), 2), None);
        assert_eq!(primary_memory_index(false, Some(0), 2), None);
    }

    #[test]
    fn legacy_pio_disables_cqhci_and_inline_crypto() {
        assert_eq!(legacy_pio_cqhci_config(0xffff_ffff), 0xffff_fffc);
        assert_eq!(legacy_pio_cqhci_config(0), 0);
        assert_eq!(legacy_pio_func4(0), 0x0000_8000);
        assert_eq!(legacy_pio_func4(0x1234_0000), 0x1234_8000);
    }

    #[test]
    fn selects_named_power_irq_after_host_irq() {
        let names = b"hc_irq\0pwr_irq\0";
        assert_eq!(string_list_index(names, "hc_irq"), Some(0));
        assert_eq!(string_list_index(names, "pwr_irq"), Some(1));
        assert_eq!(string_list_index(names, "missing"), None);

        let registers = b"hc\0cqhci\0";
        assert_eq!(string_list_index(registers, "hc"), Some(0));
        assert_eq!(string_list_index(registers, "cqhci"), Some(1));
    }

    #[test]
    fn accepts_only_inherited_enabled_standard_voltages() {
        assert!(inherited_power_control_is_usable(0x0b));
        assert!(inherited_power_control_is_usable(0x0d));
        assert!(inherited_power_control_is_usable(0x0f));
        assert!(!inherited_power_control_is_usable(0x00));
        assert!(!inherited_power_control_is_usable(0x01));
        assert!(!inherited_power_control_is_usable(0x0a));
    }

    #[test]
    fn handoff_acknowledges_only_already_satisfied_bus_on() {
        assert_eq!(
            handoff_power_action(CORE_PWRCTL_BUS_ON, 0x0d),
            HandoffPowerAction::AcknowledgeBusOn
        );
        assert_eq!(handoff_power_action(0, 0x0d), HandoffPowerAction::None);
        assert_eq!(
            handoff_power_action(CORE_PWRCTL_BUS_ON, 0x0c),
            HandoffPowerAction::Reject
        );
        assert_eq!(handoff_power_action(0, 0x00), HandoffPowerAction::Reject);
        for rejected in [1, 4, 8, 3, 6, 10, 15] {
            assert_eq!(
                handoff_power_action(rejected, 0x0d),
                HandoffPowerAction::Reject
            );
        }
    }
}

/// Linux selects the three-quarter point of the longest circular valid run.
pub(crate) fn select_tuning_phase(valid: u16) -> Option<u8> {
    if valid == 0 {
        return None;
    }
    let mut best_start = 0;
    let mut best_len = 0;
    for start in 0..16 {
        if valid != u16::MAX && valid & (1 << ((start + 15) % 16)) != 0 {
            continue;
        }
        let len = (0..16)
            .take_while(|offset| valid & (1 << ((start + offset) % 16)) != 0)
            .count();
        if len > best_len {
            best_start = start;
            best_len = len;
        }
    }
    Some(((best_start + (best_len * 3 / 4).saturating_sub(1)) % 16) as u8)
}

#[cfg(test)]
mod tuning_tests {
    use super::select_tuning_phase;
    #[test]
    fn selects_within_wrapped_longest_window() {
        assert_eq!(select_tuning_phase(0), None);
        assert_eq!(select_tuning_phase(0xffff), Some(11));
        assert_eq!(select_tuning_phase(0xc007), Some(0));
        assert_eq!(select_tuning_phase(1 << 7), Some(7));
        for mask in 1..=u16::MAX {
            assert_ne!(mask & (1 << select_tuning_phase(mask).unwrap()), 0);
        }
    }
}
