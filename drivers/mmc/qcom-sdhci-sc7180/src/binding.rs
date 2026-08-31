//! Pure SC7180 SDHCI binding data and property helpers.

pub(crate) const REQUIRED_BUS_WIDTH: usize = 8;

// Qualcomm SDCC5 vendor registers are relative to the first ("hc") resource.
pub(crate) const CORE_DLL_CONFIG: usize = 0x200;
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
pub(crate) const REQUIRED_MMIO_SIZE: usize = CORE_MCI_DATA_CNT + core::mem::size_of::<u32>();
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

pub(crate) fn accepts_emmc_properties(non_removable: bool, bus_width: Option<usize>) -> bool {
    non_removable && bus_width == Some(REQUIRED_BUS_WIDTH)
}

pub(crate) const fn vendor_spec_for_fixed_1v8_supply(
    vendor_spec: u32,
    minimum_uv: u32,
    maximum_uv: u32,
) -> Option<u32> {
    if minimum_uv == EMMC_IO_VOLTAGE_UV && maximum_uv == EMMC_IO_VOLTAGE_UV {
        Some(vendor_spec | CORE_IO_PAD_PWR_SWITCH_MASK)
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
    fn accepts_only_non_removable_eight_bit_emmc() {
        assert!(accepts_emmc_properties(true, Some(8)));
        assert!(!accepts_emmc_properties(false, Some(8)));
        assert!(!accepts_emmc_properties(true, Some(4)));
        assert!(!accepts_emmc_properties(true, None));
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
    fn fixed_1v8_supply_selects_low_voltage_io_pads() {
        assert_eq!(
            vendor_spec_for_fixed_1v8_supply(
                CORE_VENDOR_SPEC_POR_VAL,
                EMMC_IO_VOLTAGE_UV,
                EMMC_IO_VOLTAGE_UV,
            ),
            Some(0x0001_8a9c)
        );
        assert_eq!(
            vendor_spec_for_fixed_1v8_supply(CORE_VENDOR_SPEC_POR_VAL, 1_700_000, 1_950_000),
            None
        );
        assert_eq!(
            vendor_spec_for_fixed_1v8_supply(CORE_VENDOR_SPEC_POR_VAL, 3_000_000, 3_000_000),
            None
        );
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
