// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 DSI0 host controller.

use scarlet::{arch, time};
use scarlet_driver_ti_sn65dsi86::DisplayTiming;

use crate::{phy::DsiHostClockTimings, registers::RegisterWindow};

pub(crate) const DSI_BASE: usize = 0x94000;

const HARDWARE_VERSION: usize = 0x000;
const CONTROL: usize = 0x004;
const STATUS: usize = 0x008;
const FIFO_STATUS: usize = 0x00c;
const VIDEO_MODE_CONTROL: usize = 0x010;
const VIDEO_MODE_CONTROL1: usize = 0x020;
const VIDEO_ACTIVE_HORIZONTAL: usize = 0x024;
const VIDEO_ACTIVE_VERTICAL: usize = 0x028;
const VIDEO_ACTIVE_TOTAL: usize = 0x02c;
const VIDEO_ACTIVE_HSYNC: usize = 0x030;
const VIDEO_ACTIVE_VSYNC: usize = 0x034;
const VIDEO_ACTIVE_VSYNC_POSITION: usize = 0x038;
const COMMAND_DMA_CONTROL: usize = 0x03c;
const ACK_ERROR_STATUS: usize = 0x068;
const TRIGGER_CONTROL: usize = 0x084;
const LANE_STATUS: usize = 0x0a8;
const LANE_CONTROL: usize = 0x0ac;
const LANE_SWAP_CONTROL: usize = 0x0b0;
const DATA_LANE0_PHY_ERROR: usize = 0x0b4;
const HS_TIMER_CONTROL: usize = 0x0bc;
const TIMEOUT_STATUS: usize = 0x0c0;
const CLOCK_OUT_TIMING_CONTROL: usize = 0x0c4;
const EOT_PACKET_CONTROL: usize = 0x0d0;
const INTERRUPT_CONTROL: usize = 0x110;
const SOFT_RESET: usize = 0x118;
const CLOCK_CONTROL: usize = 0x11c;
const CLOCK_STATUS: usize = 0x120;
const PHY_RESET: usize = 0x12c;
const TEST_PATTERN_CONTROL: usize = 0x15c;
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_VIDEO_INITIAL_VALUE: usize = 0x164;
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_MAIN_CONTROL: usize = 0x19c;
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_VIDEO_CONFIG: usize = 0x1a4;
const TEST_PATTERN_DMA_FIFO_RESET: usize = 0x1ec;
const CLOCK_PRE_EXTEND: usize = 0x180;

const PHY_BASE: usize = 0x94400;
const PHY_CLOCK_CONFIG0: usize = 0x010;
const PHY_CLOCK_CONFIG1: usize = 0x014;
const PHY_GLOBAL_CONTROL: usize = 0x018;
const PHY_VREG_CONTROL: usize = 0x020;
const PHY_CONTROL0: usize = 0x024;
const PHY_CONTROL2: usize = 0x02c;
const PHY_LANE_CONFIG0: usize = 0x030;
const PHY_LANE_CONFIG1: usize = 0x034;
const PHY_PLL_CONTROL: usize = 0x038;
const PHY_LANE_CONTROL0: usize = 0x098;
const PHY_STATUS: usize = 0x0ec;
const PLL_BASE: usize = 0x94a00;
const PLL_COMMON_STATUS: usize = 0x1a0;

const CONTROL_ENABLE: u32 = 1;
const CONTROL_VIDEO_ENABLE: u32 = 1 << 1;
const CONTROL_CLOCK_LANE_ENABLE: u32 = 1 << 8;
const VIDEO_TRAFFIC_MODE: u32 = 1 << 8;
const VIDEO_BLANKING_LOW_POWER: u32 = 9 << 12;
const VIDEO_FORMAT_RGB888: u32 = 3 << 4;
const HIGH_SPEED_TIMEOUT: u32 = 0xea60 | (4 << 16);
const TRIGGER_LINUX_VIDEO: u32 = (1 << 31) | (1 << 12) | 4;
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_CHECKERBOARD: u32 = 1 << 8;
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_VIDEO_RGB888: u32 = 1 | (1 << 2);
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_VIDEO_GENERAL: u32 = 3 << 4;
#[cfg(feature = "diagnostic-dsi-pattern")]
const TEST_PATTERN_ENABLE: u32 = 1;

pub(crate) struct DsiHost {
    mdss: RegisterWindow,
}

#[derive(Clone, Copy)]
pub(crate) struct DsiDiagnosticSnapshot {
    pub(crate) hardware_version: u32,
    pub(crate) control: u32,
    pub(crate) status: u32,
    pub(crate) fifo_status: u32,
    pub(crate) video_mode_control: u32,
    pub(crate) clock_control: u32,
    pub(crate) clock_status: u32,
    pub(crate) clock_pre_extend: u32,
    pub(crate) lane_status: u32,
    pub(crate) lane_control: u32,
    pub(crate) ack_error_status: u32,
    pub(crate) data_lane0_phy_error: u32,
    pub(crate) timeout_status: u32,
    pub(crate) interrupt_control: u32,
    pub(crate) test_pattern_control: u32,
    pub(crate) phy_clock_config0: u32,
    pub(crate) phy_clock_config1: u32,
    pub(crate) phy_global_control: u32,
    pub(crate) phy_vreg_control: u32,
    pub(crate) phy_control0: u32,
    pub(crate) phy_control2: u32,
    pub(crate) phy_lane_config0: u32,
    pub(crate) phy_lane_config1: u32,
    pub(crate) phy_pll_control: u32,
    pub(crate) phy_lane_control0: u32,
    pub(crate) phy_status: u32,
    pub(crate) pll_status: u32,
}

impl DsiHost {
    pub(crate) const fn new(mdss: RegisterWindow) -> Self {
        Self { mdss }
    }

    fn write(&self, offset: usize, value: u32) {
        self.mdss.write(DSI_BASE + offset, value)
    }

    fn update(&self, offset: usize, clear: u32, set: u32) {
        self.mdss.update(DSI_BASE + offset, clear, set)
    }

    fn read(&self, offset: usize) -> u32 {
        self.mdss.read(DSI_BASE + offset)
    }

    fn enable_internal_clocks(&self) {
        self.write(CLOCK_CONTROL, 0);
        self.update(CLOCK_CONTROL, 0, (1 << 1) | (1 << 9));
        self.update(CLOCK_CONTROL, 0, 1 << 2);
        self.update(CLOCK_CONTROL, 0, (1 << 0) | (1 << 3) | (1 << 4) | (1 << 5));
    }

    /// Reset the DSI PHY through the host-side reset line before programming
    /// the 10 nm PHY. This is the reset used by the Linux MSM DSI manager;
    /// toggling a PHY common register is not equivalent.
    pub(crate) fn reset_phy(&self) {
        self.write(PHY_RESET, 1);
        arch::io_wmb();
        time::udelay(1_000);
        self.write(PHY_RESET, 0);
        time::udelay(100);
    }

    /// Reset the controller after its DISP_CC link clocks are running.
    pub(crate) fn reset(&self) {
        self.write(CONTROL, 0);
        self.enable_internal_clocks();
        arch::io_wmb();
        self.write(SOFT_RESET, 1);
        time::udelay(20_000);
        self.write(SOFT_RESET, 0);
        arch::io_wmb();
        self.write(HS_TIMER_CONTROL, HIGH_SPEED_TIMEOUT);
        // Coreboot uses command-DMA test-pattern FIFO mode to issue panel
        // transactions. Those controls survive the alternate-firmware
        // handoff, so explicitly return both the selector and FIFO to the
        // video-host baseline after the controller reset.
        self.write(TEST_PATTERN_CONTROL, 0);
        self.write(TEST_PATTERN_DMA_FIFO_RESET, 1);
        arch::io_wmb();
        self.write(TEST_PATTERN_DMA_FIFO_RESET, 0);
    }

    pub(crate) fn configure_host(&self, data_lanes: u32) -> Result<(), &'static str> {
        if data_lanes == 0 || data_lanes > 4 {
            return Err("qcom-sc7180-mdss: invalid DSI lane count");
        }
        let lane_mask = (1u32 << data_lanes) - 1;

        self.write(
            VIDEO_MODE_CONTROL,
            VIDEO_BLANKING_LOW_POWER | VIDEO_TRAFFIC_MODE | VIDEO_FORMAT_RGB888,
        );
        self.write(VIDEO_MODE_CONTROL1, 0);
        self.write(COMMAND_DMA_CONTROL, (1 << 28) | (1 << 26));
        self.write(TRIGGER_CONTROL, TRIGGER_LINUX_VIDEO);
        self.write(EOT_PACKET_CONTROL, 1);
        self.write(LANE_SWAP_CONTROL, 0);
        self.write(HS_TIMER_CONTROL, HIGH_SPEED_TIMEOUT);
        // Linux programs every host parameter before exposing enabled lanes to
        // the video engine. Keep CONTROL as the final host-setup write.
        self.write(
            CONTROL,
            (lane_mask << 4) | CONTROL_ENABLE | CONTROL_CLOCK_LANE_ENABLE,
        );
        Ok(())
    }

    /// Program the video and D-PHY clock timing registers before host reset,
    /// following the ordering used by the Linux MSM DSI host driver.
    pub(crate) fn configure_timing(&self, timing: DisplayTiming, clock: DsiHostClockTimings) {
        let horizontal_total = timing.horizontal_total();
        let vertical_total = timing.vertical_total();
        let horizontal_start = u32::from(timing.hblank - timing.hsync_offset);
        let horizontal_end = horizontal_total - u32::from(timing.hsync_offset);
        let vertical_start = u32::from(timing.vblank - timing.vsync_offset);
        let vertical_end = vertical_total - u32::from(timing.vsync_offset);

        self.write(
            VIDEO_ACTIVE_HORIZONTAL,
            (horizontal_end << 16) | horizontal_start,
        );
        self.write(VIDEO_ACTIVE_VERTICAL, (vertical_end << 16) | vertical_start);
        self.write(
            VIDEO_ACTIVE_TOTAL,
            ((vertical_total - 1) << 16) | (horizontal_total - 1),
        );
        self.write(VIDEO_ACTIVE_HSYNC, u32::from(timing.hsync_width) << 16);
        self.write(VIDEO_ACTIVE_VSYNC, 0);
        self.write(
            VIDEO_ACTIVE_VSYNC_POSITION,
            u32::from(timing.vsync_width) << 16,
        );
        self.write(
            CLOCK_OUT_TIMING_CONTROL,
            (clock.clock_post << 8) | clock.clock_pre,
        );
        self.write(CLOCK_PRE_EXTEND, clock.clock_pre_double);
    }

    /// Switch the already-configured host into video mode without disturbing
    /// its lane mask or clock-lane state.
    pub(crate) fn enable_video(&self) {
        self.update(CONTROL, 0, CONTROL_ENABLE | CONTROL_VIDEO_ENABLE);
    }

    /// Clear FIFO faults accumulated while the host is enabled but the DPU
    /// stream is not running. The register is write-one-to-clear; the live
    /// FIFO empty/full state remains readable after this write.
    pub(crate) fn clear_fifo_status(&self) -> u32 {
        let status = self.read(FIFO_STATUS);
        if status != 0 {
            self.write(FIFO_STATUS, status);
        }
        status
    }

    /// Enable the DSI6G video-mode test-pattern generator used by Linux.
    ///
    /// This substitutes a generated checkerboard at the DSI host boundary,
    /// bypassing DPU source, mixer, and interface pixel data while retaining
    /// the real DSI host, PHY, bridge, eDP link, and panel path.
    #[cfg(feature = "diagnostic-dsi-pattern")]
    pub(crate) fn enable_video_test_pattern(&self) {
        self.write(TEST_PATTERN_VIDEO_INITIAL_VALUE, 0xff);
        self.write(TEST_PATTERN_MAIN_CONTROL, TEST_PATTERN_CHECKERBOARD);
        self.write(TEST_PATTERN_VIDEO_CONFIG, TEST_PATTERN_VIDEO_RGB888);
        // Coreboot uses the same register's DMA-FIFO mode while issuing
        // panel commands and that bit survives the firmware handoff. Select
        // the Linux video-pattern state outright instead of inheriting an
        // unrelated command-path mode.
        self.write(
            TEST_PATTERN_CONTROL,
            TEST_PATTERN_VIDEO_GENERAL | TEST_PATTERN_ENABLE,
        );
    }

    pub(crate) fn diagnostic_snapshot(&self) -> DsiDiagnosticSnapshot {
        DsiDiagnosticSnapshot {
            hardware_version: self.read(HARDWARE_VERSION),
            control: self.read(CONTROL),
            status: self.read(STATUS),
            fifo_status: self.read(FIFO_STATUS),
            video_mode_control: self.read(VIDEO_MODE_CONTROL),
            clock_control: self.read(CLOCK_CONTROL),
            clock_status: self.read(CLOCK_STATUS),
            clock_pre_extend: self.read(CLOCK_PRE_EXTEND),
            lane_status: self.read(LANE_STATUS),
            lane_control: self.read(LANE_CONTROL),
            ack_error_status: self.read(ACK_ERROR_STATUS),
            data_lane0_phy_error: self.read(DATA_LANE0_PHY_ERROR),
            timeout_status: self.read(TIMEOUT_STATUS),
            interrupt_control: self.read(INTERRUPT_CONTROL),
            test_pattern_control: self.read(TEST_PATTERN_CONTROL),
            phy_clock_config0: self.mdss.read(PHY_BASE + PHY_CLOCK_CONFIG0),
            phy_clock_config1: self.mdss.read(PHY_BASE + PHY_CLOCK_CONFIG1),
            phy_global_control: self.mdss.read(PHY_BASE + PHY_GLOBAL_CONTROL),
            phy_vreg_control: self.mdss.read(PHY_BASE + PHY_VREG_CONTROL),
            phy_control0: self.mdss.read(PHY_BASE + PHY_CONTROL0),
            phy_control2: self.mdss.read(PHY_BASE + PHY_CONTROL2),
            phy_lane_config0: self.mdss.read(PHY_BASE + PHY_LANE_CONFIG0),
            phy_lane_config1: self.mdss.read(PHY_BASE + PHY_LANE_CONFIG1),
            phy_pll_control: self.mdss.read(PHY_BASE + PHY_PLL_CONTROL),
            phy_lane_control0: self.mdss.read(PHY_BASE + PHY_LANE_CONTROL0),
            phy_status: self.mdss.read(PHY_BASE + PHY_STATUS),
            pll_status: self.mdss.read(PLL_BASE + PLL_COMMON_STATUS),
        }
    }
}
