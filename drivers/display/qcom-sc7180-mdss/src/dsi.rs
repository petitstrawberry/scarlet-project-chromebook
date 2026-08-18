// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 DSI0 host controller.

use scarlet_driver_ti_sn65dsi86::DisplayTiming;

use crate::registers::RegisterWindow;

pub(crate) const DSI_BASE: usize = 0x94000;

const CONTROL: usize = 0x004;
const VIDEO_MODE_CONTROL: usize = 0x010;
const VIDEO_ACTIVE_HORIZONTAL: usize = 0x024;
const VIDEO_ACTIVE_VERTICAL: usize = 0x028;
const VIDEO_ACTIVE_TOTAL: usize = 0x02c;
const VIDEO_ACTIVE_HSYNC: usize = 0x030;
const VIDEO_ACTIVE_VSYNC: usize = 0x034;
const VIDEO_ACTIVE_VSYNC_POSITION: usize = 0x038;
const COMMAND_DMA_CONTROL: usize = 0x03c;
const TRIGGER_CONTROL: usize = 0x084;
const HS_TIMER_CONTROL: usize = 0x0bc;
const EOT_PACKET_CONTROL: usize = 0x0d0;
const SOFT_RESET: usize = 0x118;
const CLOCK_CONTROL: usize = 0x11c;
const TPG_DMA_FIFO_RESET: usize = 0x1ec;

const CONTROL_ENABLE: u32 = 1;
const CONTROL_VIDEO_ENABLE: u32 = 1 << 1;
const CONTROL_CLOCK_LANE_ENABLE: u32 = 1 << 8;
const VIDEO_TRAFFIC_MODE: u32 = 1 << 8;
const VIDEO_BLANKING_LOW_POWER: u32 = 9 << 12;
const VIDEO_FORMAT_RGB888: u32 = 3 << 4;
const HIGH_SPEED_TIMEOUT: u32 = 0xea60 | (4 << 16);

pub(crate) struct DsiHost {
    mdss: RegisterWindow,
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

    pub(crate) fn reset(&self) {
        self.write(CONTROL, 0);
        self.write(SOFT_RESET, 1);
        self.write(SOFT_RESET, 0);
        self.write(HS_TIMER_CONTROL, HIGH_SPEED_TIMEOUT);
        self.write(TPG_DMA_FIFO_RESET, 1);
        self.write(TPG_DMA_FIFO_RESET, 0);
    }

    pub(crate) fn configure_host(&self, data_lanes: u32) -> Result<(), &'static str> {
        if data_lanes == 0 || data_lanes > 4 {
            return Err("qcom-sc7180-mdss: invalid DSI lane count");
        }
        let lane_mask = (1u32 << data_lanes) - 1;

        self.write(CLOCK_CONTROL, 0);
        self.update(CLOCK_CONTROL, 0, (1 << 1) | (1 << 9));
        self.update(CLOCK_CONTROL, 0, 1 << 2);
        self.update(CLOCK_CONTROL, 0, (1 << 0) | (1 << 3) | (1 << 4) | (1 << 5));
        self.write(TRIGGER_CONTROL, 4);
        self.write(
            CONTROL,
            (lane_mask << 4) | CONTROL_ENABLE | CONTROL_VIDEO_ENABLE | CONTROL_CLOCK_LANE_ENABLE,
        );
        self.write(COMMAND_DMA_CONTROL, (1 << 28) | (1 << 26));
        self.write(EOT_PACKET_CONTROL, 1);
        Ok(())
    }

    pub(crate) fn configure_video(&self, timing: DisplayTiming) {
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
            VIDEO_MODE_CONTROL,
            VIDEO_BLANKING_LOW_POWER | VIDEO_TRAFFIC_MODE | VIDEO_FORMAT_RGB888,
        );
        self.write(HS_TIMER_CONTROL, HIGH_SPEED_TIMEOUT);
        self.write(
            CONTROL,
            (0x0f << 4) | CONTROL_ENABLE | CONTROL_VIDEO_ENABLE | CONTROL_CLOCK_LANE_ENABLE,
        );
    }
}
