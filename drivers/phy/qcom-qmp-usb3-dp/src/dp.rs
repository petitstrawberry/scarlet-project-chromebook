// SPDX-License-Identifier: GPL-2.0-only
//! SC7180 DP half of the QMP v3 combo PHY. Register sequences and electrical
//! tables follow ChromeOS Linux 6.6 cc1a33bf3fcee271d9f66ccd9fbbc7b06eacdabd,
//! drivers/phy/qualcomm/phy-qcom-qmp-combo.c and the QSERDES v3 headers.
use super::*;
use scarlet::device::phy::DisplayPortConfig;
const SERDES: usize = 0x2000;
const TX0: usize = 0x2200;
const TX1: usize = 0x2600;
const PHY: usize = 0x2a00;
const QMP_V3_DP_SERDES_TBL: &[(usize, u32)] = &[
    (0x164, 0x01),
    (0x080, 0x37),
    (0x03c, 0x02),
    (0x038, 0x0e),
    (0x040, 0x06),
    (0x138, 0x30),
    (0x15c, 0x02),
    (0x0b8, 0x00),
    (0x0d8, 0x3f),
    (0x0dc, 0x00),
    (0x0f0, 0x00),
    (0x0a0, 0x00),
    (0x00c, 0x0a),
    (0x148, 0x0a),
    (0x0ec, 0x00),
    (0x034, 0x3f),
    (0x154, 0x1f),
    (0x048, 0x07),
    (0x070, 0x36),
    (0x068, 0x16),
    (0x060, 0x06),
];
const QMP_V3_DP_SERDES_TBL_RBR: &[(usize, u32)] = &[
    (0x13c, 0x0c),
    (0x0b0, 0x69),
    (0x0bc, 0x80),
    (0x0c0, 0x07),
    (0x098, 0x6f),
    (0x09c, 0x08),
    (0x090, 0x00),
];
const QMP_V3_DP_SERDES_TBL_HBR: &[(usize, u32)] = &[
    (0x13c, 0x04),
    (0x0b0, 0x69),
    (0x0bc, 0x80),
    (0x0c0, 0x07),
    (0x098, 0x0f),
    (0x09c, 0x0e),
    (0x090, 0x00),
];
const QMP_V3_DP_SERDES_TBL_HBR2: &[(usize, u32)] = &[
    (0x13c, 0x00),
    (0x0b0, 0x8c),
    (0x0bc, 0x00),
    (0x0c0, 0x0a),
    (0x098, 0x1f),
    (0x09c, 0x1c),
    (0x090, 0x00),
];
const QMP_V3_DP_TX_TBL: &[(usize, u32)] = &[
    (0x05c, 0x1a),
    (0x0f0, 0x40),
    (0x028, 0x30),
    (0x034, 0x3d),
    (0x008, 0x0f),
    (0x024, 0x03),
    (0x0c0, 0x03),
    (0x068, 0x00),
    (0x0c4, 0x00),
    (0x02c, 0x4),
    (0x064, 0x0a),
    (0x01c, 0x38),
    (0x00c, 0x20),
    (0x044, 0x06),
    (0x048, 0x07),
];
const QMP_DP_V3_PRE_EMPHASIS_HBR3_HBR2: [[u8; 4]; 4] = [
    [0x00, 0x0c, 0x15, 0x1a],
    [0x02, 0x0e, 0x16, 0xff],
    [0x02, 0x11, 0xff, 0xff],
    [0x04, 0xff, 0xff, 0xff],
];
const QMP_DP_V3_VOLTAGE_SWING_HBR3_HBR2: [[u8; 4]; 4] = [
    [0x02, 0x12, 0x16, 0x1a],
    [0x09, 0x19, 0x1f, 0xff],
    [0x10, 0x1f, 0xff, 0xff],
    [0x1f, 0xff, 0xff, 0xff],
];
const QMP_DP_V3_PRE_EMPHASIS_HBR_RBR: [[u8; 4]; 4] = [
    [0x00, 0x0c, 0x14, 0x19],
    [0x00, 0x0b, 0x12, 0xff],
    [0x00, 0x0b, 0xff, 0xff],
    [0x04, 0xff, 0xff, 0xff],
];
const QMP_DP_V3_VOLTAGE_SWING_HBR_RBR: [[u8; 4]; 4] = [
    [0x08, 0x0f, 0x16, 0x1f],
    [0x11, 0x1e, 0x1f, 0xff],
    [0x19, 0x1f, 0xff, 0xff],
    [0x1f, 0xff, 0xff, 0xff],
];

pub(super) struct Sc7180DpPhy(pub(super) Arc<Sc7180QmpUsb3>);
impl Sc7180QmpUsb3 {
    fn dp_aux_init(&self) {
        let r = self.registers;
        r.write(PHY + QSERDES_DP_PHY_PD_CTL, 0x65);
        r.write(SERDES + QSERDES_V3_COM_BIAS_EN_CLKBUFLR_EN, 0x1b);
        r.write(PHY + QSERDES_DP_PHY_PD_CTL, 0x02);
        r.write(PHY + QSERDES_DP_PHY_PD_CTL, 0x7d);
        r.write(SERDES + QSERDES_V3_COM_BIAS_EN_CLKBUFLR_EN, 0x3f);
        for (index, value) in [0, 0x13, 0x24, 0, 0x0a, 0x26, 0x0a, 3, 0xbb, 3]
            .iter()
            .enumerate()
        {
            r.write(PHY + QSERDES_DP_PHY_AUX_CFG0 + index * 4, *value);
        }
        r.write(PHY + QSERDES_V3_DP_PHY_AUX_INTERRUPT_MASK, 0x1f);
    }
    fn dp_levels(&self, config: DisplayPortConfig) -> Result<(), PhyError> {
        let lanes = usize::from(config.lanes);
        let v = usize::from(*config.voltage[..lanes].iter().max().unwrap());
        let p = usize::from(*config.pre_emphasis[..lanes].iter().max().unwrap());
        if v > 3 || p > 3 || v + p > 3 {
            return Err(PhyError::InvalidMode);
        }
        let (swing, emphasis) = if config.link_rate_mbps <= 2700 {
            (
                QMP_DP_V3_VOLTAGE_SWING_HBR_RBR[v][p],
                QMP_DP_V3_PRE_EMPHASIS_HBR_RBR[v][p],
            )
        } else {
            (
                QMP_DP_V3_VOLTAGE_SWING_HBR3_HBR2[v][p],
                QMP_DP_V3_PRE_EMPHASIS_HBR3_HBR2[v][p],
            )
        };
        if swing == 255 || emphasis == 255 {
            return Err(PhyError::InvalidMode);
        }
        for tx in [TX0, TX1] {
            self.registers
                .write(tx + QSERDES_V3_TX_TX_DRV_LVL, u32::from(swing) | 0x20);
            self.registers.write(
                tx + QSERDES_V3_TX_TX_EMP_POST1_LVL,
                u32::from(emphasis) | 0x20,
            );
            self.registers.write(
                tx + QSERDES_V3_TX_HIGHZ_DRVR_EN,
                if lanes == 1 { 0x13 } else { 0x10 },
            );
            self.registers.write(
                tx + QSERDES_V3_TX_TRANSCEIVER_BIAS_EN,
                if lanes == 1 { 0x3e } else { 0x3f },
            );
        }
        Ok(())
    }
    fn dp_start(
        &self,
        config: DisplayPortConfig,
        orientation: PhyOrientation,
    ) -> Result<(), PhyError> {
        let r = self.registers;
        r.write_table(SERDES, QMP_V3_DP_SERDES_TBL);
        r.write_table(
            SERDES,
            match config.link_rate_mbps {
                1620 => QMP_V3_DP_SERDES_TBL_RBR,
                2700 => QMP_V3_DP_SERDES_TBL_HBR,
                5400 => QMP_V3_DP_SERDES_TBL_HBR2,
                _ => return Err(PhyError::InvalidMode),
            },
        );
        for tx in [TX0, TX1] {
            r.write_table(tx, QMP_V3_DP_TX_TBL);
        }
        self.dp_levels(config)?;
        let reverse = orientation == PhyOrientation::Reverse;
        let mut pd = 0x65;
        if config.lanes == 4 || reverse {
            pd |= 8;
        }
        if config.lanes == 4 || !reverse {
            pd |= 16;
        }
        r.write(PHY + QSERDES_DP_PHY_PD_CTL, pd);
        r.write(PHY + QSERDES_DP_PHY_MODE, if reverse { 0x4c } else { 0x5c });
        r.write(PHY + QSERDES_V3_DP_PHY_TX0_TX1_LANE_CTL, 5);
        r.write(PHY + QSERDES_V3_DP_PHY_TX2_TX3_LANE_CTL, 5);
        r.write(
            PHY + QSERDES_V3_DP_PHY_VCO_DIV,
            if config.link_rate_mbps == 5400 { 2 } else { 1 },
        );
        r.write(PHY + QSERDES_DP_PHY_AUX_CFG2, 4);
        for value in [1, 5, 1, 9] {
            r.write(PHY + QSERDES_DP_PHY_CFG, value);
        }
        r.write(SERDES + QSERDES_V3_COM_RESETSM_CNTRL, 0x20);
        self.dp_wait(SERDES + QSERDES_V3_COM_C_READY_STATUS, 1)?;
        r.write(PHY + QSERDES_DP_PHY_CFG, 0x19);
        self.dp_wait(PHY + QSERDES_V3_DP_PHY_STATUS, 2)?;
        r.write(PHY + QSERDES_DP_PHY_CFG, 0x18);
        time::udelay(2000);
        r.write(PHY + QSERDES_DP_PHY_CFG, 0x19);
        self.dp_wait(PHY + QSERDES_V3_DP_PHY_STATUS, 2)
    }
    fn dp_wait(&self, offset: usize, mask: u32) -> Result<(), PhyError> {
        let start = time::current_time();
        while self.registers.read(offset) & mask == 0 {
            if time::current_time().saturating_sub(start) >= 10_000 {
                return Err(PhyError::Timeout);
            }
            time::udelay(100);
        }
        Ok(())
    }
}
impl Phy for Sc7180DpPhy {
    fn name(&self) -> &'static str {
        "sc7180-qmp-dp"
    }
    fn reset(&self) -> Result<(), PhyError> {
        let state = self.0.state.lock();
        if state.dp_powered {
            return Err(PhyError::Busy);
        }
        self.0.registers.write(PHY + QSERDES_DP_PHY_PD_CTL, 2);
        Ok(())
    }
    fn get_mode(&self) -> Option<PhyMode> {
        Some(PhyMode::DisplayPort)
    }
    fn power_on(&self) -> Result<(), PhyError> {
        let mut state = self.0.state.lock();
        if state.dp_powered {
            return Ok(());
        }
        let common_new = !state.powered;
        if common_new {
            self.0.enable_common(state.orientation)?;
        }
        self.0.dp_aux_init();
        if let Err(e) = self.0.dp_start(state.dp_config, state.orientation) {
            self.0.registers.write(PHY + QSERDES_DP_PHY_PD_CTL, 2);
            if common_new {
                self.0.disable_common();
            }
            return Err(e);
        }
        state.dp_powered = true;
        Ok(())
    }
    fn power_off(&self) -> Result<(), PhyError> {
        let mut state = self.0.state.lock();
        if state.dp_powered {
            self.0.registers.write(PHY + QSERDES_DP_PHY_PD_CTL, 2);
            state.dp_powered = false;
            if !state.powered {
                self.0.disable_common();
            }
        }
        Ok(())
    }
    fn set_mode(&self, mode: PhyMode) -> Result<(), PhyError> {
        if mode == PhyMode::DisplayPort {
            Ok(())
        } else {
            Err(PhyError::InvalidMode)
        }
    }
    fn set_orientation(&self, orientation: PhyOrientation) -> Result<(), PhyError> {
        self.0.set_orientation(orientation)
    }
    fn configure_displayport(&self, config: DisplayPortConfig) -> Result<(), PhyError> {
        if !matches!(config.lanes, 1 | 2 | 4)
            || !matches!(config.link_rate_mbps, 1620 | 2700 | 5400)
            || config
                .voltage
                .iter()
                .zip(config.pre_emphasis)
                .any(|(&v, p)| v > 3 || p > 3 || v + p > 3)
        {
            return Err(PhyError::InvalidMode);
        }
        let mut state = self.0.state.lock();
        if state.dp_powered {
            if config.link_rate_mbps != state.dp_config.link_rate_mbps
                || config.lanes != state.dp_config.lanes
            {
                // Changing the PLL while it is running can leave C_READY set
                // for the old rate. Power down only DP before programming it;
                // the common block and live USB half retain their lifetime.
                self.0.registers.write(PHY + QSERDES_DP_PHY_PD_CTL, 2);
                self.0.dp_start(config, state.orientation)?;
            } else {
                self.0.dp_levels(config)?;
            }
        }
        state.dp_config = config;
        Ok(())
    }
}

const QSERDES_DP_PHY_AUX_CFG0: usize = 0x020;
const QSERDES_DP_PHY_AUX_CFG2: usize = 0x028;
const QSERDES_DP_PHY_CFG: usize = 0x010;
const QSERDES_DP_PHY_MODE: usize = 0x01c;
const QSERDES_DP_PHY_PD_CTL: usize = 0x018;
const QSERDES_V3_COM_BIAS_EN_CLKBUFLR_EN: usize = 0x034;
const QSERDES_V3_COM_C_READY_STATUS: usize = 0x158;
const QSERDES_V3_COM_RESETSM_CNTRL: usize = 0x088;
const QSERDES_V3_DP_PHY_AUX_INTERRUPT_MASK: usize = 0x048;
const QSERDES_V3_DP_PHY_STATUS: usize = 0x0c0;
const QSERDES_V3_DP_PHY_TX0_TX1_LANE_CTL: usize = 0x06c;
const QSERDES_V3_DP_PHY_TX2_TX3_LANE_CTL: usize = 0x088;
const QSERDES_V3_DP_PHY_VCO_DIV: usize = 0x064;
const QSERDES_V3_TX_HIGHZ_DRVR_EN: usize = 0x060;
const QSERDES_V3_TX_TRANSCEIVER_BIAS_EN: usize = 0x05c;
const QSERDES_V3_TX_TX_DRV_LVL: usize = 0x01c;
const QSERDES_V3_TX_TX_EMP_POST1_LVL: usize = 0x00c;
