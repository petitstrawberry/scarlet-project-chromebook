// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! TI SN65DSI86 MIPI DSI to embedded DisplayPort bridge.
//!
//! The driver binds the bridge as an I2C client, exposes DisplayPort AUX, reads
//! and decodes the preferred EDID timing, and programs the DSI-to-eDP link for
//! a native display controller.
//!
//! # Provenance
//!
//! The register interface and device-ID check are adapted from Linux
//! `drivers/gpu/drm/bridge/ti-sn65dsi86.c` and coreboot
//! `src/drivers/ti/sn65dsi86bridge/sn65dsi86bridge.c`.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    device::{
        DeviceInfo,
        i2c::{I2cAddress, I2cBus, I2cError, I2cMessage},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
    sync::IrqSpinLock,
    time,
};

const MAX_7BIT_ADDRESS: usize = 0x7f;

const DEVICE_ID_START: u8 = 0x00;
const DEVICE_REVISION: u8 = 0x08;
const DP_PLL_SOURCE: u8 = 0x0a;
const PLL_ENABLE: u8 = 0x0d;
const DSI_LANES: u8 = 0x10;
const DSI_CLOCK: u8 = 0x12;
const ACTIVE_WIDTH_LOW: u8 = 0x20;
const ACTIVE_WIDTH_HIGH: u8 = 0x21;
const ACTIVE_HEIGHT_LOW: u8 = 0x24;
const ACTIVE_HEIGHT_HIGH: u8 = 0x25;
const HSYNC_WIDTH_LOW: u8 = 0x2c;
const HSYNC_WIDTH_HIGH: u8 = 0x2d;
const VSYNC_WIDTH_LOW: u8 = 0x30;
const VSYNC_WIDTH_HIGH: u8 = 0x31;
const HORIZONTAL_BACK_PORCH: u8 = 0x34;
const VERTICAL_BACK_PORCH: u8 = 0x36;
const HORIZONTAL_FRONT_PORCH: u8 = 0x38;
const VERTICAL_FRONT_PORCH: u8 = 0x3a;
const COLOR_BAR: u8 = 0x3c;
const DP_LANE_ASSIGNMENT: u8 = 0x59;
const ENHANCED_FRAME: u8 = 0x5a;
const DATA_FORMAT: u8 = 0x5b;
const HPD_DISABLE: u8 = 0x5c;
const SSC_CONFIG: u8 = 0x93;
const DATA_RATE: u8 = 0x94;
const TRAINING_SETTINGS: u8 = 0x95;
const MAIN_LINK_MODE: u8 = 0x96;
const AUX_WRITE_DATA: u8 = 0x64;
const AUX_ADDRESS_HIGH: u8 = 0x74;
const AUX_LENGTH: u8 = 0x77;
const AUX_COMMAND: u8 = 0x78;
const AUX_READ_DATA: u8 = 0x79;
const AUX_STATUS: u8 = 0xf4;
const ERROR_STATUS_START: u8 = 0xf0;
const VIDEO_ERROR_STATUS_END: u8 = 0xf7;

const DEVICE_ID: [u8; 8] = *b"68ISD   ";
const DP_PLL_LOCKED: u8 = 1 << 7;
const VIDEO_STREAM_ENABLED: u8 = 1 << 3;
const DSI_CHANNEL_MODE_MASK: u8 = 0x3 << 5;
const DSI_SINGLE_CHANNEL_A: u8 = 1 << 5;
const DSI_LANE_COUNT_MASK: u8 = 0x3 << 3;
const DP_LANE_POLARITY_MASK: u8 = 0xf0;
const DATA_FORMAT_18BPP_RGB: u8 = 1 << 0;
const SCRAMBLER_DISABLED: u8 = 1 << 4;
const SYNC_PULSE_NEGATIVE: u8 = 1 << 7;
const HPD_IS_DISABLED: u8 = 1 << 0;
const HPD_DEBOUNCED_STATE: u8 = 1 << 4;
const MAIN_LINK_OFF: u8 = 0x0;
const MAIN_LINK_NORMAL: u8 = 0x1;
const MAIN_LINK_SEMI_AUTOMATIC_TRAINING: u8 = 0x0a;
const AUX_SEND: u8 = 1 << 0;
const AUX_REPLY_TIMEOUT: u8 = 1 << 3;
const AUX_DEFERRED: u8 = 1 << 4;
const AUX_SHORT_REPLY: u8 = 1 << 5;
const AUX_NATIVE_I2C_FAILURE: u8 = 1 << 6;
const AUX_CLEAR_STATUS: u8 =
    AUX_SEND | AUX_REPLY_TIMEOUT | AUX_DEFERRED | AUX_SHORT_REPLY | AUX_NATIVE_I2C_FAILURE;

const AUX_MAX_PAYLOAD: usize = 16;
const AUX_ADDRESS_MASK: u32 = 0x000f_ffff;
const AUX_TIMEOUT_US: u64 = 50_000;
const AUX_POLL_INTERVAL_US: u64 = 10;
const PLL_TIMEOUT_US: u64 = 500_000;
const LINK_TRAINING_TIMEOUT_US: u64 = 500_000;
const LINK_TRAINING_RETRIES: usize = 10;

const DPCD_MAX_LINK_RATE: u32 = 0x001;
const DPCD_CONFIGURATION_SET: u32 = 0x10a;
const DPCD_DISPLAY_CONTROL: u32 = 0x720;
const DPCD_BACKLIGHT_MODE: u32 = 0x721;
const DPCD_BACKLIGHT_BRIGHTNESS_MSB: u32 = 0x722;
const DPCD_BACKLIGHT_CONTROL_MODE: u8 = 0x2;
const DPCD_BACKLIGHT_ENABLE: u8 = 0x1;
const DPCD_LANE_COUNT_MASK: u8 = 0x1f;

const DP_RATE_MBPS: [u32; 8] = [0, 1620, 2160, 2430, 2700, 3240, 4320, 5400];
const DP_LINK_RATE_1_62: u8 = 0x06;
const DP_LINK_RATE_2_70: u8 = 0x0a;
const DP_LINK_RATE_5_40: u8 = 0x14;

const DSI_MIN_CLOCK_MHZ: u64 = 40;
const DSI_MAX_CLOCK_MHZ: u64 = 750;
const DSI_CLOCK_STEP_MHZ: u64 = 5;

const EDID_I2C_ADDRESS: u32 = 0x50;
const EDID_BLOCK_SIZE: usize = 128;
const EDID_MAX_BLOCKS: usize = 2;
const EDID_EXTENSION_COUNT: usize = 0x7e;
const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
const EDID_DETAILED_TIMING_HSYNC_POSITIVE: u8 = 1 << 1;
const EDID_DETAILED_TIMING_VSYNC_POSITIVE: u8 = 1 << 2;

/// One DisplayPort AUX request supported by the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DpAuxRequest {
    /// I2C-over-AUX write followed by STOP.
    I2cWrite = 0x0,
    /// I2C-over-AUX read followed by STOP.
    I2cRead = 0x1,
    /// I2C-over-AUX write retaining the bus for a following request.
    I2cWriteMot = 0x4,
    /// I2C-over-AUX read retaining the bus for a following request.
    I2cReadMot = 0x5,
    /// Native DisplayPort AUX write.
    NativeWrite = 0x8,
    /// Native DisplayPort AUX read.
    NativeRead = 0x9,
}

impl DpAuxRequest {
    const fn is_write(self) -> bool {
        matches!(self, Self::I2cWrite | Self::I2cWriteMot | Self::NativeWrite)
    }
}

/// Error returned by an SN65DSI86 AUX transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sn65dsi86AuxError {
    /// The bridge's register I2C transaction failed.
    I2c(I2cError),
    /// Request address or payload length is unsupported.
    InvalidArgument,
    /// The bridge did not complete the request before the deadline.
    Timeout,
    /// The downstream native or I2C target rejected the request.
    Nack,
    /// The downstream target deferred the request.
    Deferred,
    /// The bridge returned fewer bytes than requested.
    ShortReply,
}

impl From<I2cError> for Sn65dsi86AuxError {
    fn from(error: I2cError) -> Self {
        Self::I2c(error)
    }
}

/// Complete timing for one progressive display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayTiming {
    /// Pixel clock in kHz.
    pub pixel_clock_khz: u32,
    /// Active horizontal pixels.
    pub hactive: u16,
    /// Total horizontal blanking pixels.
    pub hblank: u16,
    /// Horizontal front porch in pixels.
    pub hsync_offset: u16,
    /// Horizontal sync pulse width in pixels.
    pub hsync_width: u16,
    /// Active vertical lines.
    pub vactive: u16,
    /// Total vertical blanking lines.
    pub vblank: u16,
    /// Vertical front porch in lines.
    pub vsync_offset: u16,
    /// Vertical sync pulse width in lines.
    pub vsync_width: u16,
    /// Whether the horizontal sync pulse is active high.
    pub hsync_positive: bool,
    /// Whether the vertical sync pulse is active high.
    pub vsync_positive: bool,
}

impl DisplayTiming {
    /// Total horizontal pixels including blanking.
    pub const fn horizontal_total(self) -> u32 {
        self.hactive as u32 + self.hblank as u32
    }

    /// Total vertical lines including blanking.
    pub const fn vertical_total(self) -> u32 {
        self.vactive as u32 + self.vblank as u32
    }

    /// Horizontal back porch in pixels.
    pub fn horizontal_back_porch(self) -> Option<u16> {
        self.hblank
            .checked_sub(self.hsync_offset)?
            .checked_sub(self.hsync_width)
    }

    /// Vertical back porch in lines.
    pub fn vertical_back_porch(self) -> Option<u16> {
        self.vblank
            .checked_sub(self.vsync_offset)?
            .checked_sub(self.vsync_width)
    }

    fn validate(self) -> Result<Self, Sn65dsi86LinkError> {
        if self.pixel_clock_khz == 0
            || self.hactive == 0
            || self.vactive == 0
            || self.horizontal_back_porch().is_none()
            || self.vertical_back_porch().is_none()
        {
            return Err(Sn65dsi86LinkError::InvalidTiming);
        }
        Ok(self)
    }
}

/// Error returned while preparing or training the native display link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sn65dsi86LinkError {
    /// The bridge's register I2C transaction failed.
    I2c(I2cError),
    /// A DisplayPort AUX transaction failed.
    Aux(Sn65dsi86AuxError),
    /// The EDID preferred timing is absent or malformed.
    InvalidTiming,
    /// The sink and bridge have no usable lane/rate combination.
    UnsupportedLink,
    /// The DisplayPort PLL did not lock.
    PllTimeout,
    /// Semi-automatic DisplayPort link training did not converge.
    LinkTrainingFailed,
}

impl From<I2cError> for Sn65dsi86LinkError {
    fn from(error: I2cError) -> Self {
        Self::I2c(error)
    }
}

impl From<Sn65dsi86AuxError> for Sn65dsi86LinkError {
    fn from(error: Sn65dsi86AuxError) -> Self {
        Self::Aux(error)
    }
}

/// Up to two validated 128-byte EDID blocks read through DisplayPort AUX.
#[derive(Clone)]
pub struct Sn65dsi86Edid {
    bytes: [u8; EDID_BLOCK_SIZE * EDID_MAX_BLOCKS],
    blocks: usize,
}

impl Sn65dsi86Edid {
    /// Return the validated EDID bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.blocks * EDID_BLOCK_SIZE]
    }

    /// Return the number of complete EDID blocks.
    pub const fn block_count(&self) -> usize {
        self.blocks
    }

    /// Decode the first detailed timing descriptor from the base EDID block.
    pub fn preferred_timing(&self) -> Result<DisplayTiming, Sn65dsi86LinkError> {
        const DETAILED_TIMING_OFFSET: usize = 54;
        const DETAILED_TIMING_SIZE: usize = 18;

        let descriptor = self
            .as_bytes()
            .get(DETAILED_TIMING_OFFSET..DETAILED_TIMING_OFFSET + DETAILED_TIMING_SIZE)
            .ok_or(Sn65dsi86LinkError::InvalidTiming)?;
        let pixel_clock_khz = u32::from(u16::from_le_bytes([descriptor[0], descriptor[1]])) * 10;
        let hactive = u16::from(descriptor[2]) | (u16::from(descriptor[4] & 0xf0) << 4);
        let hblank = u16::from(descriptor[3]) | (u16::from(descriptor[4] & 0x0f) << 8);
        let vactive = u16::from(descriptor[5]) | (u16::from(descriptor[7] & 0xf0) << 4);
        let vblank = u16::from(descriptor[6]) | (u16::from(descriptor[7] & 0x0f) << 8);
        let hsync_offset = u16::from(descriptor[8]) | (u16::from(descriptor[11] & 0xc0) << 2);
        let hsync_width = u16::from(descriptor[9]) | (u16::from(descriptor[11] & 0x30) << 4);
        let vsync_offset = u16::from(descriptor[10] >> 4) | (u16::from(descriptor[11] & 0x0c) << 2);
        let vsync_width =
            u16::from(descriptor[10] & 0x0f) | (u16::from(descriptor[11] & 0x03) << 4);
        let sync = descriptor[17];

        DisplayTiming {
            pixel_clock_khz,
            hactive,
            hblank,
            hsync_offset,
            hsync_width,
            vactive,
            vblank,
            vsync_offset,
            vsync_width,
            hsync_positive: sync & EDID_DETAILED_TIMING_HSYNC_POSITIVE != 0,
            vsync_positive: sync & EDID_DETAILED_TIMING_VSYNC_POSITIVE != 0,
        }
        .validate()
    }
}

/// Read-only snapshot of the bridge state inherited from firmware.
#[derive(Debug, Clone, Copy)]
pub struct Sn65dsi86DiagnosticSnapshot {
    /// Silicon revision register.
    pub revision: u8,
    /// Raw DP PLL source and lock register.
    pub dp_pll_source: u8,
    /// Whether the DP PLL is enabled.
    pub pll_enabled: bool,
    /// Raw DSI lane configuration.
    pub dsi_lanes: u8,
    /// Raw DSI clock-frequency selector.
    pub dsi_clock: u8,
    /// Raw DisplayPort lane assignment.
    pub dp_lane_assignment: u8,
    /// Raw enhanced-frame and video-stream register.
    pub enhanced_frame: u8,
    /// Raw DisplayPort output data format.
    pub data_format: u8,
    /// Raw link-training settings, including the scrambler-disable bit.
    pub training_settings: u8,
    /// Raw high byte of the horizontal sync width and polarity.
    pub hsync_width_high: u8,
    /// Raw high byte of the vertical sync width and polarity.
    pub vsync_width_high: u8,
    /// Raw HPD state/control register.
    pub hpd: u8,
    /// Raw spread-spectrum and DP lane-count register.
    pub ssc_config: u8,
    /// Raw DP data-rate selector.
    pub data_rate: u8,
    /// Raw main-link mode register.
    pub main_link_mode: u8,
    /// Raw internal color-bar generator register.
    pub color_bar: u8,
    /// Raw status/error register window `0xF0..=0xF8`.
    pub error_status: [u8; 9],
}

impl Sn65dsi86DiagnosticSnapshot {
    /// Return whether the bridge reports a locked DP PLL.
    pub const fn dp_pll_locked(self) -> bool {
        self.dp_pll_source & DP_PLL_LOCKED != 0
    }

    /// Return whether the video stream is currently enabled.
    pub const fn video_stream_enabled(self) -> bool {
        self.enhanced_frame & VIDEO_STREAM_ENABLED != 0
    }

    /// Return whether internal HPD handling is disabled.
    pub const fn hpd_disabled(self) -> bool {
        self.hpd & HPD_IS_DISABLED != 0
    }

    /// Return the bridge's debounced HPD state.
    pub const fn hpd_asserted(self) -> bool {
        self.hpd & HPD_DEBOUNCED_STATE != 0
    }

    /// Return whether the bridge's internal color-bar generator is enabled.
    pub const fn color_bar_enabled(self) -> bool {
        self.color_bar & (1 << 4) != 0
    }
}

/// Internal SN65DSI86 DisplayPort test-pattern selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Sn65dsi86ColorBar {
    /// Eight vertical SMPTE color bars.
    VerticalEightColors = 0,
    /// Eight vertical grayscale bars.
    VerticalEightGrayscale = 1,
    /// Three vertical color bars.
    VerticalThreeColors = 2,
    /// Vertical stripe pattern.
    VerticalStripes = 3,
    /// Eight horizontal SMPTE color bars.
    HorizontalEightColors = 4,
    /// Eight horizontal grayscale bars.
    HorizontalEightGrayscale = 5,
    /// Three horizontal color bars.
    HorizontalThreeColors = 6,
    /// Horizontal stripe pattern.
    HorizontalStripes = 7,
}

/// An SN65DSI86 bridge connected to a Scarlet I2C bus.
pub struct Sn65dsi86 {
    bus: Arc<dyn I2cBus>,
    address: I2cAddress,
    phandle: u32,
    bus_phandle: u32,
    aux_lock: IrqSpinLock<()>,
}

impl Sn65dsi86 {
    fn new(bus: Arc<dyn I2cBus>, address: I2cAddress, phandle: u32, bus_phandle: u32) -> Self {
        Self {
            bus,
            address,
            phandle,
            bus_phandle,
            aux_lock: IrqSpinLock::new(()),
        }
    }

    fn read_into(&self, register: u8, value: &mut [u8]) -> Result<(), I2cError> {
        let mut messages = vec![
            I2cMessage::write(self.address, &[register], false),
            I2cMessage::read(self.address, value.len(), true),
        ];
        self.bus.transfer(&mut messages)?;
        value.copy_from_slice(&messages[1].data);
        Ok(())
    }

    fn read_exact<const N: usize>(&self, register: u8) -> Result<[u8; N], I2cError> {
        let mut value = [0; N];
        self.read_into(register, &mut value)?;
        Ok(value)
    }

    fn write_bytes(&self, register: u8, value: &[u8]) -> Result<(), I2cError> {
        let mut frame = Vec::with_capacity(value.len() + 1);
        frame.push(register);
        frame.extend_from_slice(value);
        self.bus
            .transfer(&mut [I2cMessage::write(self.address, &frame, true)])
    }

    fn read_u8(&self, register: u8) -> Result<u8, I2cError> {
        Ok(self.read_exact::<1>(register)?[0])
    }

    fn write_u8(&self, register: u8, value: u8) -> Result<(), I2cError> {
        self.write_bytes(register, &[value])
    }

    fn update_u8(&self, register: u8, mask: u8, value: u8) -> Result<(), I2cError> {
        let current = self.read_u8(register)?;
        self.write_u8(register, (current & !mask) | (value & mask))
    }

    fn wait_register(
        &self,
        register: u8,
        timeout_us: u64,
        predicate: impl Fn(u8) -> bool,
    ) -> Result<u8, I2cError> {
        let start = time::current_time();
        loop {
            let value = self.read_u8(register)?;
            if predicate(value) {
                return Ok(value);
            }
            if time::current_time().saturating_sub(start) >= timeout_us {
                return Err(I2cError::Timeout);
            }
            time::udelay(100);
        }
    }

    fn verify_device_id(&self) -> Result<(), I2cError> {
        if self.read_exact::<8>(DEVICE_ID_START)? == DEVICE_ID {
            Ok(())
        } else {
            Err(I2cError::BusError)
        }
    }

    /// Read bridge state without altering link or display configuration.
    pub fn diagnostic_snapshot(&self) -> Result<Sn65dsi86DiagnosticSnapshot, I2cError> {
        Ok(Sn65dsi86DiagnosticSnapshot {
            revision: self.read_u8(DEVICE_REVISION)?,
            dp_pll_source: self.read_u8(DP_PLL_SOURCE)?,
            pll_enabled: self.read_u8(PLL_ENABLE)? & 1 != 0,
            dsi_lanes: self.read_u8(DSI_LANES)?,
            dsi_clock: self.read_u8(DSI_CLOCK)?,
            dp_lane_assignment: self.read_u8(DP_LANE_ASSIGNMENT)?,
            enhanced_frame: self.read_u8(ENHANCED_FRAME)?,
            data_format: self.read_u8(DATA_FORMAT)?,
            training_settings: self.read_u8(TRAINING_SETTINGS)?,
            hsync_width_high: self.read_u8(HSYNC_WIDTH_HIGH)?,
            vsync_width_high: self.read_u8(VSYNC_WIDTH_HIGH)?,
            hpd: self.read_u8(HPD_DISABLE)?,
            ssc_config: self.read_u8(SSC_CONFIG)?,
            data_rate: self.read_u8(DATA_RATE)?,
            main_link_mode: self.read_u8(MAIN_LINK_MODE)?,
            color_bar: self.read_u8(COLOR_BAR)?,
            error_status: self.read_exact::<9>(ERROR_STATUS_START)?,
        })
    }

    /// Select the bridge's DisplayPort-side test pattern.
    ///
    /// The generator bypasses the DSI input and isolates panel, backlight, and
    /// eDP link failures from the source display-controller path.
    pub fn set_color_bar(&self, pattern: Option<Sn65dsi86ColorBar>) -> Result<(), I2cError> {
        let value = pattern.map_or(0, |pattern| (1 << 4) | pattern as u8);
        self.write_u8(COLOR_BAR, value)
    }

    fn aux_transfer_locked(
        &self,
        request: DpAuxRequest,
        address: u32,
        payload: &mut [u8],
    ) -> Result<usize, Sn65dsi86AuxError> {
        if payload.is_empty() || payload.len() > AUX_MAX_PAYLOAD || address > AUX_ADDRESS_MASK {
            return Err(Sn65dsi86AuxError::InvalidArgument);
        }

        let command = (request as u8) << 4;
        self.write_bytes(AUX_COMMAND, &[command])?;
        self.write_bytes(
            AUX_ADDRESS_HIGH,
            &[
                ((address >> 16) & 0x0f) as u8,
                (address >> 8) as u8,
                address as u8,
                payload.len() as u8,
            ],
        )?;
        if request.is_write() {
            self.write_bytes(AUX_WRITE_DATA, payload)?;
        }

        self.write_bytes(AUX_STATUS, &[AUX_CLEAR_STATUS])?;
        self.write_bytes(AUX_COMMAND, &[command | AUX_SEND])?;

        let start = time::current_time();
        loop {
            if self.read_u8(AUX_COMMAND)? & AUX_SEND == 0 {
                break;
            }
            if time::current_time().saturating_sub(start) >= AUX_TIMEOUT_US {
                return Err(Sn65dsi86AuxError::Timeout);
            }
            time::udelay(AUX_POLL_INTERVAL_US);
        }

        let status = self.read_u8(AUX_STATUS)?;
        if status & AUX_REPLY_TIMEOUT != 0 {
            return Err(Sn65dsi86AuxError::Timeout);
        }
        if status & AUX_DEFERRED != 0 {
            return Err(Sn65dsi86AuxError::Deferred);
        }
        if status & AUX_NATIVE_I2C_FAILURE != 0 {
            return Err(Sn65dsi86AuxError::Nack);
        }

        let received = if status & AUX_SHORT_REPLY != 0 {
            usize::from(self.read_u8(AUX_LENGTH)?)
        } else {
            payload.len()
        };
        if received > payload.len() {
            return Err(Sn65dsi86AuxError::InvalidArgument);
        }
        if !request.is_write() {
            self.read_into(AUX_READ_DATA, &mut payload[..received])?;
        }
        Ok(received)
    }

    /// Execute one bridge AUX transaction of at most 16 bytes.
    pub fn aux_transfer(
        &self,
        request: DpAuxRequest,
        address: u32,
        payload: &mut [u8],
    ) -> Result<usize, Sn65dsi86AuxError> {
        let _guard = self.aux_lock.lock();
        self.aux_transfer_locked(request, address, payload)
    }

    /// Read an arbitrary DPCD range using native AUX requests.
    pub fn read_dpcd(&self, mut address: u32, output: &mut [u8]) -> Result<(), Sn65dsi86AuxError> {
        let _guard = self.aux_lock.lock();
        for chunk in output.chunks_mut(AUX_MAX_PAYLOAD) {
            let received = self.aux_transfer_locked(DpAuxRequest::NativeRead, address, chunk)?;
            if received != chunk.len() {
                return Err(Sn65dsi86AuxError::ShortReply);
            }
            address = address
                .checked_add(chunk.len() as u32)
                .ok_or(Sn65dsi86AuxError::InvalidArgument)?;
        }
        Ok(())
    }

    /// Write an arbitrary DPCD range using native AUX requests.
    pub fn write_dpcd(&self, mut address: u32, input: &[u8]) -> Result<(), Sn65dsi86AuxError> {
        let _guard = self.aux_lock.lock();
        for chunk in input.chunks(AUX_MAX_PAYLOAD) {
            let mut payload = [0u8; AUX_MAX_PAYLOAD];
            payload[..chunk.len()].copy_from_slice(chunk);
            let sent = self.aux_transfer_locked(
                DpAuxRequest::NativeWrite,
                address,
                &mut payload[..chunk.len()],
            )?;
            if sent != chunk.len() {
                return Err(Sn65dsi86AuxError::ShortReply);
            }
            address = address
                .checked_add(chunk.len() as u32)
                .ok_or(Sn65dsi86AuxError::InvalidArgument)?;
        }
        Ok(())
    }

    fn read_edid_block(
        &self,
        block: usize,
        output: &mut [u8; EDID_BLOCK_SIZE],
    ) -> Result<(), Sn65dsi86AuxError> {
        let mut offset = [(block * EDID_BLOCK_SIZE) as u8];
        let sent =
            self.aux_transfer_locked(DpAuxRequest::I2cWriteMot, EDID_I2C_ADDRESS, &mut offset)?;
        if sent != offset.len() {
            return Err(Sn65dsi86AuxError::ShortReply);
        }

        let chunk_count = EDID_BLOCK_SIZE / AUX_MAX_PAYLOAD;
        for (index, chunk) in output.chunks_mut(AUX_MAX_PAYLOAD).enumerate() {
            let request = if index + 1 == chunk_count {
                DpAuxRequest::I2cRead
            } else {
                DpAuxRequest::I2cReadMot
            };
            let received = self.aux_transfer_locked(request, EDID_I2C_ADDRESS, chunk)?;
            if received != chunk.len() {
                return Err(Sn65dsi86AuxError::ShortReply);
            }
        }
        Ok(())
    }

    /// Read and checksum the base EDID and its first extension block.
    pub fn read_edid(&self) -> Result<Sn65dsi86Edid, Sn65dsi86AuxError> {
        let _guard = self.aux_lock.lock();
        let mut edid = Sn65dsi86Edid {
            bytes: [0; EDID_BLOCK_SIZE * EDID_MAX_BLOCKS],
            blocks: 0,
        };

        let mut base = [0; EDID_BLOCK_SIZE];
        self.read_edid_block(0, &mut base)?;
        if base[..EDID_HEADER.len()] != EDID_HEADER || !edid_checksum_valid(&base) {
            return Err(Sn65dsi86AuxError::InvalidArgument);
        }
        edid.bytes[..EDID_BLOCK_SIZE].copy_from_slice(&base);
        edid.blocks = 1;

        if base[EDID_EXTENSION_COUNT] != 0 {
            let mut extension = [0; EDID_BLOCK_SIZE];
            self.read_edid_block(1, &mut extension)?;
            if !edid_checksum_valid(&extension) {
                return Err(Sn65dsi86AuxError::InvalidArgument);
            }
            edid.bytes[EDID_BLOCK_SIZE..].copy_from_slice(&extension);
            edid.blocks = 2;
        }
        Ok(edid)
    }

    /// Select the board's 19.2 MHz reference clock and ignore the HPD input.
    ///
    /// CoachZ wires the bridge interrupt as HPD but declares `no-hpd`; the
    /// panel power sequence is controlled by the board instead.
    pub fn initialize_reference_clock(&self) -> Result<(), I2cError> {
        self.update_u8(HPD_DISABLE, HPD_IS_DISABLED, HPD_IS_DISABLED)?;
        // REFCLK_FREQ bits 3:1: selector 1 is 19.2 MHz.
        self.update_u8(DP_PLL_SOURCE, 0x0e, 1 << 1)
    }

    fn required_dp_rate_mbps(
        timing: DisplayTiming,
        dp_lanes: u8,
        output_bits_per_pixel: u8,
    ) -> Result<u32, Sn65dsi86LinkError> {
        if dp_lanes == 0 {
            return Err(Sn65dsi86LinkError::UnsupportedLink);
        }
        // Account for DisplayPort's 8b/10b encoding when selecting the
        // per-lane symbol rate for the selected bridge output format.
        let numerator = u64::from(timing.pixel_clock_khz) * u64::from(output_bits_per_pixel) * 10;
        let denominator = u64::from(dp_lanes) * 8 * 1_000;
        let rate = numerator.div_ceil(denominator);
        u32::try_from(rate).map_err(|_| Sn65dsi86LinkError::UnsupportedLink)
    }

    fn select_dp_rate(
        timing: DisplayTiming,
        dp_lanes: u8,
        sink_max_rate: u8,
        output_bits_per_pixel: u8,
    ) -> Result<u8, Sn65dsi86LinkError> {
        let required = Self::required_dp_rate_mbps(timing, dp_lanes, output_bits_per_pixel)?;
        let max_selector = match sink_max_rate {
            DP_LINK_RATE_1_62 => 1,
            DP_LINK_RATE_2_70 => 4,
            DP_LINK_RATE_5_40 => 7,
            _ => 7,
        };
        [1usize, 4, 7]
            .into_iter()
            .find(|selector| *selector <= max_selector && DP_RATE_MBPS[*selector] >= required)
            .and_then(|selector| u8::try_from(selector).ok())
            .ok_or(Sn65dsi86LinkError::UnsupportedLink)
    }

    fn configure_timing_registers(&self, timing: DisplayTiming) -> Result<(), Sn65dsi86LinkError> {
        let hback = timing
            .horizontal_back_porch()
            .ok_or(Sn65dsi86LinkError::InvalidTiming)?;
        let vback = timing
            .vertical_back_porch()
            .ok_or(Sn65dsi86LinkError::InvalidTiming)?;
        let hfront =
            u8::try_from(timing.hsync_offset).map_err(|_| Sn65dsi86LinkError::InvalidTiming)?;
        let vfront =
            u8::try_from(timing.vsync_offset).map_err(|_| Sn65dsi86LinkError::InvalidTiming)?;
        let hback = u8::try_from(hback).map_err(|_| Sn65dsi86LinkError::InvalidTiming)?;
        let vback = u8::try_from(vback).map_err(|_| Sn65dsi86LinkError::InvalidTiming)?;

        self.write_u8(ACTIVE_WIDTH_LOW, timing.hactive as u8)?;
        self.write_u8(ACTIVE_WIDTH_HIGH, (timing.hactive >> 8) as u8)?;
        self.write_u8(ACTIVE_HEIGHT_LOW, timing.vactive as u8)?;
        self.write_u8(ACTIVE_HEIGHT_HIGH, (timing.vactive >> 8) as u8)?;
        self.write_u8(HSYNC_WIDTH_LOW, timing.hsync_width as u8)?;
        self.write_u8(
            HSYNC_WIDTH_HIGH,
            ((timing.hsync_width >> 8) as u8 & !SYNC_PULSE_NEGATIVE)
                | if timing.hsync_positive {
                    0
                } else {
                    SYNC_PULSE_NEGATIVE
                },
        )?;
        self.write_u8(VSYNC_WIDTH_LOW, timing.vsync_width as u8)?;
        self.write_u8(
            VSYNC_WIDTH_HIGH,
            ((timing.vsync_width >> 8) as u8 & !SYNC_PULSE_NEGATIVE)
                | if timing.vsync_positive {
                    0
                } else {
                    SYNC_PULSE_NEGATIVE
                },
        )?;
        self.write_u8(HORIZONTAL_BACK_PORCH, hback)?;
        self.write_u8(VERTICAL_BACK_PORCH, vback)?;
        self.write_u8(HORIZONTAL_FRONT_PORCH, hfront)?;
        self.write_u8(VERTICAL_FRONT_PORCH, vfront)?;
        Ok(())
    }

    fn train_link(&self) -> Result<(), Sn65dsi86LinkError> {
        self.write_u8(PLL_ENABLE, 1)?;
        if self
            .wait_register(DP_PLL_SOURCE, PLL_TIMEOUT_US, |value| {
                value & DP_PLL_LOCKED != 0
            })
            .is_err()
        {
            return Err(Sn65dsi86LinkError::PllTimeout);
        }

        self.write_dpcd(DPCD_CONFIGURATION_SET, &[1])?;
        // The SN65 supports the alternate scrambler-reset method used by eDP.
        // Linux enables ASSR in the sink above and explicitly keeps the
        // bridge scrambler enabled before starting link training. Do not
        // inherit SCRAMBLE_DISABLE from an earlier firmware display owner.
        self.update_u8(TRAINING_SETTINGS, SCRAMBLER_DISABLED, 0)?;
        for _ in 0..LINK_TRAINING_RETRIES {
            self.write_u8(MAIN_LINK_MODE, MAIN_LINK_SEMI_AUTOMATIC_TRAINING)?;
            match self.wait_register(MAIN_LINK_MODE, LINK_TRAINING_TIMEOUT_US, |value| {
                value == MAIN_LINK_NORMAL || value == MAIN_LINK_OFF
            }) {
                Ok(MAIN_LINK_NORMAL) => return Ok(()),
                Ok(_) | Err(I2cError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(Sn65dsi86LinkError::LinkTrainingFailed)
    }

    fn clear_video_error_status(&self) -> Result<(), I2cError> {
        for register in ERROR_STATUS_START..=VIDEO_ERROR_STATUS_END {
            self.write_u8(register, 0xff)?;
        }
        Ok(())
    }

    /// Arm the bridge's DSI input-clock detector for diagnostics.
    ///
    /// A zero value in the DSI clock-range register asks the bridge to
    /// measure the incoming clock. The later diagnostic snapshot therefore
    /// distinguishes a programmed expectation from an observed DSI signal.
    pub fn arm_dsi_clock_detector(&self) -> Result<(), I2cError> {
        self.clear_video_error_status()?;
        self.write_u8(DSI_CLOCK, 0)
    }

    /// Configure and train the bridge for one DSI video mode.
    ///
    /// The native SC7180 path uses four DSI lanes. The eDP lane count is
    /// clamped to the sink capability reported through DPCD.
    pub fn configure_link(
        &self,
        timing: DisplayTiming,
        dsi_lanes: u8,
        dsi_bits_per_pixel: u8,
    ) -> Result<(), Sn65dsi86LinkError> {
        let timing = timing.validate()?;
        let output_bits_per_pixel = match dsi_bits_per_pixel {
            18 | 24 => dsi_bits_per_pixel,
            _ => return Err(Sn65dsi86LinkError::UnsupportedLink),
        };
        if !(1..=4).contains(&dsi_lanes) {
            return Err(Sn65dsi86LinkError::UnsupportedLink);
        }

        self.initialize_reference_clock()?;
        let mut sink = [0u8; 2];
        self.read_dpcd(DPCD_MAX_LINK_RATE, &mut sink)?;
        let dp_lanes = (sink[1] & DPCD_LANE_COUNT_MASK).min(4);
        if dp_lanes == 0 {
            return Err(Sn65dsi86LinkError::UnsupportedLink);
        }
        let dp_rate = Self::select_dp_rate(timing, dp_lanes, sink[0], output_bits_per_pixel)?;

        self.update_u8(
            DSI_LANES,
            DSI_CHANNEL_MODE_MASK | DSI_LANE_COUNT_MASK,
            DSI_SINGLE_CHANNEL_A | (4 - dsi_lanes) << 3,
        )?;
        // CoachZ routes the bridge's eDP lanes as DT data-lanes <0 1 2 3>.
        // Linux packs that identity mapping as 0xe4 at SN_LN_ASSIGN_REG.
        self.write_u8(DP_LANE_ASSIGNMENT, 0xe4)?;
        self.update_u8(ENHANCED_FRAME, DP_LANE_POLARITY_MASK, 0)?;
        self.update_u8(SSC_CONFIG, 0x7 << 4, dp_lanes.min(3) << 4)?;

        let dsi_clock_mhz = (u64::from(timing.pixel_clock_khz) * u64::from(dsi_bits_per_pixel)
            / u64::from(dsi_lanes)
            / 2
            / 1_000)
            .clamp(DSI_MIN_CLOCK_MHZ, DSI_MAX_CLOCK_MHZ);
        self.write_u8(DSI_CLOCK, (dsi_clock_mhz / DSI_CLOCK_STEP_MHZ) as u8)?;
        self.update_u8(DATA_RATE, 0xe0, dp_rate << 5)?;

        // Select the DP output width before training, matching the Linux
        // bridge enable sequence.
        self.update_u8(
            DATA_FORMAT,
            DATA_FORMAT_18BPP_RGB,
            if output_bits_per_pixel == 18 {
                DATA_FORMAT_18BPP_RGB
            } else {
                0
            },
        )?;

        self.update_u8(ENHANCED_FRAME, VIDEO_STREAM_ENABLED, 0)?;
        self.train_link()?;
        self.configure_timing_registers(timing)?;
        // SN65DSI86 requires the programmed video timings to settle before
        // VSTREAM is asserted. Linux follows the data-sheet recommendation
        // and waits 10 ms here.
        time::udelay(10_000);
        self.write_u8(COLOR_BAR, 5)?;
        // TI recommends clearing latched bring-up errors before examining the
        // active stream. Preserve F8 so the link-training result remains visible.
        self.clear_video_error_status()?;
        self.update_u8(ENHANCED_FRAME, VIDEO_STREAM_ENABLED, VIDEO_STREAM_ENABLED)?;
        Ok(())
    }

    /// Enable sink-controlled eDP backlight at full brightness.
    ///
    /// CoachZ normally uses the Chrome EC PWM path, so callers should only use
    /// this fallback when the sink advertises DPCD backlight control.
    pub fn enable_dpcd_backlight(&self) -> Result<(), Sn65dsi86AuxError> {
        self.write_dpcd(DPCD_BACKLIGHT_MODE, &[DPCD_BACKLIGHT_CONTROL_MODE])?;
        self.write_dpcd(DPCD_BACKLIGHT_BRIGHTNESS_MSB, &[0xff])?;
        self.write_dpcd(DPCD_DISPLAY_CONTROL, &[DPCD_BACKLIGHT_ENABLE])
    }

    /// Return the bridge's Device Tree phandle.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }
}

fn edid_checksum_valid(block: &[u8; EDID_BLOCK_SIZE]) -> bool {
    block
        .iter()
        .fold(0u8, |checksum, byte| checksum.wrapping_add(*byte))
        == 0
}

static BRIDGES: IrqSpinLock<Vec<Arc<Sn65dsi86>>> = IrqSpinLock::new(Vec::new());

/// Find a registered bridge by Device Tree phandle.
pub fn get_sn65dsi86_by_phandle(phandle: u32) -> Option<Arc<Sn65dsi86>> {
    BRIDGES
        .lock()
        .iter()
        .find(|bridge| bridge.phandle == phandle)
        .cloned()
}

fn read_i2c_address(device: &PlatformDeviceInfo) -> Result<I2cAddress, &'static str> {
    let address = device
        .property("reg")
        .and_then(|property| property.as_usize())
        .ok_or("ti-sn65dsi86: missing I2C address")?;
    if address > MAX_7BIT_ADDRESS {
        return Err("ti-sn65dsi86: unsupported I2C address");
    }

    Ok(I2cAddress::SevenBit(
        u8::try_from(address).map_err(|_| "ti-sn65dsi86: unsupported I2C address")?,
    ))
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("ti-sn65dsi86: missing phandle")
}

fn resolve_i2c_bus(device: &PlatformDeviceInfo) -> Result<(u32, Arc<dyn I2cBus>), &'static str> {
    let bus_phandle = device
        .parent_phandle()
        .ok_or("ti-sn65dsi86: missing parent I2C bus")?;
    match DeviceManager::get_manager().get_i2c_bus(bus_phandle) {
        Some(bus) => Ok((bus_phandle, bus)),
        None => {
            early_println!(
                "[ti-sn65dsi86] I2C bus phandle {:#x} is not ready, deferring",
                bus_phandle
            );
            probe_defer()
        }
    }
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn enable_bridge_gpio(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let Some(property) = device.property("enable-gpios") else {
        return Ok(());
    };
    let bytes = property.value();
    let controller_phandle = read_be_u32(bytes, 0).ok_or("ti-sn65dsi86: malformed enable-gpios")?;
    let pin = read_be_u32(bytes, 4).ok_or("ti-sn65dsi86: malformed enable-gpios")?;
    let flags = read_be_u32(bytes, 8).ok_or("ti-sn65dsi86: malformed enable-gpios")?;
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(controller_phandle)
        .ok_or_else(|| {
            early_println!(
                "[ti-sn65dsi86] GPIO controller {:#x} is not ready, deferring",
                controller_phandle
            );
            scarlet::device::manager::PROBE_DEFER
        })?;
    let asserted = flags & 1 == 0;
    controller.set_direction_output(pin, asserted);
    time::udelay(2_000);
    Ok(())
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    enable_bridge_gpio(device)?;
    let (bus_phandle, bus) = resolve_i2c_bus(device)?;
    let address = read_i2c_address(device)?;
    let phandle = read_phandle(device)?;
    let bridge = Arc::new(Sn65dsi86::new(bus, address, phandle, bus_phandle));

    if let Err(error) = bridge.verify_device_id() {
        early_println!(
            "[ti-sn65dsi86] device-ID read failed: bus={:#x} addr={:#x} error={:?}",
            bus_phandle,
            address.raw(),
            error
        );
        return Err("ti-sn65dsi86: unsupported or unreachable bridge");
    }

    let snapshot = bridge.diagnostic_snapshot().map_err(|error| {
        early_println!(
            "[ti-sn65dsi86] diagnostic read failed: bus={:#x} addr={:#x} error={:?}",
            bus_phandle,
            address.raw(),
            error
        );
        "ti-sn65dsi86: failed to read bridge state"
    })?;

    early_println!(
        "[ti-sn65dsi86] registered {} phandle={:#x} bus={:#x} addr={:#x} rev={:#x} pll={} stream={} hpd={} hpd-disabled={} dsi-lanes={:#x} dsi-clock={:#x} dp-lanes={:#x} dp-rate={:#x} link={:#x}",
        device.name(),
        phandle,
        bridge.bus_phandle,
        bridge.address.raw(),
        snapshot.revision,
        snapshot.dp_pll_locked(),
        snapshot.video_stream_enabled(),
        snapshot.hpd_asserted(),
        snapshot.hpd_disabled(),
        snapshot.dsi_lanes,
        snapshot.dsi_clock,
        snapshot.ssc_config,
        snapshot.data_rate,
        snapshot.main_link_mode,
    );

    let mut dpcd = [0; 3];
    match bridge.read_dpcd(0, &mut dpcd) {
        Ok(()) => early_println!(
            "[ti-sn65dsi86] sink DPCD revision={:#x} max-rate={:#x} max-lanes={:#x}",
            dpcd[0],
            dpcd[1],
            dpcd[2],
        ),
        Err(error) => early_println!("[ti-sn65dsi86] sink DPCD probe unavailable: {:?}", error),
    }

    BRIDGES.lock().push(bridge);
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver =
        PlatformDeviceDriver::new("ti-sn65dsi86", probe_fn, remove_fn, vec!["ti,sn65dsi86"]);
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_TI_SN65DSI86_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
