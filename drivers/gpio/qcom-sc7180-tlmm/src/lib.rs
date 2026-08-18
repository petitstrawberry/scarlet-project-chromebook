// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 Top-Level Mode Multiplexer and GPIO controller.
//!
//! SC7180 distributes GPIO register windows across west, north, and south
//! tiles. Each logical GPIO keeps its global pin number as the register index
//! within the owning tile. This module exposes the controller through
//! Scarlet's generic [`GpioController`] interface.
//!
//! # Provenance
//!
//! The tile assignment and register layout are adapted from coreboot's
//! `src/soc/qualcomm/sc7180/include/soc/gpio.h` and U-Boot's
//! `drivers/pinctrl/qcom/pinctrl-sc7180.c`.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch::mmio,
    device::{
        events::InterruptCapableDevice,
        gpio::{GpioController, GpioIrqTrigger, GpioPull},
        manager::{DeviceManager, DriverPriority},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    println, vm,
};

const PIN_COUNT: usize = 119;
const PIN_STRIDE: usize = 0x1000;

const CONFIG: usize = 0x0;
const INPUT_OUTPUT: usize = 0x4;
const INTERRUPT_CONFIG: usize = 0x8;
const INTERRUPT_STATUS: usize = 0xc;

const CONFIG_PULL_MASK: u32 = 0x3;
const CONFIG_FUNCTION_MASK: u32 = 0xf << 2;
const CONFIG_OUTPUT_ENABLE: u32 = 1 << 9;
const CONFIG_EGPIO_PRESENT: u32 = 1 << 11;
const INPUT_VALUE: u32 = 1;
const OUTPUT_VALUE: u32 = 1 << 1;

const INTERRUPT_ENABLE: u32 = 1;
const INTERRUPT_POLARITY_HIGH: u32 = 1 << 1;
const INTERRUPT_DETECT_MASK: u32 = 0x3 << 2;
const INTERRUPT_RAW_STATUS_ENABLE: u32 = 1 << 4;

#[derive(Clone, Copy)]
#[repr(usize)]
enum Tile {
    West = 0,
    North = 1,
    South = 2,
}

use Tile::{North, South, West};

// Indexes are architectural SC7180 GPIO numbers; values select the physical
// TLMM tile that owns that GPIO.
const PIN_TILES: [Tile; PIN_COUNT] = [
    South, South, South, South, North, North, North, North, North, North, North, North, South,
    South, South, South, South, South, South, South, South, North, North, South, South, South,
    South, South, South, North, South, North, North, North, South, South, South, South, South,
    South, South, South, North, North, North, North, North, North, North, West, West, West, West,
    West, West, West, West, West, West, North, North, North, North, North, North, North, North,
    North, North, West, North, North, North, West, West, West, West, West, West, West, West, West,
    West, West, West, West, North, North, North, North, North, North, North, North, South, West,
    West, West, West, West, West, North, North, North, West, North, North, West, South, South,
    North, North, North, North, North, West, West, West, West,
];

/// SC7180 GPIO controller backed by its three TLMM tiles.
pub struct Sc7180Tlmm {
    tiles: [usize; 3],
}

impl Sc7180Tlmm {
    fn new(tiles: [usize; 3]) -> Self {
        Self { tiles }
    }

    fn register(&self, pin: u32, offset: usize) -> Option<usize> {
        let pin = usize::try_from(pin).ok()?;
        let tile = *PIN_TILES.get(pin)? as usize;
        self.tiles[tile]
            .checked_add(pin.checked_mul(PIN_STRIDE)?)?
            .checked_add(offset)
    }

    fn read(&self, pin: u32, offset: usize) -> Option<u32> {
        let register = self.register(pin, offset)?;
        // SAFETY: every mapped tile spans the complete SC7180 TLMM window and
        // `register` rejects logical pins outside the SoC pin table.
        Some(unsafe { mmio::read32(register) })
    }

    fn write(&self, pin: u32, offset: usize, value: u32) -> bool {
        let Some(register) = self.register(pin, offset) else {
            return false;
        };
        // SAFETY: every mapped tile spans the complete SC7180 TLMM window and
        // `register` rejects logical pins outside the SoC pin table.
        unsafe { mmio::write32(register, value) };
        true
    }

    fn update(&self, pin: u32, offset: usize, clear: u32, set: u32) {
        if let Some(value) = self.read(pin, offset) {
            let _ = self.write(pin, offset, (value & !clear) | set);
        }
    }

    fn configure_gpio(&self, pin: u32, output: bool) {
        let Some(current) = self.read(pin, CONFIG) else {
            return;
        };
        let egpio = current & CONFIG_EGPIO_PRESENT;
        let output_enable = if output { CONFIG_OUTPUT_ENABLE } else { 0 };
        // GPIO function 0, no pull, 2 mA drive. Preserve the EGPIO-present
        // indicator because firmware owns that capability bit.
        let value = egpio | output_enable;
        let _ = self.write(pin, CONFIG, value);
    }
}

impl GpioController for Sc7180Tlmm {
    fn set_direction_output(&self, pin: u32, value: bool) {
        self.set_value(pin, value);
        self.configure_gpio(pin, true);
    }

    fn set_direction_input(&self, pin: u32) {
        self.configure_gpio(pin, false);
    }

    fn set_value(&self, pin: u32, value: bool) {
        let _ = self.write(pin, INPUT_OUTPUT, if value { OUTPUT_VALUE } else { 0 });
    }

    fn get_value(&self, pin: u32) -> bool {
        self.read(pin, INPUT_OUTPUT)
            .is_some_and(|value| value & INPUT_VALUE != 0)
    }

    fn set_pull(&self, pin: u32, pull: GpioPull) {
        let value = match pull {
            GpioPull::None => 0,
            GpioPull::Down => 1,
            GpioPull::Up => 3,
        };
        self.update(pin, CONFIG, CONFIG_PULL_MASK, value);
    }

    fn set_function(&self, pin: u32, function: u8) {
        let value = (u32::from(function) << 2) & CONFIG_FUNCTION_MASK;
        self.update(pin, CONFIG, CONFIG_FUNCTION_MASK, value);
    }

    fn enable_irq(&self, pin: u32, trigger: GpioIrqTrigger) {
        let (detect, polarity) = match trigger {
            GpioIrqTrigger::HighLevel => (0, INTERRUPT_POLARITY_HIGH),
            GpioIrqTrigger::LowLevel => (0, 0),
            GpioIrqTrigger::RisingEdge => (1 << 2, INTERRUPT_POLARITY_HIGH),
            GpioIrqTrigger::FallingEdge => (2 << 2, 0),
        };
        self.update(
            pin,
            INTERRUPT_CONFIG,
            INTERRUPT_DETECT_MASK | INTERRUPT_POLARITY_HIGH,
            detect | polarity | INTERRUPT_RAW_STATUS_ENABLE | INTERRUPT_ENABLE,
        );
    }

    fn disable_irq(&self, pin: u32) {
        self.update(pin, INTERRUPT_CONFIG, INTERRUPT_ENABLE, 0);
    }

    fn ack_irq(&self, pin: u32) {
        let _ = self.write(pin, INTERRUPT_STATUS, 0);
    }

    fn request_irq(
        &self,
        _pin: u32,
        _trigger: GpioIrqTrigger,
        _handler: Arc<dyn InterruptCapableDevice>,
    ) -> bool {
        // The SC7180 TLMM parent interrupt demultiplexer is intentionally kept
        // out of this initial display-focused module.
        false
    }

    fn free_irq(&self, pin: u32) {
        self.disable_irq(pin);
    }
}

fn device_phandle(device: &PlatformDeviceInfo) -> Result<u32, &'static str> {
    device
        .property("phandle")
        .or_else(|| device.property("linux,phandle"))
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("qcom-sc7180-tlmm: missing phandle")
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let resources: Vec<_> = device
        .get_resources()
        .iter()
        .filter(|resource| matches!(resource.res_type, PlatformDeviceResourceType::MEM))
        .collect();
    if resources.len() < 3 {
        return Err("qcom-sc7180-tlmm: expected west, north, and south resources");
    }

    let mut tiles = [0usize; 3];
    for (index, resource) in resources.iter().take(3).enumerate() {
        let size = resource
            .end
            .checked_sub(resource.start)
            .and_then(|value| value.checked_add(1))
            .ok_or("qcom-sc7180-tlmm: invalid memory resource")?;
        tiles[index] =
            vm::ioremap(resource.start, size).map_err(|_| "qcom-sc7180-tlmm: ioremap failed")?;
    }

    let phandle = device_phandle(device)?;
    DeviceManager::get_manager()
        .register_gpio_controller(phandle, Arc::new(Sc7180Tlmm::new(tiles)));
    println!(
        "[qcom-sc7180-tlmm] registered {} GPIOs (phandle={:#x})",
        PIN_COUNT, phandle
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "qcom-sc7180-tlmm",
        probe_fn,
        remove_fn,
        vec!["qcom,sc7180-pinctrl"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Core);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_QCOM_SC7180_TLMM_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
