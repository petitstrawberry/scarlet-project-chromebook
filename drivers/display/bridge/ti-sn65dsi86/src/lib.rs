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

const DEVICE_ID: [u8; 8] = *b"68ISD   ";
const DP_PLL_LOCKED: u8 = 1 << 7;
const VIDEO_STREAM_ENABLED: u8 = 1 << 3;
const HPD_IS_DISABLED: u8 = 1 << 0;
const HPD_DEBOUNCED_STATE: u8 = 1 << 4;

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
}

impl Sn65dsi86 {
    fn new(bus: Arc<dyn I2cBus>, address: I2cAddress, phandle: u32, bus_phandle: u32) -> Self {
        Self {
            bus,
            address,
            phandle,
            bus_phandle,
        }
    }

    fn read_exact<const N: usize>(&self, register: u8) -> Result<[u8; N], I2cError> {
        let mut messages = vec![
            I2cMessage::write(self.address, &[register], false),
            I2cMessage::read(self.address, N, true),
        ];
        self.bus.transfer(&mut messages)?;

        let mut value = [0; N];
        value.copy_from_slice(&messages[1].data);
        Ok(value)
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

    /// Return the bridge's Device Tree phandle.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }
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
