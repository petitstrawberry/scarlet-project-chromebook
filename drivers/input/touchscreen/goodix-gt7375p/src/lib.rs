// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Goodix GT7375P touchscreen support for Google CoachZ.
//!
//! The controller uses the GT9xx register protocol exposed by the Linux Goodix
//! driver. CoachZ connects it to AP I2C4 at address `0x5d`, with GPIO 8 as an
//! active-low reset and GPIO 9 as an active-low interrupt.
//!
//! Scarlet does not yet expose multitouch slot metadata or axis capability
//! descriptors. This driver therefore publishes one honest direct-touch
//! stream on `tabletN`: the first reported contact becomes `ABS_X`, `ABS_Y`,
//! `ABS_PRESSURE`, and `BTN_TOUCH`. Additional contacts are parsed and cleared
//! at the controller, but are not advertised as supported.
//!
//! Runtime reports normally arrive through the SC7180 TLMM GPIO interrupt
//! demultiplexer. If GPIO IRQ registration is unavailable, the driver falls
//! back to polling the physical active-low IRQ line at the same 17 ms interval
//! used by Linux's Goodix polling mode.
//!
//! # Provenance
//!
//! Register addresses, reset timing, report framing, and contact decoding are
//! adapted from Linux `drivers/input/touchscreen/goodix.{c,h}` (GPL-2.0-only).

extern crate alloc;

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use scarlet::{
    device::{
        events::InterruptCapableDevice,
        gpio::{GpioController, GpioIrqTrigger},
        i2c::{I2cAddress, I2cBus, I2cError, I2cMessage},
        input::{
            event_device::EventDevice,
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
const GOODIX_REG_CONFIG: u16 = 0x8047;
const GOODIX_REG_ID: u16 = 0x8140;
const GOODIX_REG_COORDINATES: u16 = 0x814e;

const CONFIG_LENGTH: usize = 240;
const CONFIG_RESOLUTION_OFFSET: usize = 1;
const CONFIG_MAX_CONTACTS_OFFSET: usize = 5;
const MAX_CONTACTS: usize = 10;
const CONTACT_SIZE: usize = 8;
const READY: u8 = 1 << 7;
const POLL_INTERVAL_NS: u64 = 17_000_000;
const REPORT_READY_RETRIES: usize = 20;

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
    max_contacts: usize,
    resolution: (u16, u16),
    touching: IrqSpinLock<bool>,
    poll_fallback: AtomicBool,
    irq_work_pending: AtomicBool,
    active: AtomicBool,
    device_id: IrqSpinLock<Option<usize>>,
}

impl GoodixGt7375p {
    fn read_register(&self, register: u16, destination: &mut [u8]) -> Result<(), I2cError> {
        let address = register.to_be_bytes();
        let mut messages = vec![
            I2cMessage::write(self.address, &address, false),
            I2cMessage::read(self.address, destination.len(), true),
        ];
        self.bus.transfer(&mut messages)?;
        destination.copy_from_slice(&messages[1].data);
        Ok(())
    }

    fn write_register_u8(&self, register: u16, value: u8) -> Result<(), I2cError> {
        let [high, low] = register.to_be_bytes();
        self.bus
            .transfer(&mut [I2cMessage::write(self.address, &[high, low, value], true)])
    }

    fn read_ready_header(&self, header: &mut [u8; 1 + CONTACT_SIZE + 1]) -> Result<bool, I2cError> {
        for _ in 0..REPORT_READY_RETRIES {
            self.read_register(GOODIX_REG_COORDINATES, header)?;
            if header[0] & READY != 0 {
                return Ok(true);
            }
            time::udelay(1_000);
        }
        Ok(false)
    }

    fn emit_release(&self) {
        let mut touching = self.touching.lock();
        if *touching {
            self.event.push_event(EV_KEY, BTN_TOUCH, 0);
            self.event.push_event(EV_SYN, SYN_REPORT, 0);
            *touching = false;
        }
    }

    fn process_report_inner(&self) -> Result<(), I2cError> {
        let mut header = [0u8; 1 + CONTACT_SIZE + 1];
        if !self.read_ready_header(&mut header)? {
            return Ok(());
        }

        let count = usize::from(header[0] & 0x0f);
        if count > self.max_contacts || count > MAX_CONTACTS {
            return Err(I2cError::InvalidArg);
        }

        // Consume the complete report before acknowledging it, even though
        // Scarlet currently exposes only the first contact to userspace.
        if count > 1 {
            let mut remaining = [0u8; CONTACT_SIZE * (MAX_CONTACTS - 1)];
            let bytes = CONTACT_SIZE * (count - 1);
            self.read_register(
                GOODIX_REG_COORDINATES + (header.len() as u16),
                &mut remaining[..bytes],
            )?;
        }

        if count == 0 {
            self.emit_release();
        } else {
            let contact = &header[1..1 + CONTACT_SIZE];
            let x = i32::from(u16::from_le_bytes([contact[1], contact[2]]));
            let y = i32::from(u16::from_le_bytes([contact[3], contact[4]]));
            let pressure = i32::from(u16::from_le_bytes([contact[5], contact[6]]));

            self.event.push_event(EV_ABS, ABS_X, x);
            self.event.push_event(EV_ABS, ABS_Y, y);
            self.event.push_event(EV_ABS, ABS_PRESSURE, pressure);
            self.event.push_event(EV_KEY, BTN_TOUCH, 1);
            self.event.push_event(EV_SYN, SYN_REPORT, 0);
            *self.touching.lock() = true;
        }

        Ok(())
    }

    fn process_report(&self) -> Result<(), I2cError> {
        let result = self.process_report_inner();
        // Linux acknowledges every IRQ/poll attempt, including a spurious
        // finger-up notification whose READY bit never becomes set.
        let acknowledge = self.write_register_u8(GOODIX_REG_COORDINATES, 0);
        result.and(acknowledge)
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
            // process_report() first clears the Goodix coordinate buffer,
            // deasserting the level source. Clear the TLMM child latch next,
            // then explicitly unmask the LowLevel GPIO.
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
    // Goodix address selection: holding INT low during reset selects 7-bit
    // address 0x5d (wire addresses 0xba/0xbb).
    let reset_high_before = reset.controller.get_value(reset.pin);
    let irq_high_before = irq.controller.get_value(irq.pin);
    let reset_asserted_high = !reset.active_low;
    // Linux explicitly takes ownership of the reset direction here. TLMM's
    // direction update preserves CoachZ's inherited 8 mA drive setting.
    reset
        .controller
        .set_direction_output(reset.pin, reset_asserted_high);
    time::udelay(20_000);
    irq.controller.set_direction_output(irq.pin, false);
    time::udelay(200);
    reset.set_asserted(false);
    time::udelay(6_000);

    // Synchronize and return INT to input mode (Linux T5 = 50 ms).
    irq.controller.set_direction_output(irq.pin, false);
    time::udelay(50_000);
    irq.controller.set_direction_input(irq.pin);
    early_println!(
        "[goodix-gt7375p] reset/address select: reset-high {}->{} irq-high {}->{} addr=0x{:02x}",
        reset_high_before,
        reset.controller.get_value(reset.pin),
        irq_high_before,
        irq.controller.get_value(irq.pin),
        GOODIX_ADDRESS,
    );
}

fn read_identity(bus: Arc<dyn I2cBus>, irq: GpioLine) -> Result<Arc<GoodixGt7375p>, &'static str> {
    let event = Arc::new(EventDevice::new("tablet"));
    let mut device = GoodixGt7375p {
        bus,
        address: I2cAddress::SevenBit(GOODIX_ADDRESS),
        irq,
        event,
        max_contacts: MAX_CONTACTS,
        resolution: (4096, 4096),
        touching: IrqSpinLock::new(false),
        poll_fallback: AtomicBool::new(false),
        irq_work_pending: AtomicBool::new(false),
        active: AtomicBool::new(true),
        device_id: IrqSpinLock::new(None),
    };

    // Match Linux goodix_i2c_test(): retry the ID register once after 20 ms.
    // Preserve the concrete GENI error in the log instead of collapsing a
    // NACK, bus error, and timeout into the same probe message.
    let mut test = [0u8; 1];
    let mut last_error = None;
    for attempt in 1..=2 {
        match device.read_register(GOODIX_REG_ID, &mut test) {
            Ok(()) => {
                last_error = None;
                break;
            }
            Err(error) => {
                last_error = Some(error);
                early_println!(
                    "[goodix-gt7375p] ID probe attempt {} at 0x{:02x} failed: {:?}",
                    attempt,
                    GOODIX_ADDRESS,
                    error,
                );
                time::udelay(20_000);
            }
        }
    }
    if last_error.is_some() {
        return Err("goodix-gt7375p: device-ID read failed");
    }

    let mut identity = [0u8; 6];
    device
        .read_register(GOODIX_REG_ID, &mut identity)
        .map_err(|error| {
            early_println!(
                "[goodix-gt7375p] full ID read at 0x{:02x} failed after successful bus test: {:?}",
                GOODIX_ADDRESS,
                error,
            );
            "goodix-gt7375p: full device-ID read failed"
        })?;
    if identity[..4].iter().all(|byte| *byte == 0 || *byte == 0xff) {
        return Err("goodix-gt7375p: invalid device ID");
    }

    let mut config = [0u8; CONFIG_LENGTH];
    if device.read_register(GOODIX_REG_CONFIG, &mut config).is_ok() {
        let width = u16::from_le_bytes([
            config[CONFIG_RESOLUTION_OFFSET],
            config[CONFIG_RESOLUTION_OFFSET + 1],
        ]);
        let height = u16::from_le_bytes([
            config[CONFIG_RESOLUTION_OFFSET + 2],
            config[CONFIG_RESOLUTION_OFFSET + 3],
        ]);
        let contacts = usize::from(config[CONFIG_MAX_CONTACTS_OFFSET] & 0x0f);
        if width != 0 && height != 0 {
            device.resolution = (width, height);
        }
        if contacts != 0 {
            device.max_contacts = contacts.min(MAX_CONTACTS);
        }
    }

    early_println!(
        "[goodix-gt7375p] ID={}{}{}{} version={:#06x} resolution={}x{} contacts={} ABI=single-contact-absolute",
        identity[0] as char,
        identity[1] as char,
        identity[2] as char,
        identity[3] as char,
        u16::from_le_bytes([identity[4], identity[5]]),
        device.resolution.0,
        device.resolution.1,
        device.max_contacts,
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
    // controller-defined reset/address-selection sequence here.
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
