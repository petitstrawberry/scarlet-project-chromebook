// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Qualcomm SC7180 Top-Level Mode Multiplexer and GPIO controller.
//!
//! SC7180 distributes GPIO register windows across west, north, and south
//! tiles. Each logical GPIO keeps its global pin number as the register index
//! within the owning tile. This module exposes GPIO operations through
//! [`GpioController`] and keeps SC7180-specific pin/function decoding behind
//! [`PinctrlController`].
//!
//! # Provenance
//!
//! The tile assignment and register layout are adapted from coreboot's
//! `src/soc/qualcomm/sc7180/include/soc/gpio.h`, U-Boot's
//! `drivers/pinctrl/qcom/pinctrl-sc7180.c`, and Linux's
//! `drivers/pinctrl/qcom/pinctrl-{msm,sc7180}.c`.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use scarlet::{
    arch,
    arch::mmio,
    device::{
        events::InterruptCapableDevice,
        gpio::{GpioController, GpioIrqTrigger, GpioPull},
        manager::{DeviceManager, DriverPriority},
        pinctrl::{PinctrlBias, PinctrlController, PinctrlError, PinctrlState},
        platform::{
            PlatformDeviceDriver, PlatformDeviceInfo, resource::PlatformDeviceResourceType,
        },
    },
    interrupt::{InterruptClaim, InterruptId, InterruptResult},
    println,
    sync::IrqSpinLock,
    vm,
};

const PIN_COUNT: usize = 119;
const PIN_STRIDE: usize = 0x1000;

const CONFIG: usize = 0x0;
const INPUT_OUTPUT: usize = 0x4;
const INTERRUPT_CONFIG: usize = 0x8;
const INTERRUPT_STATUS: usize = 0xc;

const CONFIG_PULL_MASK: u32 = 0x3;
const CONFIG_FUNCTION_MASK: u32 = 0xf << 2;
const CONFIG_DRIVE_STRENGTH_MASK: u32 = 0x7 << 6;
const CONFIG_OUTPUT_ENABLE: u32 = 1 << 9;
const INPUT_VALUE: u32 = 1;
const OUTPUT_VALUE: u32 = 1 << 1;

const INTERRUPT_ENABLE: u32 = 1;
const INTERRUPT_POLARITY_HIGH: u32 = 1 << 1;
const INTERRUPT_DETECT_MASK: u32 = 0x3 << 2;
const INTERRUPT_RAW_STATUS_ENABLE: u32 = 1 << 4;
const INTERRUPT_TARGET_MASK: u32 = 0x7 << 5;
const INTERRUPT_TARGET_KPSS: u32 = 3 << 5;
const INTERRUPT_STATUS_PENDING: u32 = 1;

struct GpioIrqHandler {
    trigger: GpioIrqTrigger,
    device: Arc<dyn InterruptCapableDevice>,
}

struct GpioIrqSlot {
    generation: u64,
    enabled: bool,
    handler: Option<GpioIrqHandler>,
}

struct PendingGpioIrq {
    pin: u32,
    generation: u64,
    trigger: GpioIrqTrigger,
    device: Arc<dyn InterruptCapableDevice>,
}

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
    summary_irq: InterruptId,
    irq_handlers: IrqSpinLock<Vec<GpioIrqSlot>>,
}

impl Sc7180Tlmm {
    fn new(tiles: [usize; 3], summary_irq: InterruptId) -> Self {
        Self {
            tiles,
            summary_irq,
            irq_handlers: IrqSpinLock::new(
                (0..PIN_COUNT)
                    .map(|_| GpioIrqSlot {
                        generation: 0,
                        enabled: false,
                        handler: None,
                    })
                    .collect(),
            ),
        }
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
        let output_enable = if output { CONFIG_OUTPUT_ENABLE } else { 0 };
        // Select GPIO function 0 and update direction without disturbing the
        // DTS/firmware-programmed pull, drive strength, or EGPIO fields.
        self.update(
            pin,
            CONFIG,
            CONFIG_FUNCTION_MASK | CONFIG_OUTPUT_ENABLE,
            output_enable,
        );
    }

    fn named_function(pin: u32, function: &str) -> Option<u8> {
        match function {
            "gpio" if pin < PIN_COUNT as u32 => Some(0),
            // Linux's SC7180 PINGROUP table places qup11_i2c in mux slot 1
            // for GPIO6 (SDA) and GPIO7 (SCL).
            "qup11_i2c" if matches!(pin, 6 | 7) => Some(1),
            _ => None,
        }
    }

    fn named_pin(pin: &str) -> Option<u32> {
        let pin: u32 = pin.strip_prefix("gpio")?.parse().ok()?;
        (pin < PIN_COUNT as u32).then_some(pin)
    }

    fn drive_strength_selector(milliamps: u32) -> Option<u32> {
        ((2..=16).contains(&milliamps) && milliamps.is_multiple_of(2)).then_some(milliamps / 2 - 1)
    }

    fn configure_drive_strength(&self, pin: u32, selector: u32) {
        self.update(pin, CONFIG, CONFIG_DRIVE_STRENGTH_MASK, selector << 6);
    }

    fn configure_input(&self, pin: u32) {
        self.update(pin, CONFIG, CONFIG_OUTPUT_ENABLE, 0);
    }

    fn configure_output(&self, pin: u32, value: bool) {
        self.set_value(pin, value);
        self.update(pin, CONFIG, CONFIG_OUTPUT_ENABLE, CONFIG_OUTPUT_ENABLE);
    }

    fn configure_irq(&self, pin: u32, trigger: GpioIrqTrigger, enabled: bool) {
        let (detect, polarity) = match trigger {
            GpioIrqTrigger::HighLevel => (0, INTERRUPT_POLARITY_HIGH),
            GpioIrqTrigger::LowLevel => (0, 0),
            GpioIrqTrigger::RisingEdge => (1 << 2, INTERRUPT_POLARITY_HIGH),
            // SC7180 uses detection value 2 with the polarity bit set for a
            // falling edge. The polarity bit is not the sampled GPIO level
            // when edge detection is selected.
            GpioIrqTrigger::FallingEdge => (2 << 2, INTERRUPT_POLARITY_HIGH),
        };
        let enable = if enabled {
            INTERRUPT_RAW_STATUS_ENABLE | INTERRUPT_ENABLE
        } else {
            0
        };
        self.update(
            pin,
            INTERRUPT_CONFIG,
            INTERRUPT_DETECT_MASK
                | INTERRUPT_POLARITY_HIGH
                | INTERRUPT_TARGET_MASK
                | INTERRUPT_RAW_STATUS_ENABLE
                | INTERRUPT_ENABLE,
            detect | polarity | INTERRUPT_TARGET_KPSS | enable,
        );
    }

    fn pin_pending(&self, pin: u32) -> bool {
        self.read(pin, INTERRUPT_STATUS)
            .is_some_and(|status| status & INTERRUPT_STATUS_PENDING != 0)
    }

    fn mask_irq_for_delivery(&self, pin: u32, trigger: GpioIrqTrigger) {
        let clear = match trigger {
            GpioIrqTrigger::HighLevel | GpioIrqTrigger::LowLevel => {
                INTERRUPT_ENABLE | INTERRUPT_RAW_STATUS_ENABLE
            }
            // Preserve RAW_STATUS_EN for an edge source while its callback is
            // running so another edge can still latch for the next summary
            // IRQ, matching Linux's TLMM mask behavior.
            GpioIrqTrigger::RisingEdge | GpioIrqTrigger::FallingEdge => INTERRUPT_ENABLE,
        };
        self.update(pin, INTERRUPT_CONFIG, clear, 0);
    }

    fn registration_matches(&self, pending: &PendingGpioIrq) -> bool {
        self.irq_handlers
            .lock()
            .get(pending.pin as usize)
            .is_some_and(|slot| {
                slot.generation == pending.generation
                    && slot.enabled
                    && slot.handler.as_ref().is_some_and(|handler| {
                        handler.trigger == pending.trigger
                            && Arc::ptr_eq(&handler.device, &pending.device)
                    })
            })
    }

    fn service_pending_irqs(&self) -> InterruptResult<InterruptClaim> {
        let (pending, cleared_stale) = {
            let handlers = self.irq_handlers.lock();
            // Keep hard-IRQ dispatch allocation-free. The fixed array is
            // bounded by SC7180's architectural GPIO count.
            let mut pending: [Option<PendingGpioIrq>; PIN_COUNT] = core::array::from_fn(|_| None);
            let mut pending_count = 0;
            let mut cleared_stale = false;

            for (pin, slot) in handlers.iter().enumerate() {
                let pin = pin as u32;
                if !self.pin_pending(pin) {
                    continue;
                }

                if let Some(handler) = slot.handler.as_ref().filter(|_| slot.enabled) {
                    // Quiesce and acknowledge under the same irq-safe lock
                    // that protects registration generations. free_irq() or a
                    // replacement cannot interleave with this snapshot.
                    self.mask_irq_for_delivery(pin, handler.trigger);
                    self.ack_irq(pin);
                    pending[pending_count] = Some(PendingGpioIrq {
                        pin,
                        generation: slot.generation,
                        trigger: handler.trigger,
                        device: handler.device.clone(),
                    });
                    pending_count += 1;
                } else {
                    // Firmware or a previous owner may have left an
                    // unregistered GPIO enabled. Drain it so the level-high
                    // SPI 208 summary cannot remain asserted forever.
                    self.update(
                        pin,
                        INTERRUPT_CONFIG,
                        INTERRUPT_ENABLE | INTERRUPT_RAW_STATUS_ENABLE,
                        0,
                    );
                    self.ack_irq(pin);
                    cleared_stale = true;
                }
            }

            (pending, cleared_stale)
        };

        if pending[0].is_none() {
            return Ok(if cleared_stale {
                InterruptClaim::Handled
            } else {
                InterruptClaim::NotMine
            });
        }

        let mut reschedule = false;
        for irq in pending.into_iter().flatten() {
            // A free/re-register that happened after the snapshot invalidates
            // this delivery. Never invoke a replacement handler for status
            // latched by the previous generation.
            if !self.registration_matches(&irq) {
                continue;
            }

            let reenable = match irq.device.claim_interrupt() {
                Ok(InterruptClaim::Reschedule) => {
                    reschedule = true;
                    true
                }
                Ok(InterruptClaim::Handled | InterruptClaim::NotMine) => true,
                Ok(InterruptClaim::Deferred) => {
                    println!(
                        "[qcom-sc7180-tlmm] ERROR: GPIO {} returned Deferred; source left masked",
                        irq.pin
                    );
                    false
                }
                Err(error) => {
                    println!(
                        "[qcom-sc7180-tlmm] ERROR: GPIO {} handler failed: {}; source left masked",
                        irq.pin, error
                    );
                    false
                }
            };

            // Validate the generation again after running the callback with no
            // TLMM lock held. A callback may free its own IRQ, and another CPU
            // may install a replacement in the meantime.
            let handlers = self.irq_handlers.lock();
            let still_registered = handlers.get(irq.pin as usize).is_some_and(|slot| {
                slot.generation == irq.generation
                    && slot.enabled
                    && slot.handler.as_ref().is_some_and(|handler| {
                        handler.trigger == irq.trigger && Arc::ptr_eq(&handler.device, &irq.device)
                    })
            });
            if reenable && still_registered {
                self.configure_irq(irq.pin, irq.trigger, true);
            }
        }

        Ok(if reschedule {
            InterruptClaim::Reschedule
        } else {
            InterruptClaim::Handled
        })
    }
}

impl InterruptCapableDevice for Sc7180Tlmm {
    fn handle_interrupt(&self) -> InterruptResult<()> {
        let _ = self.service_pending_irqs()?;
        Ok(())
    }

    fn interrupt_id(&self) -> Option<InterruptId> {
        Some(self.summary_irq)
    }

    fn claim_interrupt(&self) -> InterruptResult<InterruptClaim> {
        self.service_pending_irqs()
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
        let Ok(index) = usize::try_from(pin) else {
            return;
        };
        let mut handlers = self.irq_handlers.lock();
        let Some(slot) = handlers.get_mut(index) else {
            return;
        };
        let Some(handler) = slot.handler.as_mut() else {
            return;
        };
        handler.trigger = trigger;
        slot.generation = slot.generation.wrapping_add(1);
        slot.enabled = true;
        self.ack_irq(pin);
        self.configure_irq(pin, trigger, true);
    }

    fn disable_irq(&self, pin: u32) {
        let Ok(index) = usize::try_from(pin) else {
            return;
        };
        let mut handlers = self.irq_handlers.lock();
        let Some(slot) = handlers.get_mut(index) else {
            return;
        };
        slot.generation = slot.generation.wrapping_add(1);
        slot.enabled = false;
        self.update(
            pin,
            INTERRUPT_CONFIG,
            INTERRUPT_ENABLE | INTERRUPT_RAW_STATUS_ENABLE,
            0,
        );
    }

    fn ack_irq(&self, pin: u32) {
        let _ = self.write(pin, INTERRUPT_STATUS, 0);
    }

    fn request_irq(
        &self,
        pin: u32,
        trigger: GpioIrqTrigger,
        handler: Arc<dyn InterruptCapableDevice>,
    ) -> bool {
        let Ok(index) = usize::try_from(pin) else {
            return false;
        };
        let mut handlers = self.irq_handlers.lock();
        let Some(slot) = handlers.get_mut(index) else {
            return false;
        };
        if slot.handler.is_some() {
            return false;
        }

        self.configure_gpio(pin, false);
        self.update(
            pin,
            INTERRUPT_CONFIG,
            INTERRUPT_ENABLE | INTERRUPT_RAW_STATUS_ENABLE,
            0,
        );
        self.ack_irq(pin);
        slot.generation = slot.generation.wrapping_add(1);
        slot.enabled = true;
        slot.handler = Some(GpioIrqHandler {
            trigger,
            device: handler,
        });
        self.configure_irq(pin, trigger, true);
        true
    }

    fn free_irq(&self, pin: u32) {
        if let Ok(index) = usize::try_from(pin)
            && let Some(slot) = self.irq_handlers.lock().get_mut(index)
        {
            self.update(
                pin,
                INTERRUPT_CONFIG,
                INTERRUPT_ENABLE | INTERRUPT_RAW_STATUS_ENABLE,
                0,
            );
            self.ack_irq(pin);
            slot.generation = slot.generation.wrapping_add(1);
            slot.enabled = false;
            slot.handler = None;
        }
    }
}

impl PinctrlController for Sc7180Tlmm {
    fn apply_state(&self, state: &PinctrlState<'_>) -> Result<usize, PinctrlError> {
        if state.input_enable && state.output.is_some() {
            return Err(PinctrlError::Invalid);
        }

        let drive_strength = match state.drive_strength_ma {
            Some(milliamps) => {
                Some(Self::drive_strength_selector(milliamps).ok_or(PinctrlError::Invalid)?)
            }
            None => None,
        };

        // Resolve and validate the complete state before touching hardware so
        // an unsupported function never leaves a partially programmed group.
        let mut pins = Vec::with_capacity(state.pins.len());
        for pin_name in &state.pins {
            let pin = Self::named_pin(pin_name).ok_or(PinctrlError::Invalid)?;
            let function = match state.function {
                Some(function) => {
                    Some(Self::named_function(pin, function).ok_or(PinctrlError::Unsupported)?)
                }
                None => None,
            };
            pins.push((pin, function));
        }

        for (pin, function) in pins {
            if let Some(function) = function {
                self.set_function(pin, function);
            }
            if let Some(bias) = state.bias {
                self.set_pull(
                    pin,
                    match bias {
                        PinctrlBias::Disable => GpioPull::None,
                        PinctrlBias::PullDown => GpioPull::Down,
                        PinctrlBias::PullUp => GpioPull::Up,
                    },
                );
            }
            if let Some(selector) = drive_strength {
                self.configure_drive_strength(pin, selector);
            }
            if let Some(value) = state.output {
                self.configure_output(pin, value);
            } else if state.input_enable {
                self.configure_input(pin);
            }
        }

        Ok(state.pins.len())
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
    let irq_resource = device
        .get_resources()
        .iter()
        .find(|resource| matches!(resource.res_type, PlatformDeviceResourceType::IRQ))
        .ok_or("qcom-sc7180-tlmm: missing parent summary interrupt")?;
    let summary_irq = scarlet::interrupt::resolve_platform_irq(irq_resource)
        .map_err(|_| "qcom-sc7180-tlmm: failed to resolve parent summary interrupt")?;
    let controller = Arc::new(Sc7180Tlmm::new(tiles, summary_irq));
    // Take ownership of the TLMM summary line from firmware before enabling
    // it at the GIC. No unregistered stale status may keep SPI 208 asserted.
    for pin in 0..PIN_COUNT as u32 {
        controller.update(
            pin,
            INTERRUPT_CONFIG,
            INTERRUPT_ENABLE | INTERRUPT_RAW_STATUS_ENABLE,
            0,
        );
        controller.ack_irq(pin);
    }
    scarlet::interrupt::register_and_enable_platform_irq_device(
        irq_resource,
        controller.clone(),
        arch::get_cpu().get_cpuid() as u32,
    )
    .map_err(|_| "qcom-sc7180-tlmm: failed to register parent summary interrupt")?;
    let manager = DeviceManager::get_manager();
    manager.register_gpio_controller(phandle, controller.clone());
    manager.register_pinctrl_controller(phandle, controller);
    if let Err(error) = manager.apply_registered_pinctrl_default(device) {
        println!(
            "[qcom-sc7180-tlmm] failed to apply provider default state: {}",
            error
        );
    }
    println!(
        "[qcom-sc7180-tlmm] registered {} GPIOs (phandle={:#x}, summary IRQ={})",
        PIN_COUNT, phandle, summary_irq
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
