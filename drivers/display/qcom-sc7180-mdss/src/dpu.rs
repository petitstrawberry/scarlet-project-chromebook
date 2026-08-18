// SPDX-License-Identifier: GPL-2.0-only

//! SC7180 DPU scanout path: VIG0 → LM0 → CTL0 → DSI INTF1.

use scarlet::arch;
use scarlet_driver_ti_sn65dsi86::DisplayTiming;

use crate::registers::RegisterWindow;

const CONTROL_BASE: usize = 0x02000;
const SOURCE_PIPE_BASE: usize = 0x05000;
const LAYER_MIXER_BASE: usize = 0x45000;
const INTERFACE_BASE: usize = 0x6b800;
const VBIF_BASE: usize = 0xb0000;

const CONTROL_LAYER0: usize = 0x000;
const CONTROL_TOP: usize = 0x014;
const CONTROL_FLUSH: usize = 0x018;
const CONTROL_INTERFACE_ACTIVE: usize = 0x0f4;
const CONTROL_INTERFACE_FLUSH: usize = 0x110;

const SOURCE_SIZE: usize = 0x000;
const SOURCE_XY: usize = 0x008;
const SOURCE_OUTPUT_SIZE: usize = 0x00c;
const SOURCE_OUTPUT_XY: usize = 0x010;
const SOURCE_ADDRESS0: usize = 0x014;
const SOURCE_STRIDE0: usize = 0x024;
const SOURCE_FORMAT: usize = 0x030;
const SOURCE_UNPACK_PATTERN: usize = 0x034;
const SOURCE_OPERATION_MODE: usize = 0x038;
const SOURCE_EXTENSION_C0: usize = 0x108;
const SOURCE_EXTENSION_C1_C2: usize = 0x118;
const SOURCE_EXTENSION_C3: usize = 0x128;

const MIXER_OPERATION_MODE: usize = 0x000;
const MIXER_OUTPUT_SIZE: usize = 0x004;
const MIXER_BORDER_COLOR_0: usize = 0x008;
const MIXER_BORDER_COLOR_1: usize = 0x010;
const MIXER_BLEND_BASE: usize = 0x020;
const MIXER_BLEND_STRIDE: usize = 0x018;
const MIXER_BLEND_OPERATION: usize = 0x000;
const MIXER_BLEND_ALPHA: usize = 0x004;

const INTERFACE_TIMING_ENABLE: usize = 0x000;
const INTERFACE_CONFIG: usize = 0x004;
const INTERFACE_HSYNC_CONTROL: usize = 0x008;
const INTERFACE_VSYNC_PERIOD: usize = 0x00c;
const INTERFACE_VSYNC_PULSE: usize = 0x014;
const INTERFACE_DISPLAY_VSTART: usize = 0x01c;
const INTERFACE_DISPLAY_VEND: usize = 0x024;
const INTERFACE_DISPLAY_HCONTROL: usize = 0x03c;
const INTERFACE_UNDERFLOW_COLOR: usize = 0x048;
const INTERFACE_PANEL_FORMAT: usize = 0x090;
const INTERFACE_FETCH_START: usize = 0x170;
const INTERFACE_MUX: usize = 0x25c;

const VBIF_MEMORY_TYPE0: usize = 0x160;
const VBIF_MEMORY_TYPE1: usize = 0x164;
const VBIF_QOS_READ_BASE: usize = 0x550;
const VBIF_QOS_LEVEL_BASE: usize = 0x590;

const MAXIMUM_PREFILL_LINES: u32 = 24;
const PROGRAMMABLE_FETCH_ENABLE: u32 = 1 << 31;
const SOURCE_PIXEL_EXTENSION_OVERRIDE: u32 = 1 << 31;
const LAYER_BORDER_OUTPUT: u32 = 1 << 24;
const LAYER_VIG0_OUTPUT: u32 = 1;
const INTERFACE1_ACTIVE: u32 = 1 << 1;
const INTERFACE1_FLUSH: u32 = 1 << 1;
const FLUSH_VIG0: u32 = 1;
const FLUSH_MIXER0: u32 = 1 << 6;
const FLUSH_CONTROL: u32 = 1 << 17;
const FLUSH_INTERFACE: u32 = 1 << 31;

pub(crate) struct Dpu {
    registers: RegisterWindow,
}

#[derive(Clone, Copy)]
pub(crate) struct DpuDiagnosticSnapshot {
    pub(crate) control_layer0: u32,
    pub(crate) control_flush: u32,
    pub(crate) interface_active: u32,
    pub(crate) source_address: u32,
    pub(crate) source_stride: u32,
    pub(crate) source_format: u32,
    pub(crate) source_operation: u32,
    pub(crate) mixer_output_size: u32,
    pub(crate) mixer_border_color_0: u32,
    pub(crate) mixer_border_color_1: u32,
    pub(crate) timing_enable: u32,
    pub(crate) interface_config: u32,
    pub(crate) hsync_control: u32,
    pub(crate) vsync_period: u32,
    pub(crate) display_hcontrol: u32,
    pub(crate) panel_format: u32,
    pub(crate) fetch_start: u32,
    pub(crate) interface_mux: u32,
}

impl Dpu {
    pub(crate) const fn new(registers: RegisterWindow) -> Self {
        Self { registers }
    }

    fn write(&self, base: usize, offset: usize, value: u32) {
        self.registers.write(base + offset, value)
    }

    fn read(&self, base: usize, offset: usize) -> u32 {
        self.registers.read(base + offset)
    }

    fn configure_timing_generator(&self, timing: DisplayTiming) {
        let horizontal_total = timing.horizontal_total();
        let vertical_total = timing.vertical_total();
        let hsync_start = u32::from(timing.hblank - timing.hsync_offset);
        let hsync_end = horizontal_total - u32::from(timing.hsync_offset) - 1;
        let display_vstart = u32::from(timing.vblank - timing.vsync_offset) * horizontal_total;
        let display_vend = (vertical_total - u32::from(timing.vsync_offset)) * horizontal_total - 1;

        self.write(
            INTERFACE_BASE,
            INTERFACE_HSYNC_CONTROL,
            (horizontal_total << 16) | u32::from(timing.hsync_width),
        );
        self.write(
            INTERFACE_BASE,
            INTERFACE_VSYNC_PERIOD,
            vertical_total * horizontal_total,
        );
        self.write(
            INTERFACE_BASE,
            INTERFACE_VSYNC_PULSE,
            u32::from(timing.vsync_width) * horizontal_total,
        );
        self.write(
            INTERFACE_BASE,
            INTERFACE_DISPLAY_HCONTROL,
            (hsync_end << 16) | hsync_start,
        );
        self.write(INTERFACE_BASE, INTERFACE_DISPLAY_VSTART, display_vstart);
        self.write(INTERFACE_BASE, INTERFACE_DISPLAY_VEND, display_vend);
        self.write(INTERFACE_BASE, INTERFACE_UNDERFLOW_COLOR, 0);
        self.write(INTERFACE_BASE, INTERFACE_PANEL_FORMAT, 0x2100);
    }

    fn configure_fetch_start(&self, timing: DisplayTiming) {
        let back_porch_and_sync = u32::from(timing.vblank - timing.vsync_offset);
        if back_porch_and_sync + 1 >= MAXIMUM_PREFILL_LINES {
            return;
        }

        let vertical_total = timing.vertical_total();
        let horizontal_total = timing.horizontal_total();
        let front_porch_start = vertical_total - u32::from(timing.vsync_offset);
        let mut available = vertical_total - front_porch_start;
        let needed =
            MAXIMUM_PREFILL_LINES - u32::from(timing.vblank) + u32::from(timing.vsync_offset);
        available = available.min(needed);
        let fetch_start = (vertical_total - available) * horizontal_total + horizontal_total + 1;
        self.write(INTERFACE_BASE, INTERFACE_FETCH_START, fetch_start);
        self.write(INTERFACE_BASE, INTERFACE_CONFIG, PROGRAMMABLE_FETCH_ENABLE);
    }

    fn configure_vbif(&self) {
        self.write(VBIF_BASE, VBIF_MEMORY_TYPE0, 0x3333_3333);
        self.write(VBIF_BASE, VBIF_MEMORY_TYPE1, 0x0033_3333);

        const READ_REMAP: [(usize, u32); 6] = [
            (0, 0x0000_0003),
            (1, 0x1111_1113),
            (2, 0x2222_2224),
            (3, 0x3333_3334),
            (4, 0x4444_4445),
            (7, 0x7777_7776),
        ];
        for (index, value) in READ_REMAP {
            self.write(VBIF_BASE, VBIF_QOS_READ_BASE + index * 8, value);
        }

        const LEVEL_REMAP: [u32; 6] = [
            0x0000_0003,
            0x1111_1113,
            0x2222_2224,
            0x3333_3334,
            0x4444_4445,
            0x7777_7776,
        ];
        for (index, value) in LEVEL_REMAP.into_iter().enumerate() {
            self.write(VBIF_BASE, VBIF_QOS_LEVEL_BASE + index * 8, value);
        }
    }

    fn configure_source(&self, timing: DisplayTiming, framebuffer: u32) {
        let size = (u32::from(timing.vactive) << 16) | u32::from(timing.hactive);
        let stride = u32::from(timing.hactive) * 4;

        self.write(SOURCE_PIPE_BASE, SOURCE_STRIDE0, stride);
        self.write(SOURCE_PIPE_BASE, SOURCE_SIZE, size);
        self.write(SOURCE_PIPE_BASE, SOURCE_OUTPUT_SIZE, size);
        self.write(SOURCE_PIPE_BASE, SOURCE_XY, 0);
        self.write(SOURCE_PIPE_BASE, SOURCE_OUTPUT_XY, 0);
        self.write(SOURCE_PIPE_BASE, SOURCE_ADDRESS0, framebuffer);
        self.write(SOURCE_PIPE_BASE, SOURCE_FORMAT, 0x0002_36ff);
        self.write(SOURCE_PIPE_BASE, SOURCE_UNPACK_PATTERN, 0x0302_0001);
        self.write(SOURCE_PIPE_BASE, SOURCE_EXTENSION_C0, size);
        self.write(SOURCE_PIPE_BASE, SOURCE_EXTENSION_C1_C2, size);
        self.write(SOURCE_PIPE_BASE, SOURCE_EXTENSION_C3, size);
        self.write(
            SOURCE_PIPE_BASE,
            SOURCE_OPERATION_MODE,
            SOURCE_PIXEL_EXTENSION_OVERRIDE,
        );
    }

    fn configure_mixer_and_control(&self, timing: DisplayTiming) {
        let size = (u32::from(timing.vactive) << 16) | u32::from(timing.hactive);
        self.write(LAYER_MIXER_BASE, MIXER_OUTPUT_SIZE, size);
        self.write(LAYER_MIXER_BASE, MIXER_OPERATION_MODE, 0);
        #[cfg(feature = "diagnostic-border-fill")]
        {
            // SC7180 LM border channels are 12-bit G/B in COLOR_0 and
            // 12-bit R/A in COLOR_1. Full scale in all channels produces an
            // unmistakable white frame without enabling a source pipe.
            self.write(LAYER_MIXER_BASE, MIXER_BORDER_COLOR_0, 0x0fff_0fff);
            self.write(LAYER_MIXER_BASE, MIXER_BORDER_COLOR_1, 0x0fff_0fff);
        }
        for stage in 0..6 {
            let blend = MIXER_BLEND_BASE + stage * MIXER_BLEND_STRIDE;
            self.write(LAYER_MIXER_BASE, blend + MIXER_BLEND_OPERATION, 0x100);
            self.write(LAYER_MIXER_BASE, blend + MIXER_BLEND_ALPHA, 0x00ff_0000);
        }

        #[cfg(feature = "diagnostic-border-fill")]
        self.write(CONTROL_BASE, CONTROL_LAYER0, LAYER_BORDER_OUTPUT);
        #[cfg(not(feature = "diagnostic-border-fill"))]
        self.write(
            CONTROL_BASE,
            CONTROL_LAYER0,
            LAYER_BORDER_OUTPUT | LAYER_VIG0_OUTPUT,
        );
        self.write(CONTROL_BASE, CONTROL_TOP, 0);
        self.write(CONTROL_BASE, CONTROL_INTERFACE_ACTIVE, INTERFACE1_ACTIVE);
        self.write(INTERFACE_BASE, INTERFACE_MUX, 0x000f_0000);
    }

    fn flush(&self) {
        self.write(CONTROL_BASE, CONTROL_INTERFACE_FLUSH, INTERFACE1_FLUSH);
        #[cfg(feature = "diagnostic-border-fill")]
        let flush = FLUSH_MIXER0 | FLUSH_CONTROL | FLUSH_INTERFACE;
        #[cfg(not(feature = "diagnostic-border-fill"))]
        let flush = FLUSH_VIG0 | FLUSH_MIXER0 | FLUSH_CONTROL | FLUSH_INTERFACE;
        self.write(CONTROL_BASE, CONTROL_FLUSH, flush);
        arch::io_wmb();
    }

    pub(crate) fn configure(
        &self,
        timing: DisplayTiming,
        framebuffer: usize,
    ) -> Result<(), &'static str> {
        let framebuffer = u32::try_from(framebuffer)
            .map_err(|_| "qcom-sc7180-mdss: scanout address exceeds 32 bits")?;
        self.configure_timing_generator(timing);
        self.configure_fetch_start(timing);
        self.configure_vbif();
        self.configure_source(timing, framebuffer);
        self.configure_mixer_and_control(timing);
        Ok(())
    }

    pub(crate) fn start(&self) {
        self.flush();
        self.write(INTERFACE_BASE, INTERFACE_TIMING_ENABLE, 1);
        arch::io_wmb();
    }

    pub(crate) fn diagnostic_snapshot(&self) -> DpuDiagnosticSnapshot {
        arch::io_rmb();
        DpuDiagnosticSnapshot {
            control_layer0: self.read(CONTROL_BASE, CONTROL_LAYER0),
            control_flush: self.read(CONTROL_BASE, CONTROL_FLUSH),
            interface_active: self.read(CONTROL_BASE, CONTROL_INTERFACE_ACTIVE),
            source_address: self.read(SOURCE_PIPE_BASE, SOURCE_ADDRESS0),
            source_stride: self.read(SOURCE_PIPE_BASE, SOURCE_STRIDE0),
            source_format: self.read(SOURCE_PIPE_BASE, SOURCE_FORMAT),
            source_operation: self.read(SOURCE_PIPE_BASE, SOURCE_OPERATION_MODE),
            mixer_output_size: self.read(LAYER_MIXER_BASE, MIXER_OUTPUT_SIZE),
            mixer_border_color_0: self.read(LAYER_MIXER_BASE, MIXER_BORDER_COLOR_0),
            mixer_border_color_1: self.read(LAYER_MIXER_BASE, MIXER_BORDER_COLOR_1),
            timing_enable: self.read(INTERFACE_BASE, INTERFACE_TIMING_ENABLE),
            interface_config: self.read(INTERFACE_BASE, INTERFACE_CONFIG),
            hsync_control: self.read(INTERFACE_BASE, INTERFACE_HSYNC_CONTROL),
            vsync_period: self.read(INTERFACE_BASE, INTERFACE_VSYNC_PERIOD),
            display_hcontrol: self.read(INTERFACE_BASE, INTERFACE_DISPLAY_HCONTROL),
            panel_format: self.read(INTERFACE_BASE, INTERFACE_PANEL_FORMAT),
            fetch_start: self.read(INTERFACE_BASE, INTERFACE_FETCH_START),
            interface_mux: self.read(INTERFACE_BASE, INTERFACE_MUX),
        }
    }

    pub(crate) fn present(&self, framebuffer: usize) -> Result<(), &'static str> {
        let framebuffer = u32::try_from(framebuffer)
            .map_err(|_| "qcom-sc7180-mdss: scanout address exceeds 32 bits")?;
        self.write(SOURCE_PIPE_BASE, SOURCE_ADDRESS0, framebuffer);
        self.flush();
        Ok(())
    }
}
