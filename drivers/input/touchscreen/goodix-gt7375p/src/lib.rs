// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Goodix GT7375P touchscreen support for Google CoachZ.
//!
//! The controller uses I2C-HID with the Goodix-specific power/reset sequencing
//! implemented by Linux's `i2c-hid-of-goodix` driver. CoachZ connects it to AP
//! I2C4 at address `0x5d`, with GPIO 8 as active-low reset and GPIO 9 as an
//! active-low interrupt.
//!
//! The controller's raw HID contact records are published as Linux type-B
//! multitouch slots. The primary contact is also mirrored to `ABS_X`, `ABS_Y`,
//! and `ABS_PRESSURE` for consumers which only understand direct-touch input.
//!
//! Runtime reports normally arrive through the SC7180 TLMM GPIO interrupt
//! demultiplexer. If GPIO IRQ registration is unavailable, the driver falls
//! back to polling the physical active-low IRQ line at a 17 ms interval.
//!
//! # Provenance
//!
//! Reset timing and I2C-HID framing are adapted from Linux
//! `drivers/hid/i2c-hid/{i2c-hid-of-goodix,i2c-hid-core}.c` (GPL-2.0-only).

extern crate alloc;

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

mod hid;

use hid::{
    HidField, HidTouchLayout, SlotTracker, TouchFrame, decode_contacts, decode_input_length,
    encode_simple_command, parse_touch_layout,
};

use scarlet::{
    device::{
        events::InterruptCapableDevice,
        gpio::{GpioController, GpioIrqTrigger},
        i2c::{I2cAddress, I2cBus, I2cError, I2cMessage},
        input::{
            abs_codes::{
                ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_PRESSURE, ABS_MT_SLOT,
                ABS_MT_TOUCH_MAJOR, ABS_MT_TRACKING_ID,
            },
            event_device::{
                EventDevice, INPUT_CAP_DIRECT_TOUCH, INPUT_CAP_KEY, InputDeviceKind,
                InputDeviceMetadata,
            },
            event_types::{EV_ABS, EV_KEY, EV_SYN},
            syn_codes::SYN_REPORT,
        },
        manager::{DeviceManager, DriverPriority, PROBE_DEFER},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
    interrupt::{InterruptId, InterruptResult},
    sync::IrqSpinLock,
    time,
};

const GOODIX_ADDRESS: u8 = 0x5d;
const I2C_HID_DESCRIPTOR_REGISTER: u16 = 0x0001;
const I2C_HID_DESCRIPTOR_LENGTH: usize = 30;
const GOODIX_HID_VENDOR_ID: u16 = 0x27c6;
const COACHZ_GT7375P_PRODUCT_IDS: [u16; 2] = [0x0e51, 0x0e94];
const I2C_HID_OPCODE_RESET: u8 = 0x01;
const I2C_HID_OPCODE_SET_POWER: u8 = 0x08;
const I2C_HID_POWER_ON_DELAY_US: u64 = 60_000;
const I2C_HID_RESET_TIMEOUT_US: u64 = 5_000_000;
const MAX_INPUT_LENGTH: usize = 512;
const MAX_REPORT_DESCRIPTOR_LENGTH: usize = 4096;
const POLL_INTERVAL_NS: u64 = 17_000_000;
const MAX_TOUCH_SLOTS: usize = 5;

// Linux input-event ABI values. Scarlet currently defines EV_ABS but does not
// yet export the associated direct-touch axis/button code constants.
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const BTN_TOUCH: u16 = 0x14a;

#[derive(Clone)]
struct GpioLine {
    controller: Arc<dyn GpioController>,
    pin: u32,
    active_low: bool,
}

impl GpioLine {
    fn set_asserted(&self, asserted: bool) {
        let physical_high = if self.active_low { !asserted } else { asserted };
        // CoachZ pinctrl hands reset over as an 8 mA output-low line. Updating
        // only its value preserves that drive-strength configuration.
        self.controller.set_value(self.pin, physical_high);
    }
}

struct GoodixGt7375p {
    bus: Arc<dyn I2cBus>,
    address: I2cAddress,
    irq: GpioLine,
    event: Arc<EventDevice>,
    max_input_length: usize,
    layout: HidTouchLayout,
    slots: IrqSpinLock<SlotTracker>,
    poll_fallback: AtomicBool,
    irq_work_pending: AtomicBool,
    active: AtomicBool,
    device_id: IrqSpinLock<Option<usize>>,
}

impl GoodixGt7375p {
    fn emit_touch_frame(&self, frame: TouchFrame) {
        for update in frame.updates {
            self.event
                .push_event(EV_ABS, ABS_MT_SLOT, update.slot as i32);
            if update.began {
                self.event
                    .push_event(EV_ABS, ABS_MT_TRACKING_ID, update.tracking_id);
            }
            self.event
                .push_event(EV_ABS, ABS_MT_POSITION_X, update.contact.x);
            self.event
                .push_event(EV_ABS, ABS_MT_POSITION_Y, update.contact.y);
            if let Some(pressure) = update.contact.pressure {
                self.event.push_event(EV_ABS, ABS_MT_PRESSURE, pressure);
            }
            if let Some(touch_major) = update.contact.touch_major {
                self.event
                    .push_event(EV_ABS, ABS_MT_TOUCH_MAJOR, touch_major);
            }
        }
        for slot in frame.releases {
            self.event.push_event(EV_ABS, ABS_MT_SLOT, slot as i32);
            self.event.push_event(EV_ABS, ABS_MT_TRACKING_ID, -1);
        }
        if let Some(primary) = frame.primary {
            self.event.push_event(EV_ABS, ABS_X, primary.x);
            self.event.push_event(EV_ABS, ABS_Y, primary.y);
            if let Some(pressure) = primary.pressure {
                self.event.push_event(EV_ABS, ABS_PRESSURE, pressure);
            }
        }
        self.event
            .push_event(EV_KEY, BTN_TOUCH, i32::from(frame.any_contact));
        self.event.push_event(EV_SYN, SYN_REPORT, 0);
    }

    fn process_report_inner(&self) -> Result<(), I2cError> {
        let mut message = I2cMessage::read(self.address, self.max_input_length, true);
        self.bus.transfer(core::slice::from_mut(&mut message))?;
        let declared_length = decode_input_length(&message.data).ok_or(I2cError::BusError)?;
        if declared_length == 0 {
            return Ok(());
        }
        if declared_length < 2 || declared_length > message.data.len() {
            return Err(I2cError::BusError);
        }
        let report = &message.data[2..declared_length];
        if self.layout.report_id != 0 && report.first().copied() != Some(self.layout.report_id) {
            return Ok(());
        }
        let contacts = decode_contacts(report, &self.layout).map_err(|_| I2cError::BusError)?;
        let frame = self.slots.lock().apply(&contacts);
        self.emit_touch_frame(frame);
        Ok(())
    }

    fn process_report(&self) -> Result<(), I2cError> {
        self.process_report_inner()
    }

    fn irq_asserted(&self) -> bool {
        let high = self.irq.controller.get_value(self.irq.pin);
        if self.irq.active_low { !high } else { high }
    }

    fn log_report_error(&self, prefix: &str, error: I2cError) {
        let count = REPORT_ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
            early_println!(
                "[goodix-gt7375p] {} report read failed: {:?} (count={})",
                prefix,
                error,
                count
            );
        }
    }

    fn process_worker_report(&self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.process_report() {
            self.log_report_error("worker", error);
        }

        if self.active.load(Ordering::Acquire) && !self.poll_fallback.load(Ordering::Acquire) {
            // Reading the complete I2C-HID input report deasserts the level
            // source. Clear the TLMM child latch next, then explicitly unmask
            // the LowLevel GPIO.
            self.irq.controller.ack_irq(self.irq.pin);
            self.irq
                .controller
                .enable_irq(self.irq.pin, GpioIrqTrigger::LowLevel);
        }
    }
}

impl InterruptCapableDevice for GoodixGt7375p {
    fn handle_interrupt(&self) -> InterruptResult<()> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        // I2C is sleeping/serialized work and must never run in the TLMM hard
        // IRQ callback. Mask and coalesce notifications for the worker.
        self.irq.controller.disable_irq(self.irq.pin);
        if !self.irq_work_pending.swap(true, Ordering::AcqRel) {
            WORKER_WAKER.wake_one();
        }
        Ok(())
    }

    fn interrupt_id(&self) -> Option<InterruptId> {
        // GPIO children are dispatched by the TLMM summary interrupt and do
        // not own a separate kernel-global VIRQ.
        None
    }
}

static DEVICE: IrqSpinLock<Option<Arc<GoodixGt7375p>>> = IrqSpinLock::new(None);
static WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static REPORT_ERRORS: AtomicU32 = AtomicU32::new(0);
static WORKER_WAKER: scarlet::sync::Waker =
    scarlet::sync::Waker::new_uninterruptible("goodix-touch-worker");

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn resolve_i2c_bus(device: &PlatformDeviceInfo) -> Result<Arc<dyn I2cBus>, &'static str> {
    let phandle = device
        .parent_phandle()
        .ok_or("goodix-gt7375p: missing parent I2C bus")?;
    DeviceManager::get_manager()
        .get_i2c_bus(phandle)
        .ok_or_else(|| {
            early_println!(
                "[goodix-gt7375p] I2C bus phandle {:#x} is not ready, deferring",
                phandle
            );
            PROBE_DEFER
        })
}

fn resolve_gpio(property: &[u8], label: &'static str) -> Result<GpioLine, &'static str> {
    let phandle = read_be_u32(property, 0).ok_or("goodix-gt7375p: malformed GPIO property")?;
    let pin = read_be_u32(property, 4).ok_or("goodix-gt7375p: malformed GPIO property")?;
    let flags = read_be_u32(property, 8).unwrap_or(0);
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(phandle)
        .ok_or_else(|| {
            early_println!(
                "[goodix-gt7375p] {} GPIO controller {:#x} is not ready, deferring",
                label,
                phandle
            );
            PROBE_DEFER
        })?;
    Ok(GpioLine {
        controller,
        pin,
        active_low: flags & 1 != 0,
    })
}

fn resolve_interrupt_gpio(device: &PlatformDeviceInfo) -> Result<GpioLine, &'static str> {
    let phandle_property = device
        .property("interrupt-parent")
        .ok_or("goodix-gt7375p: missing interrupt-parent")?;
    let interrupts = device
        .property("interrupts")
        .ok_or("goodix-gt7375p: missing interrupts")?;
    let phandle = read_be_u32(phandle_property.value(), 0)
        .ok_or("goodix-gt7375p: malformed interrupt-parent")?;
    let pin = read_be_u32(interrupts.value(), 0).ok_or("goodix-gt7375p: malformed interrupts")?;
    let trigger = read_be_u32(interrupts.value(), 4).unwrap_or(8);
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(phandle)
        .ok_or(PROBE_DEFER)?;
    if trigger != 8 {
        early_println!(
            "[goodix-gt7375p] warning: expected active-low level IRQ, DT flags={:#x}",
            trigger
        );
    }
    Ok(GpioLine {
        controller,
        pin,
        active_low: true,
    })
}

fn reset_controller(reset: &GpioLine, irq: &GpioLine) {
    let reset_high_before = reset.controller.get_value(reset.pin);
    let irq_high_before = irq.controller.get_value(irq.pin);
    let reset_asserted_high = !reset.active_low;
    // Linux explicitly takes ownership of the reset direction here. TLMM's
    // direction update preserves CoachZ's inherited 8 mA drive setting.
    reset
        .controller
        .set_direction_output(reset.pin, reset_asserted_high);
    time::udelay(20_000);
    reset.set_asserted(false);
    // GT7375P is I2C-HID, not a GT9xx register device. Linux's Goodix
    // I2C-HID power sequence waits 180 ms after releasing reset and does not
    // drive INT for legacy GT9xx address selection.
    time::udelay(180_000);
    irq.controller.set_direction_input(irq.pin);
    early_println!(
        "[goodix-gt7375p] I2C-HID reset: reset-high {}->{} irq-high {}->{} addr=0x{:02x}",
        reset_high_before,
        reset.controller.get_value(reset.pin),
        irq_high_before,
        irq.controller.get_value(irq.pin),
        GOODIX_ADDRESS,
    );
}

fn send_i2c_hid_command(
    bus: &Arc<dyn I2cBus>,
    command_register: u16,
    opcode: u8,
) -> Result<(), I2cError> {
    let command = encode_simple_command(command_register, opcode);
    bus.transfer(&mut [I2cMessage::write(
        I2cAddress::SevenBit(GOODIX_ADDRESS),
        &command,
        true,
    )])
}

fn wait_for_reset_completion(
    bus: &Arc<dyn I2cBus>,
    irq: &GpioLine,
    max_input_length: usize,
) -> Result<(), I2cError> {
    let start = time::current_time();
    while time::current_time().saturating_sub(start) < I2C_HID_RESET_TIMEOUT_US {
        let irq_high = irq.controller.get_value(irq.pin);
        let irq_asserted = if irq.active_low { !irq_high } else { irq_high };
        if !irq_asserted {
            time::udelay(1_000);
            continue;
        }

        let mut message =
            I2cMessage::read(I2cAddress::SevenBit(GOODIX_ADDRESS), max_input_length, true);
        bus.transfer(core::slice::from_mut(&mut message))?;
        let length = decode_input_length(&message.data).ok_or(I2cError::BusError)?;
        if length == 0 {
            return Ok(());
        }
    }
    Err(I2cError::Timeout)
}

fn initialize_i2c_hid(
    bus: &Arc<dyn I2cBus>,
    irq: &GpioLine,
    command_register: u16,
    max_input_length: usize,
) -> Result<(), I2cError> {
    send_i2c_hid_command(bus, command_register, I2C_HID_OPCODE_SET_POWER)?;
    time::udelay(I2C_HID_POWER_ON_DELAY_US);
    send_i2c_hid_command(bus, command_register, I2C_HID_OPCODE_RESET)?;
    wait_for_reset_completion(bus, irq, max_input_length)?;

    // Linux powers the device on again after a successful reset unless the
    // controller carries the NO_WAKEUP_AFTER_RESET quirk. GT7375P does not.
    send_i2c_hid_command(bus, command_register, I2C_HID_OPCODE_SET_POWER)?;
    time::udelay(I2C_HID_POWER_ON_DELAY_US);
    Ok(())
}

fn axis_range(fields: &[HidField]) -> Option<(i32, i32)> {
    let first = fields.first()?;
    Some(fields.iter().skip(1).fold(
        (first.logical_minimum, first.logical_maximum),
        |(minimum, maximum), field| {
            (
                minimum.min(field.logical_minimum),
                maximum.max(field.logical_maximum),
            )
        },
    ))
}

fn read_identity(bus: Arc<dyn I2cBus>, irq: GpioLine) -> Result<Arc<GoodixGt7375p>, &'static str> {
    let mut descriptor_messages = vec![
        I2cMessage::write(
            I2cAddress::SevenBit(GOODIX_ADDRESS),
            &I2C_HID_DESCRIPTOR_REGISTER.to_le_bytes(),
            false,
        ),
        I2cMessage::read(
            I2cAddress::SevenBit(GOODIX_ADDRESS),
            I2C_HID_DESCRIPTOR_LENGTH,
            true,
        ),
    ];
    bus.transfer(&mut descriptor_messages).map_err(|error| {
        early_println!(
            "[goodix-gt7375p] I2C-HID descriptor read failed: {:?}",
            error
        );
        "goodix-gt7375p: I2C-HID descriptor read failed"
    })?;
    let descriptor = &descriptor_messages[1].data;
    let word = |offset: usize| {
        descriptor
            .get(offset..offset + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
    };
    if word(0) != Some(I2C_HID_DESCRIPTOR_LENGTH as u16) || word(2) != Some(0x0100) {
        return Err("goodix-gt7375p: invalid I2C-HID descriptor");
    }
    let vendor_id = word(20).ok_or("goodix: missing HID vendor ID")?;
    let product_id = word(22).ok_or("goodix: missing HID product ID")?;
    if vendor_id != GOODIX_HID_VENDOR_ID || !COACHZ_GT7375P_PRODUCT_IDS.contains(&product_id) {
        early_println!(
            "[goodix-gt7375p] rejected I2C-HID identity vendor={:#06x} product={:#06x}",
            vendor_id,
            product_id,
        );
        return Err("goodix-gt7375p: unsupported I2C-HID identity");
    }
    let report_descriptor_length = usize::from(word(4).ok_or("goodix: missing report length")?);
    let report_descriptor_register = word(6).ok_or("goodix: missing report register")?;
    let max_input_length = usize::from(word(10).ok_or("goodix: missing input length")?);
    let command_register = word(16).ok_or("goodix: missing command register")?;
    if report_descriptor_length == 0
        || report_descriptor_length > MAX_REPORT_DESCRIPTOR_LENGTH
        || max_input_length < 3
        || max_input_length > MAX_INPUT_LENGTH
    {
        return Err("goodix-gt7375p: invalid I2C-HID buffer lengths");
    }
    initialize_i2c_hid(&bus, &irq, command_register, max_input_length).map_err(|error| {
        early_println!(
            "[goodix-gt7375p] I2C-HID power/reset initialization failed: {:?}",
            error,
        );
        "goodix-gt7375p: I2C-HID power/reset initialization failed"
    })?;
    let mut report_messages = vec![
        I2cMessage::write(
            I2cAddress::SevenBit(GOODIX_ADDRESS),
            &report_descriptor_register.to_le_bytes(),
            false,
        ),
        I2cMessage::read(
            I2cAddress::SevenBit(GOODIX_ADDRESS),
            report_descriptor_length,
            true,
        ),
    ];
    bus.transfer(&mut report_messages).map_err(|error| {
        early_println!(
            "[goodix-gt7375p] HID report descriptor read failed: {:?}",
            error
        );
        "goodix-gt7375p: HID report descriptor read failed"
    })?;
    let layout = parse_touch_layout(&report_messages[1].data)?;
    let (x_minimum, x_maximum) =
        axis_range(&layout.x).ok_or("goodix-gt7375p: missing HID X logical range")?;
    let (y_minimum, y_maximum) =
        axis_range(&layout.y).ok_or("goodix-gt7375p: missing HID Y logical range")?;
    let mut metadata = InputDeviceMetadata::new(
        InputDeviceKind::Touchscreen,
        INPUT_CAP_KEY | INPUT_CAP_DIRECT_TOUCH,
    )
    .with_multitouch_slots(MAX_TOUCH_SLOTS)?
    .with_absolute_axis(ABS_X, x_minimum, x_maximum)?
    .with_absolute_axis(ABS_Y, y_minimum, y_maximum)?
    .with_absolute_axis(ABS_MT_SLOT, 0, (MAX_TOUCH_SLOTS - 1) as i32)?
    .with_absolute_axis(ABS_MT_TRACKING_ID, -1, i32::MAX)?
    .with_absolute_axis(ABS_MT_POSITION_X, x_minimum, x_maximum)?
    .with_absolute_axis(ABS_MT_POSITION_Y, y_minimum, y_maximum)?;
    if let Some((pressure_minimum, pressure_maximum)) = axis_range(&layout.pressure) {
        metadata = metadata
            .with_absolute_axis(ABS_PRESSURE, pressure_minimum, pressure_maximum)?
            .with_absolute_axis(ABS_MT_PRESSURE, pressure_minimum, pressure_maximum)?;
    }
    if let Some((major_minimum, major_maximum)) = axis_range(&layout.touch_major) {
        metadata = metadata.with_absolute_axis(ABS_MT_TOUCH_MAJOR, major_minimum, major_maximum)?;
    }
    let event = Arc::new(EventDevice::new_with_metadata("touchscreen", metadata));
    let contacts = layout.contact_capacity().min(MAX_TOUCH_SLOTS);
    let device = GoodixGt7375p {
        bus,
        address: I2cAddress::SevenBit(GOODIX_ADDRESS),
        irq,
        event,
        max_input_length,
        layout,
        slots: IrqSpinLock::new(SlotTracker::new(MAX_TOUCH_SLOTS)),
        poll_fallback: AtomicBool::new(false),
        irq_work_pending: AtomicBool::new(false),
        active: AtomicBool::new(true),
        device_id: IrqSpinLock::new(None),
    };

    early_println!(
        "[goodix-gt7375p] I2C-HID vendor={:#06x} product={:#06x} version={:#06x} report={} input={} touch-report={} x-range={}..{} y-range={}..{} contacts={} ABI=type-b-multitouch",
        vendor_id,
        product_id,
        word(24).unwrap_or(0),
        report_descriptor_length,
        max_input_length,
        device.layout.report_id,
        x_minimum,
        x_maximum,
        y_minimum,
        y_maximum,
        contacts,
    );

    Ok(Arc::new(device))
}

fn ensure_worker_started() {
    if WORKER_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let task = scarlet::task::new_kernel_task("goodix-touch-worker".to_string(), 1, worker_entry);
    task.init();
    scarlet::sched::scheduler::add_task(task, scarlet::arch::get_cpu().get_cpuid());
}

fn worker_entry() {
    loop {
        let device = DEVICE.lock().as_ref().cloned();
        if let Some(device) = device {
            if device.poll_fallback.load(Ordering::Acquire) {
                if device.irq_asserted() {
                    device.process_worker_report();
                }
                if let Some(task) = scarlet::task::mytask() {
                    task.sleep(task.get_trapframe(), POLL_INTERVAL_NS);
                } else {
                    scarlet::arch::instruction::idle();
                }
                continue;
            }

            if device.irq_work_pending.swap(false, Ordering::AcqRel) {
                device.process_worker_report();
                continue;
            }
        }

        let Some(task) = scarlet::task::mytask() else {
            scarlet::arch::instruction::idle();
        };
        WORKER_WAKER.wait(task.get_id(), task.get_trapframe());
    }
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if DEVICE.lock().is_some() {
        return Err("goodix-gt7375p: controller is already registered");
    }

    let address = device
        .property("reg")
        .and_then(|property| property.as_usize())
        .ok_or("goodix-gt7375p: missing I2C address")?;
    if address != usize::from(GOODIX_ADDRESS) {
        return Err("goodix-gt7375p: unsupported I2C address");
    }

    let bus = resolve_i2c_bus(device)?;
    bus.set_bus_speed(400_000)
        .map_err(|_| "goodix-gt7375p: failed to set 400 kHz I2C speed")?;
    let reset_property = device
        .property("reset-gpios")
        .ok_or("goodix-gt7375p: missing reset-gpios")?;
    let reset = resolve_gpio(reset_property.value(), "reset")?;
    let irq = resolve_interrupt_gpio(device)?;

    // CoachZ's pp3300_ts regulator is firmware-enabled. Scarlet has no PMIC
    // regulator provider yet, so retain that rail and only perform the
    // controller-defined I2C-HID reset sequence here.
    reset_controller(&reset, &irq);
    let touch = read_identity(bus, irq)?;
    let event = touch.event.clone();
    let event_name: String = event.get_name().to_string();

    let device_id =
        DeviceManager::get_manager().register_device_with_name(event_name.clone(), event);
    *touch.device_id.lock() = Some(device_id);
    *DEVICE.lock() = Some(touch.clone());
    ensure_worker_started();

    touch.irq.controller.set_direction_input(touch.irq.pin);
    let interrupt_mode =
        if touch
            .irq
            .controller
            .request_irq(touch.irq.pin, GpioIrqTrigger::LowLevel, touch.clone())
        {
            touch
                .irq
                .controller
                .enable_irq(touch.irq.pin, GpioIrqTrigger::LowLevel);
            "GPIO9 LowLevel IRQ"
        } else {
            touch.poll_fallback.store(true, Ordering::Release);
            WORKER_WAKER.wake_one();
            "17 ms GPIO polling fallback"
        };

    early_println!(
        "[goodix-gt7375p] registered {} at 0x{:02x}; {}",
        event_name,
        GOODIX_ADDRESS,
        interrupt_mode,
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let Some(touch) = DEVICE.lock().take() else {
        return Ok(());
    };

    touch.active.store(false, Ordering::Release);
    if !touch.poll_fallback.load(Ordering::Acquire) {
        touch.irq.controller.free_irq(touch.irq.pin);
    }
    touch.irq_work_pending.store(false, Ordering::Release);
    if let Some(device_id) = touch.device_id.lock().take() {
        let _ = DeviceManager::get_manager().unregister_device(device_id);
    }
    WORKER_WAKER.wake_one();
    Ok(())
}

fn register_driver() {
    let driver = PlatformDeviceDriver::new(
        "goodix-gt7375p",
        probe_fn,
        remove_fn,
        vec!["goodix,gt7375p"],
    );
    DeviceManager::get_manager().register_driver(Box::new(driver), DriverPriority::Standard);
}

scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_GOODIX_GT7375P_ANCHOR: fn() = force_link;

#[inline(never)]
pub fn force_link() {}
