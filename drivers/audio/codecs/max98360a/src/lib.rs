// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! MAX98357A/MAX98360A GPIO-controlled I2S amplifier codec.
//!
//! These amplifiers have no register bus.  Playback power and mute are both
//! represented by the active state of the optional `sdmode-gpios` line, as in
//! Linux `sound/soc/codecs/max98357a.c`.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};

use scarlet::{
    device::{
        audio::{
            AUDIO_PCM_FORMAT_S16LE, AUDIO_PCM_FORMAT_S24LE3, AUDIO_PCM_FORMAT_S32LE, AudioCodec,
            AudioPcmParams,
        },
        gpio::GpioController,
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
    sync::IrqSpinLock,
    time,
};

struct CodecState {
    configured: bool,
    powered: bool,
    muted: bool,
    asserted: bool,
}

struct Max98360a {
    gpio: Option<Arc<dyn GpioController>>,
    pin: u32,
    active_low: bool,
    sdmode_delay_ms: u32,
    state: IrqSpinLock<CodecState>,
}

impl Max98360a {
    fn update_output(&self, state: &mut CodecState) {
        let asserted = state.configured && state.powered && !state.muted;
        if asserted && !state.asserted && self.sdmode_delay_ms != 0 {
            time::udelay(u64::from(self.sdmode_delay_ms) * 1_000);
        }
        if let Some(gpio) = &self.gpio {
            gpio.set_value(self.pin, asserted ^ self.active_low);
        }
        state.asserted = asserted;
    }
}

impl AudioCodec for Max98360a {
    fn configure_playback(
        &self,
        params: &AudioPcmParams,
        tx_mask: u32,
        slots: usize,
        slot_width: usize,
    ) -> Result<(), &'static str> {
        if !matches!(
            params.format,
            AUDIO_PCM_FORMAT_S16LE | AUDIO_PCM_FORMAT_S24LE3 | AUDIO_PCM_FORMAT_S32LE
        ) {
            return Err("max98360a: unsupported PCM format");
        }
        if !matches!(
            params.rate,
            8_000 | 16_000 | 32_000 | 44_100 | 48_000 | 88_200 | 96_000
        ) {
            return Err("max98360a: unsupported PCM rate");
        }
        if !(1..=2).contains(&params.channels) || tx_mask == 0 || slots < 2 {
            return Err("max98360a: invalid channel or slot configuration");
        }
        if !matches!(slot_width, 16 | 24 | 32) {
            return Err("max98360a: unsupported slot width");
        }

        let mut state = self.state.lock();
        state.configured = true;
        self.update_output(&mut state);
        Ok(())
    }

    fn set_playback_muted(&self, muted: bool) -> Result<(), &'static str> {
        let mut state = self.state.lock();
        state.muted = muted;
        self.update_output(&mut state);
        Ok(())
    }

    fn set_playback_powered(&self, powered: bool) -> Result<(), &'static str> {
        let mut state = self.state.lock();
        state.powered = powered;
        self.update_output(&mut state);
        Ok(())
    }
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0)
        .ok_or("max98360a: missing phandle")
}

fn probe(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let phandle = read_phandle(device)?;
    let (gpio, pin, active_low) = if let Some(property) = device.property("sdmode-gpios") {
        let bytes = property.value();
        let controller_phandle =
            read_be_u32(bytes, 0).ok_or("max98360a: malformed SD_MODE GPIO")?;
        let pin = read_be_u32(bytes, 4).ok_or("max98360a: malformed SD_MODE GPIO")?;
        let flags = read_be_u32(bytes, 8).unwrap_or(0);
        let controller = DeviceManager::get_manager()
            .get_gpio_controller(controller_phandle)
            .ok_or(PROBE_DEFER)?;
        controller.set_direction_output(pin, flags & 1 != 0);
        (Some(controller), pin, flags & 1 != 0)
    } else {
        (None, 0, false)
    };
    let sdmode_delay_ms = device
        .property("sdmode-delay")
        .and_then(|property| read_be_u32(property.value(), 0))
        .unwrap_or(0);
    let codec = Arc::new(Max98360a {
        gpio,
        pin,
        active_low,
        sdmode_delay_ms,
        state: IrqSpinLock::new(CodecState {
            configured: false,
            powered: false,
            muted: true,
            asserted: false,
        }),
    });
    DeviceManager::get_manager().register_audio_codec(phandle, codec);
    early_println!(
        "[max98360a] registered phandle={:#x} sdmode_gpio={} active_low={} delay_ms={}",
        phandle,
        pin,
        active_low,
        sdmode_delay_ms,
    );
    Ok(())
}

fn remove(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "max98360a",
            probe,
            remove,
            vec!["maxim,max98357a", "maxim,max98360a"],
        )),
        DriverPriority::Standard,
    );
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_MAX98360A_ANCHOR: fn() = force_link;

/// Keep the external MAX98360A codec linked into module builds.
#[inline(never)]
pub fn force_link() {}
