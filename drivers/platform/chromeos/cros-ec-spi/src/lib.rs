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

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use scarlet::{
    device::{
        DeviceInfo,
        events::InterruptCapableDevice,
        gpio::{GpioController, GpioIrqTrigger},
        manager::{DeviceManager, DriverPriority, probe_defer},
        platform::{PlatformDeviceDriver, PlatformDeviceInfo},
        spi::{SpiBus, SpiError, SpiTransfer},
    },
    interrupt::{InterruptId, InterruptResult},
    println,
    sync::{IrqSpinLock, Waker},
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

const COMMAND_GET_NEXT_EVENT: u16 = 0x0067;
const GET_NEXT_EVENT_VERSION: u8 = 2;
const EC_RESULT_UNAVAILABLE: u16 = 9;
const MKBP_MORE_EVENTS: u8 = 1 << 7;
const MKBP_EVENT_TYPE_MASK: u8 = MKBP_MORE_EVENTS - 1;
const MAX_EVENTS_PER_DRAIN: usize = 64;
const EVENT_RETRY_INITIAL_NS: u64 = 250_000_000;
const EVENT_RETRY_MAX_NS: u64 = 8_000_000_000;

/// Listener for decoded Chrome EC MKBP events.
pub trait CrosEcEventListener: Send + Sync {
    /// Receive one MKBP event in worker context.
    ///
    /// # Arguments
    ///
    /// * `event_type` - Raw MKBP event type with the `more` flag removed.
    /// * `data` - Event-specific response bytes.
    ///
    /// # Returns
    ///
    /// This callback has no return value.
    fn on_cros_ec_event(&self, event_type: u8, data: &[u8]);
}

struct EventListenerEntry {
    id: u64,
    listener: Weak<dyn CrosEcEventListener>,
}

#[derive(Debug, Eq, PartialEq)]
struct MkbpEvent<'a> {
    event_type: u8,
    more: bool,
    data: &'a [u8],
}

fn parse_mkbp_event(response: &[u8]) -> Result<MkbpEvent<'_>, CrosEcError> {
    let event = *response.first().ok_or(CrosEcError::InvalidResponse)?;
    Ok(MkbpEvent {
        event_type: event & MKBP_EVENT_TYPE_MASK,
        more: event & MKBP_MORE_EVENTS != 0,
        data: &response[1..],
    })
}

fn event_retry_delay_ns(error_count: u64) -> u64 {
    let shift = u32::try_from(error_count.saturating_sub(1).min(5)).unwrap_or(5);
    EVENT_RETRY_INITIAL_NS
        .saturating_mul(1u64 << shift)
        .min(EVENT_RETRY_MAX_NS)
}

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
    primary: bool,
    chip_select: u8,
    speed_hz: u32,
    command_lock: IrqSpinLock<()>,
    irq_gpio: Arc<dyn GpioController>,
    irq_pin: u32,
    active: AtomicBool,
    event_work_pending: AtomicBool,
    event_drain_errors: AtomicU64,
    event_retry_not_before_ns: AtomicU64,
    event_listeners: IrqSpinLock<Vec<EventListenerEntry>>,
}

impl CrosEcSpi {
    fn new(
        bus: Arc<dyn SpiBus>,
        phandle: u32,
        primary: bool,
        chip_select: u8,
        maximum_speed_hz: u32,
        irq_gpio: Arc<dyn GpioController>,
        irq_pin: u32,
    ) -> Self {
        Self {
            speed_hz: bus.bus_speed().min(maximum_speed_hz),
            bus,
            phandle,
            primary,
            chip_select,
            command_lock: IrqSpinLock::new(()),
            irq_gpio,
            irq_pin,
            active: AtomicBool::new(true),
            event_work_pending: AtomicBool::new(false),
            event_drain_errors: AtomicU64::new(0),
            event_retry_not_before_ns: AtomicU64::new(0),
            event_listeners: IrqSpinLock::new(Vec::new()),
        }
    }

    /// Device-tree phandle of this EC.
    pub const fn phandle(&self) -> u32 {
        self.phandle
    }

    /// Subscribe to all MKBP event types emitted by this EC.
    ///
    /// # Arguments
    ///
    /// * `listener` - Weak listener reference; dead listeners are pruned during dispatch.
    ///
    /// # Returns
    ///
    /// A stable registration identifier for [`Self::unregister_event_listener`].
    pub fn register_event_listener(&self, listener: Weak<dyn CrosEcEventListener>) -> u64 {
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
        self.event_listeners
            .lock()
            .push(EventListenerEntry { id, listener });
        id
    }

    /// Remove one MKBP event listener.
    ///
    /// # Arguments
    ///
    /// * `id` - Identifier returned by [`Self::register_event_listener`].
    ///
    /// # Returns
    ///
    /// `true` when a live registration was removed. A callback whose strong
    /// listener snapshot was already taken may finish concurrently.
    pub fn unregister_event_listener(&self, id: u64) -> bool {
        let mut listeners = self.event_listeners.lock();
        let previous_len = listeners.len();
        listeners.retain(|entry| entry.id != id);
        listeners.len() != previous_len
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

    fn dispatch_event(&self, event_type: u8, data: &[u8]) {
        let listeners = {
            let mut registered = self.event_listeners.lock();
            let mut live = Vec::with_capacity(registered.len());
            registered.retain(|entry| {
                if let Some(listener) = entry.listener.upgrade() {
                    live.push(listener);
                    true
                } else {
                    false
                }
            });
            live
        };
        for listener in listeners {
            if !self.active.load(Ordering::Acquire) {
                break;
            }
            listener.on_cros_ec_event(event_type, data);
        }
    }

    fn drain_events(&self) -> Result<(), CrosEcError> {
        for _ in 0..MAX_EVENTS_PER_DRAIN {
            let response = match self.command(COMMAND_GET_NEXT_EVENT, GET_NEXT_EVENT_VERSION, &[]) {
                Ok(response) => response,
                Err(CrosEcError::EcResult(EC_RESULT_UNAVAILABLE)) => return Ok(()),
                Err(error) => return Err(error),
            };
            let event = parse_mkbp_event(&response)?;
            self.dispatch_event(event.event_type, event.data);
            if !event.more {
                return Ok(());
            }
        }
        Err(CrosEcError::InvalidResponse)
    }

    fn finish_event_drain(&self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        self.irq_gpio.ack_irq(self.irq_pin);
        self.irq_gpio
            .enable_irq(self.irq_pin, GpioIrqTrigger::LowLevel);
    }
}

impl InterruptCapableDevice for CrosEcSpi {
    fn handle_interrupt(&self) -> InterruptResult<()> {
        if !self.active.load(Ordering::Acquire) {
            return Ok(());
        }
        // The TLMM callback already masked and acknowledged the level source.
        // Bump its registration generation so TLMM does not re-enable it on
        // return; all sleeping SPI work is deferred to the EC worker.
        self.irq_gpio.disable_irq(self.irq_pin);
        if !self.event_work_pending.swap(true, Ordering::AcqRel) {
            EVENT_WORKER_WAKER.wake_one();
        }
        Ok(())
    }

    fn interrupt_id(&self) -> Option<InterruptId> {
        None
    }
}

static CONTROLLERS: IrqSpinLock<Vec<Arc<CrosEcSpi>>> = IrqSpinLock::new(Vec::new());
static NEXT_LISTENER_ID: AtomicU64 = AtomicU64::new(1);
static EVENT_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static EVENT_WORKER_WAKER: Waker = Waker::new_interruptible("cros_ec_event_worker");

fn event_worker_entry() {
    loop {
        let controllers = CONTROLLERS.lock().clone();
        let now_ns = time::current_time_ns();
        let mut next_wait_ns: Option<u64> = None;
        for controller in controllers {
            if !controller.active.load(Ordering::Acquire)
                || !controller.event_work_pending.load(Ordering::Acquire)
            {
                continue;
            }
            let retry_not_before_ns = controller.event_retry_not_before_ns.load(Ordering::Acquire);
            if retry_not_before_ns > now_ns {
                let remaining = retry_not_before_ns - now_ns;
                next_wait_ns = Some(next_wait_ns.map_or(remaining, |wait| wait.min(remaining)));
                continue;
            }
            if !controller.event_work_pending.swap(false, Ordering::AcqRel) {
                continue;
            }
            match controller.drain_events() {
                Ok(()) => {
                    controller.event_drain_errors.store(0, Ordering::Relaxed);
                    controller
                        .event_retry_not_before_ns
                        .store(0, Ordering::Release);
                    controller.finish_event_drain();
                }
                Err(error) => {
                    let error_count = controller
                        .event_drain_errors
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    if error_count.is_power_of_two() {
                        println!(
                            "[cros-ec-spi] event drain failed: {:?} (count={})",
                            error, error_count
                        );
                    }
                    if controller.active.load(Ordering::Acquire) {
                        let retry_delay_ns = event_retry_delay_ns(error_count);
                        controller.event_retry_not_before_ns.store(
                            time::current_time_ns().saturating_add(retry_delay_ns),
                            Ordering::Release,
                        );
                        controller.event_work_pending.store(true, Ordering::Release);
                        next_wait_ns = Some(
                            next_wait_ns.map_or(retry_delay_ns, |wait| wait.min(retry_delay_ns)),
                        );
                    }
                }
            }
        }

        let Some(task) = scarlet::task::mytask() else {
            scarlet::arch::instruction::idle();
        };
        EVENT_WORKER_WAKER.wait_with_timeout(task.get_id(), task.get_trapframe(), next_wait_ns);
    }
}

fn ensure_event_worker_started() {
    if EVENT_WORKER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let task = scarlet::task::new_kernel_task("cros-ec-event-worker".into(), 1, event_worker_entry);
    task.init();
    scarlet::sched::scheduler::add_task(task, scarlet::arch::get_cpu().get_cpuid());
}

/// Look up a probed Chrome EC by its device-tree phandle.
pub fn get_cros_ec_spi_by_phandle(phandle: u32) -> Option<Arc<CrosEcSpi>> {
    CONTROLLERS
        .lock()
        .iter()
        .find(|controller| controller.phandle() == phandle)
        .cloned()
}

/// Return the primary AP EC rather than a secondary fingerprint EC.
pub fn get_primary_cros_ec_spi() -> Option<Arc<CrosEcSpi>> {
    CONTROLLERS
        .lock()
        .iter()
        .find(|controller| controller.primary)
        .cloned()
}

fn read_u32_property(device: &PlatformDeviceInfo, name: &str) -> Option<u32> {
    device
        .property(name)
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let word = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
}

fn resolve_irq(
    device: &PlatformDeviceInfo,
) -> Result<(Arc<dyn GpioController>, u32), &'static str> {
    let controller_phandle = device
        .property("interrupt-parent")
        .and_then(|property| property.as_usize())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("cros-ec-spi: missing interrupt-parent")?;
    let interrupts = device
        .property("interrupts")
        .ok_or("cros-ec-spi: missing interrupts")?;
    let pin = read_be_u32(interrupts.value(), 0).ok_or("cros-ec-spi: malformed interrupts")?;
    let flags = read_be_u32(interrupts.value(), 4).ok_or("cros-ec-spi: malformed interrupts")?;
    if flags & 0xf != 8 {
        return Err("cros-ec-spi: EC interrupt is not level-low");
    }
    let controller = DeviceManager::get_manager()
        .get_gpio_controller(controller_phandle)
        .ok_or_else(|| {
            println!(
                "[cros-ec-spi] GPIO controller {:#x} is not ready, deferring",
                controller_phandle
            );
            scarlet::device::manager::PROBE_DEFER
        })?;
    Ok((controller, pin))
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
    let primary = !device.compatible().contains(&"google,cros-ec-fp");
    let (irq_gpio, irq_pin) = resolve_irq(device)?;
    let controller = Arc::new(CrosEcSpi::new(
        bus,
        phandle,
        primary,
        chip_select,
        maximum_speed,
        irq_gpio.clone(),
        irq_pin,
    ));
    irq_gpio.set_direction_input(irq_pin);
    if !irq_gpio.request_irq(irq_pin, GpioIrqTrigger::LowLevel, controller.clone()) {
        return Err("cros-ec-spi: failed to request EC GPIO IRQ");
    }
    CONTROLLERS.lock().push(controller.clone());
    ensure_event_worker_started();
    // Preserve a level already asserted between request_irq() and publication.
    if !irq_gpio.get_value(irq_pin) {
        controller.event_work_pending.store(true, Ordering::Release);
        EVENT_WORKER_WAKER.wake_one();
    }
    println!(
        "[cros-ec-spi] registered {} phandle={:#x} bus={:#x} cs={} speed={} Hz role={} irq=GPIO{} LowLevel",
        device.name(),
        phandle,
        bus_phandle,
        chip_select,
        controller.speed_hz,
        if primary { "primary" } else { "fingerprint" },
        irq_pin,
    );
    Ok(())
}

fn remove_fn(device: &PlatformDeviceInfo) -> Result<(), &'static str> {
    let Some(phandle) =
        read_u32_property(device, "phandle").or_else(|| read_u32_property(device, "linux,phandle"))
    else {
        return Ok(());
    };
    let controller = {
        let mut controllers = CONTROLLERS.lock();
        let Some(index) = controllers
            .iter()
            .position(|controller| controller.phandle() == phandle)
        else {
            return Ok(());
        };
        controllers[index].active.store(false, Ordering::Release);
        controllers.remove(index)
    };
    controller
        .event_work_pending
        .store(false, Ordering::Release);
    controller
        .event_retry_not_before_ns
        .store(0, Ordering::Release);
    controller.irq_gpio.free_irq(controller.irq_pin);
    controller.event_listeners.lock().clear();
    EVENT_WORKER_WAKER.wake_one();
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

#[cfg(target_os = "none")]
scarlet::driver_initcall!(register_driver);

#[used]
static SCARLET_DRIVER_CROS_EC_SPI_ANCHOR: fn() = force_link;

/// Keep the external driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mkbp_type_more_flag_and_payload() {
        let response = [MKBP_MORE_EVENTS | 4, 3, 0, 0, 0];
        assert_eq!(
            parse_mkbp_event(&response),
            Ok(MkbpEvent {
                event_type: 4,
                more: true,
                data: &[3, 0, 0, 0],
            })
        );
    }

    #[test]
    fn accepts_terminal_event_and_rejects_empty_response() {
        assert_eq!(
            parse_mkbp_event(&[4, 2, 0, 0, 0]),
            Ok(MkbpEvent {
                event_type: 4,
                more: false,
                data: &[2, 0, 0, 0],
            })
        );
        assert_eq!(parse_mkbp_event(&[]), Err(CrosEcError::InvalidResponse));
    }

    #[test]
    fn parses_big_endian_irq_cells() {
        assert_eq!(read_be_u32(&[0, 0, 0, 94, 0, 0, 0, 8], 0), Some(94));
        assert_eq!(read_be_u32(&[0, 0, 0, 94, 0, 0, 0, 8], 4), Some(8));
        assert_eq!(read_be_u32(&[0, 0, 0], 0), None);
    }

    #[test]
    fn event_retry_uses_bounded_exponential_backoff() {
        assert_eq!(event_retry_delay_ns(0), EVENT_RETRY_INITIAL_NS);
        assert_eq!(event_retry_delay_ns(1), EVENT_RETRY_INITIAL_NS);
        assert_eq!(event_retry_delay_ns(2), 500_000_000);
        assert_eq!(event_retry_delay_ns(3), 1_000_000_000);
        assert_eq!(event_retry_delay_ns(6), EVENT_RETRY_MAX_NS);
        assert_eq!(event_retry_delay_ns(u64::MAX), EVENT_RETRY_MAX_NS);
    }
}
