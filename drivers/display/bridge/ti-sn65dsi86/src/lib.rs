// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! TI SN65DSI86 MIPI DSI to embedded DisplayPort bridge.
//!
//! This first stage binds the bridge as an I2C client, verifies its device ID,
//! and exposes non-destructive diagnostics to later native DSI/display
//! modules. It deliberately preserves the display state inherited from
//! Depthcharge and U-Boot; native link programming belongs in the DSI/DPU
//! takeover stage.
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
const ENHANCED_FRAME: u8 = 0x5a;
const HPD_DISABLE: u8 = 0x5c;
const SSC_CONFIG: u8 = 0x93;
const DATA_RATE: u8 = 0x94;
const MAIN_LINK_MODE: u8 = 0x96;
const AUX_WRITE_DATA: u8 = 0x64;
const AUX_ADDRESS_HIGH: u8 = 0x74;
const AUX_LENGTH: u8 = 0x77;
const AUX_COMMAND: u8 = 0x78;
const AUX_READ_DATA: u8 = 0x79;
const AUX_STATUS: u8 = 0xf4;

const DEVICE_ID: [u8; 8] = *b"68ISD   ";
const DP_PLL_LOCKED: u8 = 1 << 7;
const VIDEO_STREAM_ENABLED: u8 = 1 << 3;
const HPD_IS_DISABLED: u8 = 1 << 0;
const HPD_DEBOUNCED_STATE: u8 = 1 << 4;
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

const EDID_I2C_ADDRESS: u32 = 0x50;
const EDID_BLOCK_SIZE: usize = 128;
const EDID_MAX_BLOCKS: usize = 2;
const EDID_EXTENSION_COUNT: usize = 0x7e;
const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

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
    /// Raw enhanced-frame and video-stream register.
    pub enhanced_frame: u8,
    /// Raw HPD state/control register.
    pub hpd: u8,
    /// Raw spread-spectrum and DP lane-count register.
    pub ssc_config: u8,
    /// Raw DP data-rate selector.
    pub data_rate: u8,
    /// Raw main-link mode register.
    pub main_link_mode: u8,
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
            enhanced_frame: self.read_u8(ENHANCED_FRAME)?,
            hpd: self.read_u8(HPD_DISABLE)?,
            ssc_config: self.read_u8(SSC_CONFIG)?,
            data_rate: self.read_u8(DATA_RATE)?,
            main_link_mode: self.read_u8(MAIN_LINK_MODE)?,
        })
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

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
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
