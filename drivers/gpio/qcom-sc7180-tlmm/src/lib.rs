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

// The SDC1 pad controls are not GPIO groups.  U-Boot's SC7180 table places
// their shared register at 0x7a000 relative to the west TLMM tile (the first
// `reg` entry in the DT's west/north/south resource order).
const SDC1_CONFIG: usize = 0x7a000;
const SDC_PULL_MASK: u32 = 0x3;
const SDC_DRIVE_STRENGTH_MASK: u32 = 0x7;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PinctrlPin {
    Gpio(u32),
    Sdc1(Sdc1Pin),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Sdc1Pin {
    Rclk,
    Clk,
    Cmd,
    Data,
}

impl Sdc1Pin {
    fn pull_shift(self) -> u32 {
        match self {
            Self::Rclk => 15,
            Self::Clk => 13,
            Self::Cmd => 11,
            Self::Data => 9,
        }
    }

    fn drive_strength_shift(self) -> u32 {
        match self {
            Self::Rclk | Self::Data => 0,
            Self::Clk => 6,
            Self::Cmd => 3,
        }
    }
}

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
    sdc1_update_lock: IrqSpinLock<()>,
    irq_handlers: IrqSpinLock<Vec<GpioIrqSlot>>,
}

impl Sc7180Tlmm {
    fn new(tiles: [usize; 3], summary_irq: InterruptId) -> Self {
        Self {
            tiles,
            summary_irq,
            sdc1_update_lock: IrqSpinLock::new(()),
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
            let _ = self.write(pin, offset, Self::updated_register_value(value, clear, set));
        }
    }

    fn updated_register_value(value: u32, clear: u32, set: u32) -> u32 {
        (value & !clear) | set
    }

    fn sdc1_config_register(tiles: &[usize; 3]) -> Option<usize> {
        tiles[West as usize].checked_add(SDC1_CONFIG)
    }

    fn update_sdc1(&self, clear: u32, set: u32) {
        let Some(register) = Self::sdc1_config_register(&self.tiles) else {
            return;
        };
        // All SDC1 pads share this register. Keep the complete read-modify-
        // write sequence atomic with respect to sibling pad configuration.
        let _guard = self.sdc1_update_lock.lock();
        // SAFETY: the west tile is the first mapped SC7180 TLMM resource and
        // SDC1_CONFIG lies within its DT-declared 0x300000-byte window.
        let value = unsafe { mmio::read32(register) };
        // SAFETY: as above; this only modifies SDC1 pull or drive fields.
        unsafe { mmio::write32(register, Self::updated_register_value(value, clear, set)) };
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
            // Linux's SC7180 PINGROUP table places QUP wrap0 SE4 I2C in mux
            // slot 1 for GPIO115 (SDA) and GPIO116 (SCL).
            "qup04_i2c" if matches!(pin, 115 | 116) => Some(1),
            // Linux's SC7180 PINGROUP table places qup11_i2c in mux slot 1
            // for GPIO6 (SDA) and GPIO7 (SCL).
            "qup11_i2c" if matches!(pin, 6 | 7) => Some(1),
            _ => None,
        }
    }

    fn named_pin(pin: &str) -> Option<PinctrlPin> {
        let sdc1 = match pin {
            "sdc1_rclk" => Some(Sdc1Pin::Rclk),
            "sdc1_clk" => Some(Sdc1Pin::Clk),
            "sdc1_cmd" => Some(Sdc1Pin::Cmd),
            "sdc1_data" => Some(Sdc1Pin::Data),
            _ => None,
        };
        if let Some(pin) = sdc1 {
            return Some(PinctrlPin::Sdc1(pin));
        }

        let pin: u32 = pin.strip_prefix("gpio")?.parse().ok()?;
        (pin < PIN_COUNT as u32).then_some(PinctrlPin::Gpio(pin))
    }

    fn drive_strength_selector(milliamps: u32) -> Option<u32> {
        ((2..=16).contains(&milliamps) && milliamps.is_multiple_of(2)).then_some(milliamps / 2 - 1)
    }

    fn configure_drive_strength(&self, pin: u32, selector: u32) {
        self.update(pin, CONFIG, CONFIG_DRIVE_STRENGTH_MASK, selector << 6);
    }

    fn configure_sdc1_pull(&self, pin: Sdc1Pin, pull: GpioPull) {
        let value = match pull {
            GpioPull::None => 0,
            GpioPull::Down => 1,
            GpioPull::Up => 3,
        };
        let shift = pin.pull_shift();
        self.update_sdc1(SDC_PULL_MASK << shift, value << shift);
    }

    fn configure_sdc1_drive_strength(&self, pin: Sdc1Pin, selector: u32) {
        let shift = pin.drive_strength_shift();
        self.update_sdc1(SDC_DRIVE_STRENGTH_MASK << shift, selector << shift);
    }

    fn validate_pinctrl_state(state: &PinctrlState<'_>) -> Result<(), PinctrlError> {
        if state.input_enable && state.output.is_some() {
            return Err(PinctrlError::Invalid);
        }

        if let Some(milliamps) = state.drive_strength_ma {
            Self::drive_strength_selector(milliamps).ok_or(PinctrlError::Invalid)?;
        }

        for pin_name in &state.pins {
            let pin = Self::named_pin(pin_name).ok_or(PinctrlError::Invalid)?;
            match (pin, state.function) {
                (PinctrlPin::Gpio(pin), Some(function)) => {
                    Self::named_function(pin, function).ok_or(PinctrlError::Unsupported)?;
                }
                // SDC1 pads have no GPIO mux or direction control. They only
                // support the pull and drive-strength pinconf fields below.
                (PinctrlPin::Sdc1(_), Some(_)) => return Err(PinctrlError::Unsupported),
                (_, None) => {}
            }
            if matches!(pin, PinctrlPin::Sdc1(_)) && (state.output.is_some() || state.input_enable)
            {
                return Err(PinctrlError::Unsupported);
            }
        }

        Ok(())
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
    fn validate_state(&self, state: &PinctrlState<'_>) -> Result<(), PinctrlError> {
        Self::validate_pinctrl_state(state)
    }

    fn apply_state(&self, state: &PinctrlState<'_>) -> Result<usize, PinctrlError> {
        self.validate_state(state)?;
        let drive_strength = match state.drive_strength_ma {
            Some(milliamps) => {
                Some(Self::drive_strength_selector(milliamps).ok_or(PinctrlError::Invalid)?)
            }
            None => None,
        };

        // Resolve the complete state before touching hardware so an
        // unsupported function never leaves a partially programmed group.
        let mut pins = Vec::with_capacity(state.pins.len());
        for pin_name in &state.pins {
            let pin = Self::named_pin(pin_name).ok_or(PinctrlError::Invalid)?;
            let function = match (pin, state.function) {
                (PinctrlPin::Gpio(pin), Some(function)) => {
                    Some(Self::named_function(pin, function).ok_or(PinctrlError::Unsupported)?)
                }
                // SDC1 pads have no GPIO mux or direction control. They only
                // support the pull and drive-strength pinconf fields below.
                (PinctrlPin::Sdc1(_), Some(_)) => return Err(PinctrlError::Unsupported),
                (_, None) => None,
            };
            pins.push((pin, function));
        }

        for (pin, function) in pins {
            match pin {
                PinctrlPin::Gpio(pin) => {
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
                PinctrlPin::Sdc1(pin) => {
                    if let Some(bias) = state.bias {
                        self.configure_sdc1_pull(
                            pin,
                            match bias {
                                PinctrlBias::Disable => GpioPull::None,
                                PinctrlBias::PullDown => GpioPull::Down,
                                PinctrlBias::PullUp => GpioPull::Up,
                            },
                        );
                    }
                    if let Some(selector) = drive_strength {
                        self.configure_sdc1_drive_strength(pin, selector);
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coachz_touch_i2c_pins_use_qup04_mux_slot_one() {
        assert_eq!(Sc7180Tlmm::named_function(115, "qup04_i2c"), Some(1));
        assert_eq!(Sc7180Tlmm::named_function(116, "qup04_i2c"), Some(1));
        assert_eq!(Sc7180Tlmm::named_function(114, "qup04_i2c"), None);
        assert_eq!(Sc7180Tlmm::named_function(115, "qup04_uart"), None);
    }

    #[test]
    fn sdc1_pins_use_the_west_control_register_and_documented_fields() {
        assert_eq!(
            Sc7180Tlmm::sdc1_config_register(&[0x1000, 0x2000, 0x3000]),
            Some(0x7b000)
        );

        let fields = [
            ("sdc1_rclk", Sdc1Pin::Rclk, 15, 0),
            ("sdc1_clk", Sdc1Pin::Clk, 13, 6),
            ("sdc1_cmd", Sdc1Pin::Cmd, 11, 3),
            ("sdc1_data", Sdc1Pin::Data, 9, 0),
        ];
        for (name, expected_pin, pull_shift, drive_shift) in fields {
            assert_eq!(
                Sc7180Tlmm::named_pin(name),
                Some(PinctrlPin::Sdc1(expected_pin))
            );
            assert_eq!(expected_pin.pull_shift(), pull_shift);
            assert_eq!(expected_pin.drive_strength_shift(), drive_shift);
        }
    }

    #[test]
    fn sdc1_names_do_not_alias_gpio_pins() {
        assert_eq!(
            Sc7180Tlmm::named_pin("gpio118"),
            Some(PinctrlPin::Gpio(118))
        );
        assert_eq!(Sc7180Tlmm::named_pin("gpio119"), None);
        assert_eq!(Sc7180Tlmm::named_pin("sdc2_clk"), None);
    }

    #[test]
    fn sdc1_field_update_preserves_sibling_pad_configuration() {
        let rclk_pull_mask = SDC_PULL_MASK << Sdc1Pin::Rclk.pull_shift();
        let cmd_drive_mask = SDC_DRIVE_STRENGTH_MASK << Sdc1Pin::Cmd.drive_strength_shift();
        let data_pull_mask = SDC_PULL_MASK << Sdc1Pin::Data.pull_shift();
        let original = rclk_pull_mask
            | (0b101 << Sdc1Pin::Cmd.drive_strength_shift())
            | (0b01 << Sdc1Pin::Data.pull_shift());
        let updated = Sc7180Tlmm::updated_register_value(
            original,
            data_pull_mask,
            0b11 << Sdc1Pin::Data.pull_shift(),
        );

        assert_eq!(updated & rclk_pull_mask, original & rclk_pull_mask);
        assert_eq!(updated & cmd_drive_mask, original & cmd_drive_mask);
        assert_eq!(updated & data_pull_mask, 0b11 << Sdc1Pin::Data.pull_shift());
    }

    #[test]
    fn sdc1_on_children_validate_without_a_mux_function() {
        for (pin, bias, drive_strength_ma) in [
            ("sdc1_clk", PinctrlBias::Disable, Some(16)),
            ("sdc1_cmd", PinctrlBias::PullUp, Some(16)),
            ("sdc1_data", PinctrlBias::PullUp, Some(16)),
            ("sdc1_rclk", PinctrlBias::PullDown, None),
        ] {
            let state = PinctrlState {
                pins: alloc::vec![pin],
                function: None,
                bias: Some(bias),
                drive_strength_ma,
                output: None,
                input_enable: false,
            };
            assert_eq!(Sc7180Tlmm::validate_pinctrl_state(&state), Ok(()));
        }
    }

    #[test]
    fn sdc1_rejects_gpio_only_configuration() {
        let state = PinctrlState {
            pins: alloc::vec!["sdc1_clk"],
            function: Some("gpio"),
            bias: None,
            drive_strength_ma: None,
            output: None,
            input_enable: false,
        };
        assert_eq!(
            Sc7180Tlmm::validate_pinctrl_state(&state),
            Err(PinctrlError::Unsupported)
        );

        for (output, input_enable) in [(Some(false), false), (None, true)] {
            let state = PinctrlState {
                pins: alloc::vec!["sdc1_clk"],
                function: None,
                bias: None,
                drive_strength_ma: None,
                output,
                input_enable,
            };
            assert_eq!(
                Sc7180Tlmm::validate_pinctrl_state(&state),
                Err(PinctrlError::Unsupported)
            );
        }
    }
}
