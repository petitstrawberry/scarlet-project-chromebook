// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 QMP v3 USB3 register layout and initialization data.
//!
//! Values are adapted from ChromeOS Linux 6.6
//! `drivers/phy/qualcomm/phy-qcom-qmp-combo.c` and its QMP v3 headers.

pub(super) const REGISTER_WINDOW_SIZE: usize = 0x3000;

pub(super) const COM_BASE: usize = 0x0000;
pub(super) const USB3_SERDES_BASE: usize = 0x1000;
pub(super) const TXA_BASE: usize = 0x1200;
pub(super) const RXA_BASE: usize = 0x1400;
pub(super) const TXB_BASE: usize = 0x1600;
pub(super) const RXB_BASE: usize = 0x1800;
pub(super) const USB3_PCS_BASE: usize = 0x1c00;

pub(super) const DP_COM_PHY_MODE_CTRL: usize = COM_BASE;
pub(super) const DP_COM_SW_RESET: usize = COM_BASE + 0x04;
pub(super) const DP_COM_POWER_DOWN_CTRL: usize = COM_BASE + 0x08;
pub(super) const DP_COM_SWI_CTRL: usize = COM_BASE + 0x0c;
pub(super) const DP_COM_TYPEC_CTRL: usize = COM_BASE + 0x10;
pub(super) const DP_COM_RESET_OVRD_CTRL: usize = COM_BASE + 0x1c;

pub(super) const PCS_SW_RESET: usize = USB3_PCS_BASE;
pub(super) const PCS_POWER_DOWN_CONTROL: usize = USB3_PCS_BASE + 0x004;
pub(super) const PCS_START_CONTROL: usize = USB3_PCS_BASE + 0x008;
pub(super) const PCS_STATUS: usize = USB3_PCS_BASE + 0x174;

pub(super) const SERDES_CMN_STATUS: usize = USB3_SERDES_BASE + 0x124;
pub(super) const SERDES_RESET_SM_STATUS: usize = USB3_SERDES_BASE + 0x128;
pub(super) const SERDES_C_READY_STATUS: usize = USB3_SERDES_BASE + 0x158;
pub(super) const SERDES_PLL_IVCO: usize = USB3_SERDES_BASE + 0x048;
pub(super) const TXA_HIGHZ_DRVR_EN: usize = TXA_BASE + 0x060;
pub(super) const RXA_UCDR_FASTLOCK_FO_GAIN: usize = RXA_BASE + 0x030;
pub(super) const PCS_FLL_CNTRL2: usize = USB3_PCS_BASE + 0x0c8;

pub(super) const SW_RESET: u32 = 1;
pub(super) const SW_POWER_DOWN: u32 = 1;
pub(super) const SERDES_START: u32 = 1;
pub(super) const PCS_START: u32 = 1 << 1;
pub(super) const PHY_STATUS: u32 = 1 << 6;

pub(super) const DP_RESET_OVERRIDE: u32 = 1 << 0 | 1 << 1;
pub(super) const USB3_RESET_OVERRIDE: u32 = 1 << 2 | 1 << 3;
pub(super) const USB3_AND_DP_MODE: u32 = 1 << 0 | 1 << 1;
pub(super) const SOFTWARE_PORT_SELECT_VALUE: u32 = 1 << 0;
pub(super) const SOFTWARE_PORT_SELECT_MUX: u32 = 1 << 1;

pub(super) const USB3_SERDES_TABLE: &[(usize, u32)] = &[
    (0x048, 0x07),
    (0x080, 0x14),
    (0x034, 0x08),
    (0x138, 0x30),
    (0x03c, 0x02),
    (0x08c, 0x08),
    (0x15c, 0x16),
    (0x164, 0x01),
    (0x13c, 0x80),
    (0x0b0, 0x82),
    (0x0b8, 0xab),
    (0x0bc, 0xea),
    (0x0c0, 0x02),
    (0x060, 0x06),
    (0x068, 0x16),
    (0x070, 0x36),
    (0x0dc, 0x00),
    (0x0d8, 0x3f),
    (0x0f8, 0x01),
    (0x0f4, 0xc9),
    (0x148, 0x0a),
    (0x0a0, 0x00),
    (0x09c, 0x34),
    (0x098, 0x15),
    (0x090, 0x04),
    (0x154, 0x00),
    (0x094, 0x00),
    (0x0f0, 0x00),
    (0x040, 0x0a),
    (0x010, 0x01),
    (0x01c, 0x31),
    (0x020, 0x01),
    (0x014, 0x00),
    (0x018, 0x00),
    (0x024, 0x85),
    (0x028, 0x07),
];

pub(super) const USB3_TX_TABLE: &[(usize, u32)] = &[
    (0x060, 0x10),
    (0x0a4, 0x12),
    (0x08c, 0x16),
    (0x048, 0x09),
    (0x044, 0x06),
];

pub(super) const USB3_RX_TABLE: &[(usize, u32)] = &[
    (0x030, 0x0b),
    (0x0d4, 0x0f),
    (0x0d8, 0x4e),
    (0x0dc, 0x18),
    (0x0f8, 0x77),
    (0x0fc, 0x80),
    (0x104, 0x03),
    (0x10c, 0x16),
    (0x034, 0x75),
];

pub(super) const USB3_PCS_TABLE: &[(usize, u32)] = &[
    (0x0c8, 0x83),
    (0x0cc, 0x09),
    (0x0d0, 0xa2),
    (0x0d4, 0x40),
    (0x0c4, 0x02),
    (0x080, 0xd1),
    (0x084, 0x1f),
    (0x088, 0x47),
    (0x064, 0x1b),
    (0x1d8, 0xba),
    (0x00c, 0x9f),
    (0x010, 0x9f),
    (0x014, 0xb7),
    (0x018, 0x4e),
    (0x01c, 0x65),
    (0x020, 0x6b),
    (0x024, 0x15),
    (0x028, 0x0d),
    (0x02c, 0x15),
    (0x030, 0x0d),
    (0x034, 0x15),
    (0x038, 0x0d),
    (0x03c, 0x15),
    (0x040, 0x1d),
    (0x044, 0x15),
    (0x048, 0x0d),
    (0x04c, 0x15),
    (0x050, 0x0d),
    (0x05c, 0x02),
    (0x0a0, 0x04),
    (0x08c, 0x44),
    (0x0a0, 0x04),
    (0x070, 0xe7),
    (0x074, 0x03),
    (0x078, 0x40),
    (0x07c, 0x00),
    (0x0b8, 0x75),
    (0x0b0, 0x86),
    (0x0bc, 0x13),
];
