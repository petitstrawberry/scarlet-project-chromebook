// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 10 nm DSI D-PHY and QLink PLL.

use scarlet::time;
use scarlet_driver_ti_sn65dsi86::DisplayTiming;

use crate::registers::RegisterWindow;

const PHY_BASE: usize = 0x94400;
const PLL_BASE: usize = 0x94a00;

const COMMON_CLOCK_CONFIG0: usize = 0x10;
const COMMON_CLOCK_CONFIG1: usize = 0x14;
const COMMON_GLOBAL_CONTROL: usize = 0x18;
const COMMON_RESYNC_BUFFER_CONTROL: usize = 0x1c;
const COMMON_VREG_CONTROL: usize = 0x20;
const COMMON_CONTROL0: usize = 0x24;
const COMMON_CONTROL2: usize = 0x2c;
const COMMON_LANE_CONFIG0: usize = 0x30;
const COMMON_LANE_CONFIG1: usize = 0x34;
const COMMON_PLL_CONTROL: usize = 0x38;
const COMMON_LANE_CONTROL0: usize = 0x98;
const COMMON_TIMING_CONTROL0: usize = 0xac;
const COMMON_PHY_STATUS: usize = 0xec;

const LANE_BASE: usize = 0x200;
const LANE_STRIDE: usize = 0x80;
const LANE_CONFIG0: usize = 0x00;
const LANE_CONFIG1: usize = 0x04;
const LANE_CONFIG2: usize = 0x08;
const LANE_CONFIG3: usize = 0x0c;
const LANE_PIN_SWAP: usize = 0x14;
const LANE_HS_TX_STRENGTH: usize = 0x18;
const LANE_OFFSET_TOP: usize = 0x1c;
const LANE_OFFSET_BOTTOM: usize = 0x20;
const LANE_LP_TX_STRENGTH: usize = 0x24;
const LANE_LP_RX_CONTROL: usize = 0x28;
const LANE_TX_CONTROL: usize = 0x2c;

const PLL_SYSTEM_MUXES: usize = 0x24;
const PLL_OUTPUT_DIVIDER_RATE: usize = 0x140;
const PLL_COMMON_STATUS_ONE: usize = 0x1a0;

const REFERENCE_GENERATOR_TIMEOUT_US: u64 = 15_000;
const PLL_LOCK_TIMEOUT_US: u64 = 15_000;
const MINIMUM_VCO_HZ: u64 = 1_000_000_000;

const DIVIDERS: [(u32, u32); 19] = [
    (2, 11),
    (4, 5),
    (2, 9),
    (8, 2),
    (1, 15),
    (2, 7),
    (1, 13),
    (4, 3),
    (1, 11),
    (2, 5),
    (1, 9),
    (8, 1),
    (1, 7),
    (2, 3),
    (1, 5),
    (4, 1),
    (1, 3),
    (2, 1),
    (1, 1),
];

struct PhyConfiguration {
    desired_bit_clock_hz: u64,
    bits_per_pixel: u32,
    data_lanes: u32,
    pixel_dividend: u32,
    pixel_divisor: u32,
    phy_post_divider: u32,
    pll_post_divider: u32,
}

#[derive(Default)]
struct PhyTimings {
    half_byte_clock: i64,
    clock_zero: i64,
    clock_prepare: i64,
    clock_trail: i64,
    hs_exit: i64,
    hs_zero: i64,
    hs_prepare: i64,
    hs_trail: i64,
    hs_request: i64,
    clock_post: i64,
    clock_pre: i64,
    clock_pre_double: i64,
    turnaround_go: i64,
    turnaround_sure: i64,
    turnaround_get: i64,
}

struct PllRate {
    proportional_gain: u32,
    decimal: u32,
    fraction_low: u32,
    fraction_mid: u32,
    fraction_high: u32,
    clock_inverters: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct DsiHostClockTimings {
    pub(crate) clock_pre: u32,
    pub(crate) clock_post: u32,
    pub(crate) clock_pre_double: u32,
}

pub(crate) struct DsiPhy {
    mdss: RegisterWindow,
}

impl DsiPhy {
    pub(crate) const fn new(mdss: RegisterWindow) -> Self {
        Self { mdss }
    }

    fn phy_read(&self, offset: usize) -> u32 {
        self.mdss.read(PHY_BASE + offset)
    }

    fn phy_write(&self, offset: usize, value: u32) {
        self.mdss.write(PHY_BASE + offset, value)
    }

    fn pll_write(&self, offset: usize, value: u32) {
        self.mdss.write(PLL_BASE + offset, value)
    }

    fn wait_for_bit(&self, offset: usize, mask: u32, timeout_us: u64) -> Result<(), &'static str> {
        let start = time::current_time();
        while self.mdss.read(offset) & mask == 0 {
            if time::current_time().saturating_sub(start) >= timeout_us {
                return Err("qcom-sc7180-mdss: DSI PHY timeout");
            }
            time::udelay(100);
        }
        Ok(())
    }

    fn prepare_common_block(&self) -> Result<(), &'static str> {
        self.wait_for_bit(
            PHY_BASE + COMMON_PHY_STATUS,
            1,
            REFERENCE_GENERATOR_TIMEOUT_US,
        )?;

        self.phy_write(COMMON_CONTROL0, 0x60);
        self.phy_write(COMMON_PLL_CONTROL, 0);
        self.phy_write(COMMON_RESYNC_BUFFER_CONTROL, 0);
        // A standalone SC7180 PHY uses its internal PLL. Clear the inherited
        // pixel-clock mux and global-clock enable before reprogramming it.
        self.phy_write(COMMON_CLOCK_CONFIG1, 0);
        self.phy_write(COMMON_GLOBAL_CONTROL, 0x10);
        self.phy_write(COMMON_VREG_CONTROL, 0x59);
        // ChromeOS Linux programs the fixed logical-to-physical lane map while
        // the lanes are still powered down.
        self.phy_write(COMMON_LANE_CONFIG0, 0x21);
        self.phy_write(COMMON_LANE_CONFIG1, 0x84);

        Ok(())
    }

    fn enable_lanes(&self) {
        // Match dsi_10nm_phy_enable(): timings are committed before removing
        // power-down from the common block and lanes.
        self.phy_write(COMMON_CONTROL0, 0x7f);
        self.phy_write(COMMON_LANE_CONTROL0, 0x1f);
        self.phy_write(COMMON_CONTROL2, 0x40);

        self.configure_lanes();

        // SC7180 uses the new 10 nm timing sequence. Release the lane-3 I/O
        // freeze only after its normal drive-control value has been written.
        let lane3_tx = LANE_BASE + 3 * LANE_STRIDE + LANE_TX_CONTROL;
        self.phy_write(lane3_tx, 0x05);
        self.phy_write(lane3_tx, 0x04);
    }

    fn configure_lanes(&self) {
        // Keep the two-pass ordering used by the Linux 10 nm PHY driver:
        // establish electrical strength and disable every LP receiver first,
        // then enable only logical data lane 0 and program lane functions.
        for lane in 0..5 {
            let base = LANE_BASE + lane * LANE_STRIDE;
            self.phy_write(base + LANE_LP_TX_STRENGTH, 0x55);
            self.phy_write(base + LANE_LP_RX_CONTROL, 0);
            self.phy_write(base + LANE_PIN_SWAP, 0);
            self.phy_write(base + LANE_HS_TX_STRENGTH, 0x88);
        }
        self.phy_write(LANE_BASE + LANE_LP_RX_CONTROL, 3);

        for lane in 0..5 {
            let base = LANE_BASE + lane * LANE_STRIDE;
            let is_clock = lane == 4;
            self.phy_write(base + LANE_CONFIG0, 0);
            self.phy_write(base + LANE_CONFIG1, 0);
            self.phy_write(base + LANE_CONFIG2, 0);
            self.phy_write(base + LANE_CONFIG3, if is_clock { 0x80 } else { 0 });
            self.phy_write(base + LANE_OFFSET_TOP, 0);
            self.phy_write(base + LANE_OFFSET_BOTTOM, 0);
            let tx_control = if lane == 3 { 0x04 } else { u32::from(is_clock) };
            self.phy_write(base + LANE_TX_CONTROL, tx_control);
        }
    }

    fn calculate_dividers(&self, config: &mut PhyConfiguration) -> Result<u64, &'static str> {
        config.pixel_dividend = 1;
        config.pixel_divisor = 1;

        let (pll_post, phy_post) = DIVIDERS
            .iter()
            .rev()
            .copied()
            .find(|(pll, phy)| {
                config.desired_bit_clock_hz * u64::from(*pll) * u64::from(*phy) > MINIMUM_VCO_HZ
            })
            .ok_or("qcom-sc7180-mdss: DSI bit clock is below PLL range")?;
        config.pll_post_divider = pll_post;
        config.phy_post_divider = phy_post;

        let dsi_divider = config.pixel_dividend * config.bits_per_pixel
            / (config.pixel_divisor * config.data_lanes * 2);
        self.phy_write(
            COMMON_CLOCK_CONFIG0,
            ((dsi_divider << 4) & 0xf0) | (phy_post & 0x0f),
        );
        Ok(config.desired_bit_clock_hz * u64::from(phy_post) * u64::from(pll_post))
    }

    fn initialize_pll_registers(&self) {
        const VALUES: &[(usize, u32)] = &[
            (0xa8, 0x10),
            (0x08, 0x3f),
            (0x0c, 0x00),
            (0x14, 0x00),
            (0x18, 0x80),
            (0x28, 0x00),
            (0x34, 0x00),
            (0x38, 0x02),
            (0x3c, 0x82),
            (0x40, 0x00),
            (0x44, 0xff),
            (0x48, 0x00),
            (0x4c, 0x00),
            (0x50, 0x25),
            (0x58, 0x4f),
            (0x5c, 0x0a),
            (0x60, 0x00),
            (0x84, 0x42),
            (0x88, 0x00),
            (0x8c, 0x00),
            (0x90, 0x30),
            (0x98, 0x04),
            (0x9c, 0x00),
            (0xa0, 0x00),
            (0xac, 0x01),
            (0xb0, 0x08),
            (0xc8, 0x00),
            (0xec, 0x03),
            (0x108, 0x00),
            (0x13c, 0x00),
            (0x16c, 0x03),
            (0x17c, 0x00),
            (0x188, 0x19),
            (0x190, 0x00),
            (0x194, 0x40),
            (0x198, 0x20),
            (0x19c, 0x00),
        ];
        for &(offset, value) in VALUES {
            self.pll_write(offset, value);
        }
    }

    fn calculate_pll_rate(vco_hz: u64) -> PllRate {
        const FRACTION_BITS: u32 = 18;
        const REFERENCE_HZ: u64 = 19_200_000;

        let multiplier = 1u64 << FRACTION_BITS;
        let fixed_divider = vco_hz * multiplier / (REFERENCE_HZ * 2);
        let fraction = fixed_divider % multiplier;
        PllRate {
            proportional_gain: if vco_hz <= 1_900_000_000 {
                8
            } else if vco_hz <= 3_000_000_000 {
                10
            } else {
                12
            },
            decimal: (fixed_divider / multiplier) as u32,
            fraction_low: (fraction & 0xff) as u32,
            fraction_mid: ((fraction >> 8) & 0xff) as u32,
            fraction_high: ((fraction >> 16) & 0x3) as u32,
            clock_inverters: if vco_hz < 1_100_000_000 { 8 } else { 0 },
        }
    }

    fn program_pll_rate(&self, rate: &PllRate) {
        self.pll_write(0xa8, 0x12);
        self.pll_write(0xcc, rate.decimal);
        self.pll_write(0xd0, rate.fraction_low);
        self.pll_write(0xd4, rate.fraction_mid);
        self.pll_write(0xd8, rate.fraction_high);
        self.pll_write(0x144, 0x40);
        self.pll_write(0x184, 0x06);
        self.pll_write(0x2c, 0x10);
        self.pll_write(0x18c, rate.clock_inverters);

        const INDEPENDENT_VALUES: &[(usize, u32)] = &[
            (0x00, 0x80),
            (0x04, 0x03),
            (0x10, 0x00),
            (0x1c, 0x00),
            (0x20, 0x4e),
            (0x30, 0x40),
            (0x54, 0xba),
            (0x64, 0x0c),
            (0x94, 0x00),
            (0xa4, 0x00),
            (0xb4, 0x08),
            (0x154, 0xc0),
            (0x15c, 0xfa),
            (0x164, 0x4c),
            (0x180, 0x80),
            (0x7c, 0x29),
            (0x80, 0x3f),
        ];
        for &(offset, value) in INDEPENDENT_VALUES {
            self.pll_write(offset, value);
        }
        self.pll_write(0x14c, rate.proportional_gain);
    }

    fn start_pll(&self, config: &PhyConfiguration, vco_hz: u64) -> Result<(), &'static str> {
        self.pll_write(PLL_SYSTEM_MUXES, 0xc0);
        self.phy_write(
            COMMON_CLOCK_CONFIG1,
            (self.phy_read(COMMON_CLOCK_CONFIG1) & !0x03) | 1,
        );
        self.pll_write(
            PLL_OUTPUT_DIVIDER_RATE,
            config.pll_post_divider.trailing_zeros() & 0x3,
        );
        self.initialize_pll_registers();
        self.program_pll_rate(&Self::calculate_pll_rate(vco_hz));

        self.phy_write(COMMON_PLL_CONTROL, 1);
        self.wait_for_bit(PLL_BASE + PLL_COMMON_STATUS_ONE, 1, PLL_LOCK_TIMEOUT_US)?;
        self.phy_write(
            COMMON_CLOCK_CONFIG1,
            self.phy_read(COMMON_CLOCK_CONFIG1) | 0x20,
        );
        self.phy_write(COMMON_RESYNC_BUFFER_CONTROL, 1);
        Ok(())
    }

    fn signed_div_ceil(numerator: i64, denominator: i64) -> i64 {
        if numerator >= 0 {
            (numerator + denominator - 1) / denominator
        } else {
            (numerator - denominator + 1) / denominator
        }
    }

    fn interpolate(maximum: i64, minimum: i64, percent: i64, floor: i64) -> i64 {
        let value = Self::signed_div_ceil((maximum - minimum) * percent, 100) + minimum;
        value.max(floor)
    }

    fn calculate_timings(bit_rate_hz: u64) -> Result<PhyTimings, &'static str> {
        let bit_rate =
            i64::try_from(bit_rate_hz).map_err(|_| "qcom-sc7180-mdss: DSI bit rate overflow")?;
        if bit_rate < 1_000 {
            return Err("qcom-sc7180-mdss: invalid DSI bit rate");
        }

        let precision = 1_000i64;
        let unit_interval = 1_000_000 * precision / (bit_rate / 1_000);
        let interval_x8 = unit_interval << 3;
        let mut timings = PhyTimings::default();

        let minimum = Self::signed_div_ceil(38 * precision, interval_x8).max(0);
        let maximum = (95 * precision / interval_x8).max(0);
        timings.clock_prepare = Self::interpolate(maximum, minimum, 50, 0);

        let temporary = 300 * precision - (timings.clock_prepare << 3) * unit_interval;
        let minimum = Self::signed_div_ceil(temporary, interval_x8) - 1;
        let maximum = if minimum > 255 { 511 } else { 255 };
        timings.clock_zero = Self::interpolate(maximum, minimum, 2, 0);

        let minimum = Self::signed_div_ceil(60 * precision + 3 * unit_interval, interval_x8);
        let temporary = 105 * precision + 12 * unit_interval - 20 * precision;
        let maximum = (temporary + 3 * unit_interval) / interval_x8;
        timings.clock_trail = Self::interpolate(maximum, minimum, 30, 0);

        let minimum = Self::signed_div_ceil(40 * precision + 4 * unit_interval, interval_x8).max(0);
        let maximum = ((85 * precision + 6 * unit_interval) / interval_x8).max(0);
        timings.hs_prepare = Self::interpolate(maximum, minimum, 50, 0);

        let temporary =
            145 * precision + 10 * unit_interval - (timings.hs_prepare << 3) * unit_interval;
        let minimum = Self::signed_div_ceil(temporary, interval_x8) - 1;
        timings.hs_zero = Self::interpolate(255, minimum, 10, 0);

        let minimum = Self::signed_div_ceil(60 * precision + 4 * unit_interval, interval_x8) - 1;
        let temporary = 105 * precision + 12 * unit_interval - 20 * precision;
        let maximum = temporary / interval_x8 - 1;
        timings.hs_trail = Self::interpolate(maximum, minimum, 30, 0);
        timings.hs_request = Self::signed_div_ceil(50 * precision - 8 * unit_interval, interval_x8);

        let minimum = Self::signed_div_ceil(100 * precision, interval_x8) - 1;
        timings.hs_exit = Self::interpolate(255, minimum, 10, 0);

        let temporary = 60 * precision + 52 * unit_interval - 43 * unit_interval;
        let minimum = Self::signed_div_ceil(temporary, interval_x8) - 1;
        timings.clock_post = Self::interpolate(63, minimum, 10, 0);

        let mut temporary = 8 * unit_interval + (timings.clock_prepare << 3) * unit_interval;
        temporary += (((timings.clock_zero + 3) << 3) + 11) * unit_interval;
        temporary += ((timings.hs_request << 3) + 8) * unit_interval;
        let minimum = Self::signed_div_ceil(temporary, interval_x8) - 1;
        if minimum > 63 {
            let value = Self::interpolate(126, minimum, 10, 0);
            timings.clock_pre = value >> 1;
            timings.clock_pre_double = 1;
        } else {
            timings.clock_pre = Self::interpolate(63, minimum, 10, 0);
        }

        timings.turnaround_go = 3;
        timings.turnaround_sure = 0;
        timings.turnaround_get = 4;
        Ok(timings)
    }

    fn timing_value(value: i64) -> Result<u32, &'static str> {
        u32::try_from(value).map_err(|_| "qcom-sc7180-mdss: invalid DSI PHY timing")
    }

    fn program_timings(&self, timings: &PhyTimings) -> Result<(), &'static str> {
        let values = [
            timings.half_byte_clock,
            timings.clock_zero,
            timings.clock_prepare,
            timings.clock_trail,
            timings.hs_exit,
            timings.hs_zero,
            timings.hs_prepare,
            timings.hs_trail,
            timings.hs_request,
            (timings.turnaround_sure << 3) | timings.turnaround_go,
            timings.turnaround_get,
            0,
        ];
        for (index, value) in values.into_iter().enumerate() {
            self.phy_write(
                COMMON_TIMING_CONTROL0 + index * 4,
                Self::timing_value(value)?,
            );
        }
        Ok(())
    }

    pub(crate) fn initialize(
        &self,
        timing: DisplayTiming,
        data_lanes: u32,
        bits_per_pixel: u32,
    ) -> Result<DsiHostClockTimings, &'static str> {
        if data_lanes == 0 || data_lanes > 4 || !matches!(bits_per_pixel, 16 | 18 | 24) {
            return Err("qcom-sc7180-mdss: unsupported DSI PHY configuration");
        }
        self.prepare_common_block()?;

        let desired_bit_clock_hz = u64::from(timing.pixel_clock_khz)
            .checked_mul(1_000)
            .and_then(|value| value.checked_mul(u64::from(bits_per_pixel)))
            .map(|value| value / u64::from(data_lanes))
            .ok_or("qcom-sc7180-mdss: DSI bit clock overflow")?;
        let mut config = PhyConfiguration {
            desired_bit_clock_hz,
            bits_per_pixel,
            data_lanes,
            pixel_dividend: 0,
            pixel_divisor: 0,
            phy_post_divider: 0,
            pll_post_divider: 0,
        };
        let vco_hz = self.calculate_dividers(&mut config)?;
        let timings = Self::calculate_timings(desired_bit_clock_hz)?;
        self.program_timings(&timings)?;
        self.enable_lanes();
        self.start_pll(&config, vco_hz)?;

        Ok(DsiHostClockTimings {
            clock_pre: Self::timing_value(timings.clock_pre)?,
            clock_post: Self::timing_value(timings.clock_post)?,
            clock_pre_double: Self::timing_value(timings.clock_pre_double)?,
        })
    }
}
