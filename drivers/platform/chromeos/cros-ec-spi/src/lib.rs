// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! ChromeOS Embedded Controller host-command transport over SPI.
//!
//! The initial consumer is the CoachZ panel backlight, which is driven by an
//! EC PWM channel rather than by the SN65DSI86 bridge.
//!
//! # Provenance
//!
//! Protocol framing and status values follow ChromiumOS EC protocol 3 and
//! Depthcharge's `drivers/ec/cros/spi.c` transport.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    device::{
        DeviceInfo,
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
        spi::{SpiBus, SpiError, SpiTransfer},
    },
    println,
    sync::IrqSpinLock,
    time,
};

const HOST_REQUEST_VERSION: u8 = 3;
const HOST_RESPONSE_VERSION: u8 = 3;
const HOST_HEADER_BYTES: usize = 8;

const SPI_FRAME_START: u8 = 0xec;
const SPI_RX_BAD_DATA: u8 = 0xfb;
const SPI_NOT_READY: u8 = 0xfc;

const COMMAND_PWM_SET_DUTY: u16 = 0x0025;
const PWM_DISPLAY_LIGHT: u8 = 2;
const PWM_FULL_SCALE: u32 = 0xffff;

// At the inherited 1.01 MHz Trogdor EC clock this is about 32 ms of polling.
// PWM_SET_DUTY completes quickly, while keeping the allocation bounded and the
// complete response inside one CS assertion.
const RESPONSE_CLOCK_BYTES: usize = 4096;
const CHIP_SELECT_COOLDOWN_US: u64 = 200;

/// Failure returned by a Chrome EC SPI host command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrosEcError {
    /// The underlying SPI controller rejected or timed out the transaction.
    Spi(SpiError),
    /// The EC rejected the bytes before accepting the host command.
    BadRequest,
    /// The EC was not ready to receive a host command.
    NotReady,
    /// No response frame appeared in the bounded polling window.
    NoResponse,
    /// The response header, length, or checksum was invalid.
    InvalidResponse,
    /// The EC returned a non-zero protocol result.
    EcResult(u16),
    /// A caller supplied a value outside the command's valid range.
    InvalidArgument,
}

impl From<SpiError> for CrosEcError {
    fn from(error: SpiError) -> Self {
        Self::Spi(error)
    }
}

/// One Chrome EC reached through a Scarlet SPI bus.
pub struct CrosEcSpi {
    bus: Arc<dyn SpiBus>,
    phandle: u32,
    chip_select: u8,
    speed_hz: u32,
    command_lock: IrqSpinLock<()>,
}

impl CrosEcSpi {
    fn new(bus: Arc<dyn SpiBus>, phandle: u32, chip_select: u8, maximum_speed_hz: u32) -> Self {
        Self {
            speed_hz: bus.bus_speed().min(maximum_speed_hz),
            bus,
            phandle,
            chip_select,
            command_lock: IrqSpinLock::new(()),
        }
    }

    /// Device-tree phandle of this EC.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }

    fn build_request(command: u16, version: u8, payload: &[u8]) -> Vec<u8> {
        let data_len = payload.len() as u16;
        let mut packet = Vec::with_capacity(HOST_HEADER_BYTES + payload.len());
        packet.extend_from_slice(&[
            HOST_REQUEST_VERSION,
            0,
            command as u8,
            (command >> 8) as u8,
            version,
            0,
            data_len as u8,
            (data_len >> 8) as u8,
        ]);
        packet.extend_from_slice(payload);
        let checksum = packet.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        packet[1] = checksum.wrapping_neg();
        packet
    }

    fn decode_response<'a>(wire: &'a [u8]) -> Result<&'a [u8], CrosEcError> {
        let mut frame_offset = None;
        for (index, byte) in wire.iter().copied().enumerate() {
            match byte {
                SPI_FRAME_START => {
                    frame_offset = Some(index + 1);
                    break;
                }
                SPI_RX_BAD_DATA => return Err(CrosEcError::BadRequest),
                SPI_NOT_READY => return Err(CrosEcError::NotReady),
                _ => {}
            }
        }
        let header_offset = frame_offset.ok_or(CrosEcError::NoResponse)?;
        let header = wire
            .get(header_offset..header_offset + HOST_HEADER_BYTES)
            .ok_or(CrosEcError::InvalidResponse)?;
        if header[0] != HOST_RESPONSE_VERSION || header[6] != 0 || header[7] != 0 {
            return Err(CrosEcError::InvalidResponse);
        }
        let result = u16::from_le_bytes([header[2], header[3]]);
        if result != 0 {
            return Err(CrosEcError::EcResult(result));
        }
        let data_len = usize::from(u16::from_le_bytes([header[4], header[5]]));
        let packet_end = header_offset
            .checked_add(HOST_HEADER_BYTES)
            .and_then(|value| value.checked_add(data_len))
            .ok_or(CrosEcError::InvalidResponse)?;
        let packet = wire
            .get(header_offset..packet_end)
            .ok_or(CrosEcError::InvalidResponse)?;
        if packet.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
            return Err(CrosEcError::InvalidResponse);
        }
        Ok(&packet[HOST_HEADER_BYTES..])
    }

    /// Send one protocol-3 host command and return its response payload.
    pub fn command(
        &self,
        command: u16,
        version: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, CrosEcError> {
        if payload.len() > u16::MAX as usize {
            return Err(CrosEcError::InvalidArgument);
        }
        let _guard = self.command_lock.lock();
        time::udelay(CHIP_SELECT_COOLDOWN_US);

        let request = Self::build_request(command, version, payload);
        let mut segments = vec![
            SpiTransfer::write(self.chip_select, &request),
            SpiTransfer::read(self.chip_select, RESPONSE_CLOCK_BYTES),
        ];
        for segment in &mut segments {
            segment.speed_hz = self.speed_hz;
        }
        self.bus.transfer(&mut segments)?;
        Self::decode_response(&segments[1].data).map(<[u8]>::to_vec)
    }

    /// Set the display backlight to an integer percentage.
    pub fn set_display_backlight_percent(&self, percent: u8) -> Result<(), CrosEcError> {
        if percent > 100 {
            return Err(CrosEcError::InvalidArgument);
        }
        let duty = u16::try_from(u32::from(percent) * PWM_FULL_SCALE / 100)
            .map_err(|_| CrosEcError::InvalidArgument)?;
        let payload = [duty as u8, (duty >> 8) as u8, PWM_DISPLAY_LIGHT, 0];
        let response = self.command(COMMAND_PWM_SET_DUTY, 0, &payload)?;
        if !response.is_empty() {
            return Err(CrosEcError::InvalidResponse);
        }
        Ok(())
    }
}

static CONTROLLERS: IrqSpinLock<Vec<Arc<CrosEcSpi>>> = IrqSpinLock::new(Vec::new());

/// Look up a probed Chrome EC by its device-tree phandle.
pub fn get_cros_ec_spi_by_phandle(phandle: u32) -> Option<Arc<CrosEcSpi>> {
    CONTROLLERS
        .lock()
        .iter()
        .find(|controller| controller.phandle() == phandle)
        .cloned()
}

fn read_u32_property(device: &PlatformDeviceInfo, name: &str) -> Option<u32> {
    device
        .property(name)
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let bus_phandle = device
        .parent_phandle()
        .ok_or("cros-ec-spi: missing parent SPI bus")?;
    let bus = match DeviceManager::get_manager().get_spi_bus(bus_phandle) {
        Some(bus) => bus,
        None => {
            println!(
                "[cros-ec-spi] SPI bus phandle {:#x} is not ready, deferring",
                bus_phandle
            );
            return probe_defer();
        }
    };
    let phandle = read_u32_property(device, "phandle")
        .or_else(|| read_u32_property(device, "linux,phandle"))
        .ok_or("cros-ec-spi: missing phandle")?;
    let chip_select = read_u32_property(device, "reg")
        .and_then(|value| u8::try_from(value).ok())
        .ok_or("cros-ec-spi: invalid chip select")?;
    let maximum_speed = read_u32_property(device, "spi-max-frequency").unwrap_or(1_010_000);
    let controller = Arc::new(CrosEcSpi::new(bus, phandle, chip_select, maximum_speed));
    CONTROLLERS.lock().push(controller.clone());
    println!(
        "[cros-ec-spi] registered {} phandle={:#x} bus={:#x} cs={} speed={} Hz",
        device.name(),
        phandle,
        bus_phandle,
        chip_select,
        controller.speed_hz,
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "cros-ec-spi",
        probe_fn,
        remove_fn,
        vec!["google,cros-ec-spi"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_CROS_EC_SPI_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
