// SPDX-License-Identifier: GPL-2.0-only

#![no_std]

//! Elan eKTH3000 I2C clickpad support for Google CoachZ.
//!
//! The initial Scarlet interface exposes the first active contact as a
//! relative pointer and the clickpad switch as `BTN_LEFT`. The hardware is
//! still consumed in its native absolute-report mode so later multitouch
//! support can reuse the same transport without changing device setup.
//!
//! # Provenance
//!
//! The command values and report layout follow Linux
//! `drivers/input/mouse/elan_i2c_{core,i2c}.c` and ChromiumOS EC
//! `driver/touchpad_elan.c`. The event conversion follows Scarlet's Apple
//! SPI-HID pointer path.

extern crate alloc;

use alloc::{boxed::Box, string::ToString, sync::Arc, vec};
use core::convert::TryInto;
use core::sync::atomic::{AtomicBool, Ordering};

use scarlet::{
    device::{
        events::InterruptCapableDevice,
        gpio::{GpioController, GpioIrqTrigger},
        i2c::{I2cAddress, I2cBus, I2cMessage},
        input::{
            event_device::EventDevice,
            event_types::{EV_KEY, EV_REL, EV_SYN},
            key_codes::BTN_LEFT,
            rel_codes::{REL_X, REL_Y},
            syn_codes::SYN_REPORT,
        },
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
    },
    early_println,
    interrupt::{InterruptId, InterruptResult},
    sync::IrqSpinLock,
    time,
};

const MAX_7BIT_ADDRESS: usize = 0x7f;
const ETP_ADDRESS: u8 = 0x15;

const ETP_STAND_CMD: u16 = 0x0005;
const ETP_PATTERN_CMD: u16 = 0x0100;
const ETP_MAX_X_CMD: u16 = 0x0106;
const ETP_MAX_Y_CMD: u16 = 0x0107;
const ETP_SET_CMD: u16 = 0x0300;

const ETP_RESET: u16 = 0x0100;
const ETP_WAKE_UP: u16 = 0x0800;
const ETP_ENABLE_ABS: u16 = 0x0001;

const ETP_REPORT_LEN: usize = 34;
const ETP_REPORT_LEN_HIGH_PRECISION: usize = 39;
const ETP_REPORT_ID_OFFSET: usize = 2;
const ETP_TOUCH_INFO_OFFSET: usize = 3;
const ETP_FINGER_DATA_OFFSET: usize = 4;
const ETP_FINGER_DATA_LEN: usize = 5;
const ETP_REPORT_ID: u8 = 0x5d;
const ETP_REPORT_ID_HIGH_PRECISION: u8 = 0x60;
const ETP_MAX_FINGERS: usize = 5;

const POINTER_SCALE: i32 = 12;
const RESET_DELAY_US: u64 = 100_000;

struct ElanEkth3000 {
    bus: Arc<dyn I2cBus>,
    address: I2cAddress,
    irq_gpio: Arc<dyn GpioController>,
    irq_pin: u32,
    event_device: Arc<EventDevice>,
    report_len: usize,
    max_y: u16,
    last_contact: IrqSpinLock<Option<(u16, u16)>>,
    button_pressed: IrqSpinLock<bool>,
    work_pending: AtomicBool,
}

impl ElanEkth3000 {
    fn read_raw<const N: usize>(&self) -> Result<[u8; N], &'static str> {
        let mut messages = [I2cMessage::read(self.address, N, true)];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "elan-ekth3000: I2C read failed")?;
        messages[0]
            .data
            .as_slice()
            .try_into()
            .map_err(|_| "elan-ekth3000: short I2C read")
    }

    fn read_command(&self, register: u16) -> Result<u16, &'static str> {
        let command = register.to_le_bytes();
        let mut messages = [
            I2cMessage::write(self.address, &command, false),
            I2cMessage::read(self.address, 2, true),
        ];
        self.bus
            .transfer(&mut messages)
            .map_err(|_| "elan-ekth3000: command read failed")?;
        let bytes: [u8; 2] = messages[1]
            .data
            .as_slice()
            .try_into()
            .map_err(|_| "elan-ekth3000: short command response")?;
        Ok(u16::from_le_bytes(bytes))
    }

    fn write_command(&self, register: u16, value: u16) -> Result<(), &'static str> {
        let mut frame = [0u8; 4];
        frame[..2].copy_from_slice(&register.to_le_bytes());
        frame[2..].copy_from_slice(&value.to_le_bytes());
        self.bus
            .transfer(&mut [I2cMessage::write(self.address, &frame, true)])
            .map_err(|_| "elan-ekth3000: command write failed")
    }

    fn initialize(&self) -> Result<(), &'static str> {
        self.write_command(ETP_STAND_CMD, ETP_RESET)?;
        time::udelay(RESET_DELAY_US);

        let reset_ack = self.read_raw::<2>()?;
        if reset_ack != [0, 0] {
            early_println!(
                "[elan-ekth3000] unexpected reset acknowledgement {:02x} {:02x}",
                reset_ack[0],
                reset_ack[1]
            );
        }

        self.write_command(ETP_SET_CMD, ETP_ENABLE_ABS)?;
        self.write_command(ETP_STAND_CMD, ETP_WAKE_UP)?;
        Ok(())
    }

    fn contact_from_report(&self, report: &[u8]) -> Option<(u16, u16)> {
        let touch_info = *report.get(ETP_TOUCH_INFO_OFFSET)?;
        let first_active =
            (0..ETP_MAX_FINGERS).find(|index| touch_info & (1 << (3 + index)) != 0)?;
        let packed_before = (0..first_active)
            .filter(|index| touch_info & (1 << (3 + index)) != 0)
            .count();
        let offset = ETP_FINGER_DATA_OFFSET + packed_before * ETP_FINGER_DATA_LEN;
        let finger = report.get(offset..offset + ETP_FINGER_DATA_LEN)?;

        let (x, raw_y) = if report[ETP_REPORT_ID_OFFSET] == ETP_REPORT_ID_HIGH_PRECISION {
            (
                u16::from_be_bytes([finger[0], finger[1]]),
                u16::from_be_bytes([finger[2], finger[3]]),
            )
        } else {
            (
                (u16::from(finger[0] & 0xf0) << 4) | u16::from(finger[1]),
                (u16::from(finger[0] & 0x0f) << 8) | u16::from(finger[2]),
            )
        };
        Some((x, self.max_y.saturating_sub(raw_y)))
    }

    fn process_report(&self, report: &[u8]) {
        let Some(report_id) = report.get(ETP_REPORT_ID_OFFSET).copied() else {
            return;
        };
        if report_id != ETP_REPORT_ID && report_id != ETP_REPORT_ID_HIGH_PRECISION {
            early_println!("[elan-ekth3000] ignoring report id {:#x}", report_id);
            return;
        }

        let touch_info = report[ETP_TOUCH_INFO_OFFSET];
        let pressed = touch_info & 1 != 0;
        let mut previous_button = self.button_pressed.lock();
        if *previous_button != pressed {
            self.event_device
                .push_event(EV_KEY, BTN_LEFT, i32::from(pressed));
            *previous_button = pressed;
        }
        drop(previous_button);

        let contact = self.contact_from_report(report);
        let mut previous_contact = self.last_contact.lock();
        if let (Some((x, y)), Some((last_x, last_y))) = (contact, *previous_contact) {
            let dx = (i32::from(x) - i32::from(last_x)) / POINTER_SCALE;
            let dy = (i32::from(y) - i32::from(last_y)) / POINTER_SCALE;
            if dx != 0 {
                self.event_device.push_event(EV_REL, REL_X, dx);
            }
            if dy != 0 {
                self.event_device.push_event(EV_REL, REL_Y, dy);
            }
        }
        *previous_contact = contact;
        drop(previous_contact);

        self.event_device.push_event(EV_SYN, SYN_REPORT, 0);
    }

    fn read_and_process_report(&self) -> Result<(), &'static str> {
        match self.report_len {
            ETP_REPORT_LEN => self.process_report(&self.read_raw::<ETP_REPORT_LEN>()?),
            ETP_REPORT_LEN_HIGH_PRECISION => {
                self.process_report(&self.read_raw::<ETP_REPORT_LEN_HIGH_PRECISION>()?)
            }
            _ => return Err("elan-ekth3000: unsupported report length"),
        }
        Ok(())
    }

    fn process_deferred_interrupt_work(&self) -> bool {
        if !self.work_pending.swap(false, Ordering::AcqRel) {
            return false;
        }
        if let Err(error) = self.read_and_process_report() {
            early_println!("[elan-ekth3000] deferred report failed: {}", error);
        }
        true
    }
}

impl InterruptCapableDevice for ElanEkth3000 {
    fn handle_interrupt(&self) -> InterruptResult<()> {
        self.work_pending.store(true, Ordering::Release);
        ELAN_IRQ_WORKER_WAKER.wake_one();
        Ok(())
    }

    fn interrupt_id(&self) -> Option<InterruptId> {
        None
    }
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_address(device: &PlatformDeviceInfo) -> Result<I2cAddress, &'static str> {
    let address = device
        .property("reg")
        .and_then(|property| property.as_usize())
        .ok_or("elan-ekth3000: missing I2C address")?;
    if address > MAX_7BIT_ADDRESS || address != usize::from(ETP_ADDRESS) {
        return Err("elan-ekth3000: unexpected I2C address");
    }
    Ok(I2cAddress::SevenBit(address as u8))
}

fn resolve_bus(device: &PlatformDeviceInfo) -> Result<Arc<dyn I2cBus>, &'static str> {
    let phandle = device
        .parent_phandle()
        .ok_or("elan-ekth3000: missing parent I2C bus")?;
    DeviceManager::get_manager()
        .get_i2c_bus(phandle)
        .ok_or_else(|| {
            early_println!(
                "[elan-ekth3000] I2C bus phandle {:#x} is not ready, deferring",
                phandle
            );
            scarlet::device::manager::PROBE_DEFER
        })
}

fn resolve_irq(
    device: &PlatformDeviceInfo,
) -> Result<(Arc<dyn GpioController>, u32, GpioIrqTrigger), &'static str> {
    let controller_phandle = device
        .property("interrupt-parent")
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("elan-ekth3000: missing interrupt-parent")?;
    let interrupt = device
        .property("interrupts")
        .ok_or("elan-ekth3000: missing interrupts")?;
    let pin = read_be_u32(interrupt.value(), 0).ok_or("elan-ekth3000: malformed interrupts")?;
    let flags = read_be_u32(interrupt.value(), 4).ok_or("elan-ekth3000: malformed interrupts")?;
    let trigger = match flags & 0xf {
        1 => GpioIrqTrigger::RisingEdge,
        2 => GpioIrqTrigger::FallingEdge,
        4 => GpioIrqTrigger::HighLevel,
        8 => GpioIrqTrigger::LowLevel,
        _ => return Err("elan-ekth3000: unsupported IRQ trigger"),
    };
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(controller_phandle)
        .ok_or_else(|| {
            early_println!(
                "[elan-ekth3000] GPIO controller {:#x} is not ready, deferring",
                controller_phandle
            );
            scarlet::device::manager::PROBE_DEFER
        })?;
    Ok((controller, pin, trigger))
}

fn probe_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let bus = resolve_bus(device)?;
    bus.set_bus_speed(400_000)
        .map_err(|_| "elan-ekth3000: failed to set I2C bus speed")?;
    let address = read_address(device)?;
    let (irq_gpio, irq_pin, irq_trigger) = resolve_irq(device)?;

    // Scarlet has no regulator-consumer API yet. CoachZ firmware leaves
    // pp3300_fp_tp enabled, so retain that rail while taking over the device.
    let event_device = Arc::new(EventDevice::new("mouse"));
    let trackpad = Arc::new(ElanEkth3000 {
        bus,
        address,
        irq_gpio: irq_gpio.clone(),
        irq_pin,
        event_device: event_device.clone(),
        report_len: ETP_REPORT_LEN,
        max_y: 0,
        last_contact: IrqSpinLock::new(None),
        button_pressed: IrqSpinLock::new(false),
        work_pending: AtomicBool::new(false),
    });

    trackpad.initialize()?;
    let pattern_word = trackpad.read_command(ETP_PATTERN_CMD)?;
    let pattern = if pattern_word == u16::MAX {
        0
    } else {
        (pattern_word >> 8) as u8
    };
    let report_len = if pattern <= 1 {
        ETP_REPORT_LEN
    } else {
        ETP_REPORT_LEN_HIGH_PRECISION
    };
    let max_x = trackpad.read_command(ETP_MAX_X_CMD)?;
    let max_y = trackpad.read_command(ETP_MAX_Y_CMD)?;

    // Rebuild after querying geometry so the IRQ path has an immutable max_y.
    let trackpad = Arc::new(ElanEkth3000 {
        bus: trackpad.bus.clone(),
        address,
        irq_gpio: irq_gpio.clone(),
        irq_pin,
        event_device: event_device.clone(),
        report_len,
        max_y,
        last_contact: IrqSpinLock::new(None),
        button_pressed: IrqSpinLock::new(false),
        work_pending: AtomicBool::new(false),
    });

    irq_gpio.set_direction_input(irq_pin);
    if !irq_gpio.request_irq(irq_pin, irq_trigger, trackpad.clone()) {
        early_println!(
            "[elan-ekth3000] GPIO{} IRQ registration unavailable, deferring",
            irq_pin
        );
        return probe_defer();
    }

    *TRACKPAD_REGISTRY.lock() = Some(trackpad);
    ensure_irq_worker_started();

    DeviceManager::get_manager()
        .register_device_with_name(event_device.get_name().into(), event_device.clone());
    early_println!(
        "[elan-ekth3000] registered {} addr={:#x} GPIO{} {:?} report={} max={}x{} vcc=firmware-handoff",
        event_device.get_name(),
        address.raw(),
        irq_pin,
        irq_trigger,
        report_len,
        max_x,
        max_y
    );
    Ok(())
}

fn remove_fn(_device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    if let Some(trackpad) = TRACKPAD_REGISTRY.lock().take() {
        trackpad.work_pending.store(false, Ordering::Release);
        trackpad.irq_gpio.free_irq(trackpad.irq_pin);
    }
    Ok(())
}

fn register_driver() {
    DeviceManager::get_manager().register_driver(
        Box::new(PlatformDeviceDriver::new(
            "elan-ekth3000",
            probe_fn,
            remove_fn,
            vec!["elan,ekth3000"],
        )),
        DriverPriority::Standard,
    );
}

scarlet::driver_initcall!(register_driver);

static TRACKPAD_REGISTRY: IrqSpinLock<Option<Arc<ElanEkth3000>>> = IrqSpinLock::new(None);
static ELAN_IRQ_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static ELAN_IRQ_WORKER_WAKER: scarlet::sync::Waker =
    scarlet::sync::Waker::new_uninterruptible("elan-ekth3000-irq");

fn ensure_irq_worker_started() {
    if ELAN_IRQ_WORKER_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let task = scarlet::task::new_kernel_task("elan-ekth3000-irq".to_string(), 1, irq_worker_entry);
    task.init();
    scarlet::sched::scheduler::add_task(task, 0);
}

fn irq_worker_entry() {
    loop {
        let trackpad = TRACKPAD_REGISTRY.lock().as_ref().cloned();
        if trackpad.is_some_and(|trackpad| trackpad.process_deferred_interrupt_work()) {
            continue;
        }

        let Some(task) = scarlet::task::mytask() else {
            scarlet::arch::instruction::idle();
        };
        ELAN_IRQ_WORKER_WAKER.wait(task.get_id(), task.get_trapframe());
    }
}

#[used]
static SCARLET_DRIVER_ELAN_EKTH3000_ANCHOR: fn() = force_link;

/// Keep this external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}
