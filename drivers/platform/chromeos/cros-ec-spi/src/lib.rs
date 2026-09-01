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
const COMMAND_PWM_GET_DUTY: u16 = 0x0026;
const COMMAND_GET_FEATURES: u16 = 0x000d;
const PWM_DISPLAY_LIGHT: u8 = 2;
const PWM_FULL_SCALE: u32 = 0xffff;

const COMMAND_MOTION_SENSE: u16 = 0x002b;
const MOTION_SENSE_VERSION_LEGACY: u8 = 1;
const MOTION_SENSE_VERSION_FIFO: u8 = 2;
const MOTION_SENSE_VERSION_INFO_EXTENDED: u8 = 3;
const MOTION_SENSE_COMMAND_DUMP: u8 = 0;
const MOTION_SENSE_COMMAND_INFO: u8 = 1;
const MOTION_SENSE_COMMAND_SENSOR_ODR: u8 = 3;
const MOTION_SENSE_COMMAND_SENSOR_RANGE: u8 = 4;
const MOTION_SENSE_COMMAND_DATA: u8 = 6;
const MOTION_SENSE_COMMAND_FIFO_INFO: u8 = 7;
const MOTION_SENSE_COMMAND_FIFO_READ: u8 = 9;
const MOTION_SENSE_COMMAND_FIFO_INT_ENABLE: u8 = 15;
const MOTION_SENSE_REQUEST_BYTES: usize = 13;
const MOTION_SENSOR_DATA_BYTES: usize = 8;
const MOTION_FIFO_INFO_FIXED_BYTES: usize = 10;
const MOTION_FIFO_READ_FIXED_BYTES: usize = 4;
const MKBP_SENSOR_FIFO_ALIGNMENT_BYTES: usize = 3;
const MKBP_SENSOR_FIFO_EVENT_BYTES: usize =
    MKBP_SENSOR_FIFO_ALIGNMENT_BYTES + MOTION_FIFO_INFO_FIXED_BYTES;
const MOTION_FIFO_INT_QUERY: i8 = -1;
const MOTION_SENSE_QUERY_VALUE: i32 = -1;
const EC_RESULT_INVALID_VERSION: u16 = 6;
const EC_FEATURE_MOTION_SENSE: u8 = 6;
const EC_FEATURE_MOTION_SENSE_FIFO: u8 = 24;
const EC_FEATURE_MOTION_SENSE_TIGHT_TIMESTAMPS: u8 = 36;

/// FIFO entry marks a synchronization flush; its union payload is metadata.
pub const CROS_EC_MOTION_SENSOR_FLAG_FLUSH: u8 = 1 << 0;
/// FIFO entry stores an EC timestamp in the data union rather than XYZ values.
pub const CROS_EC_MOTION_SENSOR_FLAG_TIMESTAMP: u8 = 1 << 1;
/// FIFO entry originated from a wake-up sensor.
pub const CROS_EC_MOTION_SENSOR_FLAG_WAKEUP: u8 = 1 << 2;
/// FIFO entry reports an EC tablet-mode transition rather than XYZ values.
pub const CROS_EC_MOTION_SENSOR_FLAG_TABLET_MODE: u8 = 1 << 3;
/// FIFO entry reports a sensor ODR change rather than XYZ values.
pub const CROS_EC_MOTION_SENSOR_FLAG_ODR: u8 = 1 << 4;

// At the inherited 1.01 MHz Trogdor EC clock this is about 32 ms of polling.
// PWM_SET_DUTY completes quickly, while keeping the allocation bounded and the
// complete response inside one CS assertion.
const RESPONSE_CLOCK_BYTES: usize = 4096;
const CHIP_SELECT_COOLDOWN_US: u64 = 200;
// A 256-vector response is 2052 payload bytes, leaving ample room for the
// SPI response-start bytes and protocol-3 header inside the fixed 4096-byte
// clock window.  Consumers must drain larger backlogs in chunks.
/// Largest motion FIFO batch that fits safely in one EC SPI response window.
///
/// Callers draining a larger backlog must issue multiple reads.
pub const CROS_EC_MAX_MOTION_FIFO_VECTORS: u32 = 256;

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
    /// `true` when the event was consumed successfully. Returning `false`
    /// keeps the EC IRQ masked and makes the worker retry with bounded
    /// backoff, so a level-triggered source cannot turn a broken listener into
    /// a kernel busy loop.
    fn on_cros_ec_event(&self, event_type: u8, data: &[u8]) -> bool;
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
    /// A registered MKBP listener could not consume an event.
    EventListenerFailed,
}

/// Summary returned by the Chrome EC motion-sense `DUMP` command.
///
/// The EC owns `sensor_count` motion sensors and advertises their module-wide
/// status bits through `module_flags`.  A zero-count `DUMP` intentionally does
/// not include any individual sensor samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrosEcMotionSensorSummary {
    /// EC-defined module status flags.
    pub module_flags: u8,
    /// Number of motion sensors managed by the EC.
    pub sensor_count: u8,
}

/// Static metadata for one Chrome EC motion sensor.
///
/// `sensor_type`, `location`, and `chip` retain the numeric values from the
/// Chromium EC protocol.  The extended frequency and FIFO fields are absent
/// when an older EC accepts only INFO version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrosEcMotionSensorInfo {
    /// Protocol `motionsensor_type` value.
    pub sensor_type: u8,
    /// Protocol `motionsensor_location` value.
    pub location: u8,
    /// Protocol `motionsensor_chip` value.
    pub chip: u8,
    /// Minimum sampling frequency in millihertz, if INFO version 3 was used.
    pub min_frequency_millihz: Option<u32>,
    /// Maximum sampling frequency in millihertz, if INFO version 3 was used.
    pub max_frequency_millihz: Option<u32>,
    /// Maximum FIFO event count, if INFO version 3 was used.
    pub fifo_max_event_count: Option<u32>,
}

/// One three-axis motion-sensor sample returned by the Chrome EC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrosEcMotionSensorData {
    /// EC-defined per-sample flags.
    pub flags: u8,
    /// Sensor index that produced this sample.
    pub sensor_num: u8,
    /// Raw X-axis value in the sensor's protocol-defined unit.
    pub x: i16,
    /// Raw Y-axis value in the sensor's protocol-defined unit.
    pub y: i16,
    /// Raw Z-axis value in the sensor's protocol-defined unit.
    pub z: i16,
}

impl CrosEcMotionSensorData {
    /// Return the EC timestamp carried by a timestamp FIFO entry.
    ///
    /// The Chromium EC overlays the XYZ union with a reserved 16-bit word and
    /// a little-endian 32-bit timestamp.  `None` means this is not a timestamp
    /// entry, so callers must not infer a timestamp from normal axis data.
    ///
    /// # Returns
    ///
    /// The timestamp in microseconds for a timestamp entry, otherwise `None`.
    pub const fn timestamp_us(&self) -> Option<u32> {
        if self.flags & CROS_EC_MOTION_SENSOR_FLAG_TIMESTAMP == 0 {
            return None;
        }
        Some((self.y as u16 as u32) | ((self.z as u16 as u32) << 16))
    }

    /// Return whether this FIFO entry contains an XYZ vector sample.
    ///
    /// Wake-up is an annotation on an otherwise normal sample, but timestamp,
    /// flush, ODR, and tablet-mode entries repurpose the payload union and are
    /// therefore classified as metadata.
    ///
    /// # Returns
    ///
    /// `true` only when the XYZ fields are a sensor vector rather than
    /// protocol metadata.
    pub const fn is_vector_sample(&self) -> bool {
        self.flags
            & (CROS_EC_MOTION_SENSOR_FLAG_TIMESTAMP
                | CROS_EC_MOTION_SENSOR_FLAG_FLUSH
                | CROS_EC_MOTION_SENSOR_FLAG_ODR
                | CROS_EC_MOTION_SENSOR_FLAG_TABLET_MODE)
            == 0
    }
}

/// Status of the Chrome EC motion FIFO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrosEcMotionFifoInfo {
    /// Total FIFO capacity in vectors.
    pub size: u16,
    /// Vectors currently buffered by the EC.
    pub count: u16,
    /// EC timestamp in microseconds for the FIFO notification.
    pub timestamp_us: u32,
    /// Total number of vectors lost since boot.
    pub total_lost: u16,
    /// Vectors lost since the preceding `FIFO_INFO`, indexed by sensor.
    pub lost_per_sensor: Vec<u16>,
}

/// A bounded batch of motion samples drained from the Chrome EC FIFO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrosEcMotionFifoData {
    /// Samples in the same order in which the EC returned them.
    pub samples: Vec<CrosEcMotionSensorData>,
}

/// Feature bits advertised by the Chrome EC through `EC_CMD_GET_FEATURES`.
///
/// The wire response contains two little-endian 32-bit words, representing
/// feature numbers 0 through 63.  Unknown feature numbers are retained so
/// callers can make conservative compatibility decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrosEcFeatures {
    flags: [u32; 2],
}

impl CrosEcFeatures {
    /// Return whether this EC advertises a feature number in the 0..64 range.
    ///
    /// # Arguments
    ///
    /// * `feature` - Chromium EC feature number to inspect.
    ///
    /// # Returns
    ///
    /// `true` when the feature bit is set; feature numbers outside 0..64 are
    /// reported as unsupported.
    pub const fn has_feature(&self, feature: u8) -> bool {
        if feature >= 64 {
            return false;
        }
        let word = (feature / 32) as usize;
        let bit = feature % 32;
        self.flags[word] & (1u32 << bit) != 0
    }

    /// Return whether the EC owns motion sensors.
    ///
    /// # Returns
    ///
    /// `true` when feature bit 6 is set.
    pub const fn supports_motion_sense(&self) -> bool {
        self.has_feature(EC_FEATURE_MOTION_SENSE)
    }

    /// Return whether the EC implements the motion-sensor FIFO commands.
    ///
    /// # Returns
    ///
    /// `true` when feature bit 24 is set.
    pub const fn supports_motion_sense_fifo(&self) -> bool {
        self.has_feature(EC_FEATURE_MOTION_SENSE_FIFO)
    }

    /// Return whether FIFO samples use the EC's tight-timestamp extension.
    ///
    /// # Returns
    ///
    /// `true` when feature bit 36 is set.
    pub const fn supports_motion_sense_tight_timestamps(&self) -> bool {
        self.has_feature(EC_FEATURE_MOTION_SENSE_TIGHT_TIMESTAMPS)
    }
}

impl From<SpiError> for CrosEcError {
    fn from(error: SpiError) -> Self {
        Self::Spi(error)
    }
}

fn pwm_duty_from_percent(percent: u8) -> Result<u16, CrosEcError> {
    if percent > 100 {
        return Err(CrosEcError::InvalidArgument);
    }
    let scaled = u32::from(percent)
        .checked_mul(PWM_FULL_SCALE)
        .ok_or(CrosEcError::InvalidArgument)?;
    u16::try_from((scaled + 50) / 100).map_err(|_| CrosEcError::InvalidArgument)
}

fn pwm_percent_from_duty(duty: u16) -> u8 {
    let scaled = u32::from(duty)
        .saturating_mul(100)
        .saturating_add(PWM_FULL_SCALE / 2)
        / PWM_FULL_SCALE;
    u8::try_from(scaled.min(100)).unwrap_or(100)
}

fn parse_features(response: &[u8]) -> Result<CrosEcFeatures, CrosEcError> {
    if response.len() != 8 {
        return Err(CrosEcError::InvalidResponse);
    }
    Ok(CrosEcFeatures {
        flags: [
            u32::from_le_bytes([response[0], response[1], response[2], response[3]]),
            u32::from_le_bytes([response[4], response[5], response[6], response[7]]),
        ],
    })
}

fn build_motion_sense_payload(
    subcommand: u8,
    parameters: &[u8],
) -> Result<[u8; MOTION_SENSE_REQUEST_BYTES], CrosEcError> {
    if parameters.len() >= MOTION_SENSE_REQUEST_BYTES {
        return Err(CrosEcError::InvalidArgument);
    }
    // The Chromium EC ABI sends sizeof(struct ec_params_motion_sense), whose
    // packed command byte plus 12-byte SET_ACTIVITY union member occupies 13 B.
    let mut payload = [0u8; MOTION_SENSE_REQUEST_BYTES];
    payload[0] = subcommand;
    payload[1..1 + parameters.len()].copy_from_slice(parameters);
    Ok(payload)
}

fn parse_motion_sensor_summary(response: &[u8]) -> Result<CrosEcMotionSensorSummary, CrosEcError> {
    let [module_flags, sensor_count] = response else {
        return Err(CrosEcError::InvalidResponse);
    };
    Ok(CrosEcMotionSensorSummary {
        module_flags: *module_flags,
        sensor_count: *sensor_count,
    })
}

fn parse_motion_sensor_info_v1(response: &[u8]) -> Result<CrosEcMotionSensorInfo, CrosEcError> {
    let [sensor_type, location, chip] = response else {
        return Err(CrosEcError::InvalidResponse);
    };
    Ok(CrosEcMotionSensorInfo {
        sensor_type: *sensor_type,
        location: *location,
        chip: *chip,
        min_frequency_millihz: None,
        max_frequency_millihz: None,
        fifo_max_event_count: None,
    })
}

fn parse_motion_sensor_info_v3(response: &[u8]) -> Result<CrosEcMotionSensorInfo, CrosEcError> {
    if response.len() != 16 {
        return Err(CrosEcError::InvalidResponse);
    }
    Ok(CrosEcMotionSensorInfo {
        sensor_type: response[0],
        location: response[1],
        chip: response[2],
        // INFO v3 uses C's natural 4-byte alignment after the three u8 fields.
        min_frequency_millihz: Some(u32::from_le_bytes([
            response[4],
            response[5],
            response[6],
            response[7],
        ])),
        max_frequency_millihz: Some(u32::from_le_bytes([
            response[8],
            response[9],
            response[10],
            response[11],
        ])),
        fifo_max_event_count: Some(u32::from_le_bytes([
            response[12],
            response[13],
            response[14],
            response[15],
        ])),
    })
}

fn parse_motion_sensor_data(response: &[u8]) -> Result<CrosEcMotionSensorData, CrosEcError> {
    if response.len() != MOTION_SENSOR_DATA_BYTES {
        return Err(CrosEcError::InvalidResponse);
    }
    Ok(CrosEcMotionSensorData {
        flags: response[0],
        sensor_num: response[1],
        x: i16::from_le_bytes([response[2], response[3]]),
        y: i16::from_le_bytes([response[4], response[5]]),
        z: i16::from_le_bytes([response[6], response[7]]),
    })
}

fn parse_motion_fifo_info(
    response: &[u8],
    sensor_count: u8,
) -> Result<CrosEcMotionFifoInfo, CrosEcError> {
    let lost_bytes = usize::from(sensor_count)
        .checked_mul(2)
        .ok_or(CrosEcError::InvalidResponse)?;
    let expected_len = MOTION_FIFO_INFO_FIXED_BYTES
        .checked_add(lost_bytes)
        .ok_or(CrosEcError::InvalidResponse)?;
    if response.len() != expected_len {
        return Err(CrosEcError::InvalidResponse);
    }
    let mut lost_per_sensor = Vec::with_capacity(usize::from(sensor_count));
    for bytes in response[MOTION_FIFO_INFO_FIXED_BYTES..].chunks_exact(2) {
        lost_per_sensor.push(u16::from_le_bytes([bytes[0], bytes[1]]));
    }
    Ok(CrosEcMotionFifoInfo {
        size: u16::from_le_bytes([response[0], response[1]]),
        count: u16::from_le_bytes([response[2], response[3]]),
        timestamp_us: u32::from_le_bytes([response[4], response[5], response[6], response[7]]),
        total_lost: u16::from_le_bytes([response[8], response[9]]),
        lost_per_sensor,
    })
}

/// Decode the fixed FIFO snapshot embedded in an MKBP sensor-FIFO event.
///
/// The event data starts with three alignment bytes followed by the fixed
/// ten-byte `ec_response_motion_sense_fifo_info`. Per-sensor loss counters do
/// not fit in the MKBP event and must be queried separately when
/// `total_lost != 0`.
pub fn parse_motion_fifo_event(data: &[u8]) -> Result<CrosEcMotionFifoInfo, CrosEcError> {
    if data.len() != MKBP_SENSOR_FIFO_EVENT_BYTES {
        return Err(CrosEcError::InvalidResponse);
    }
    parse_motion_fifo_info(&data[MKBP_SENSOR_FIFO_ALIGNMENT_BYTES..], 0)
}

fn parse_motion_fifo_data(
    response: &[u8],
    requested_vectors: u32,
) -> Result<CrosEcMotionFifoData, CrosEcError> {
    if response.len() < MOTION_FIFO_READ_FIXED_BYTES {
        return Err(CrosEcError::InvalidResponse);
    }
    let count = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
    if count > requested_vectors || count > CROS_EC_MAX_MOTION_FIFO_VECTORS {
        return Err(CrosEcError::InvalidResponse);
    }
    let sample_bytes = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(MOTION_SENSOR_DATA_BYTES))
        .ok_or(CrosEcError::InvalidResponse)?;
    let expected_len = MOTION_FIFO_READ_FIXED_BYTES
        .checked_add(sample_bytes)
        .ok_or(CrosEcError::InvalidResponse)?;
    if response.len() != expected_len {
        return Err(CrosEcError::InvalidResponse);
    }
    let mut samples = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for bytes in response[MOTION_FIFO_READ_FIXED_BYTES..].chunks_exact(MOTION_SENSOR_DATA_BYTES) {
        samples.push(parse_motion_sensor_data(bytes)?);
    }
    Ok(CrosEcMotionFifoData { samples })
}

fn parse_motion_fifo_interrupt_enabled(response: &[u8]) -> Result<bool, CrosEcError> {
    if response.len() != 4 {
        return Err(CrosEcError::InvalidResponse);
    }
    match i32::from_le_bytes([response[0], response[1], response[2], response[3]]) {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CrosEcError::InvalidResponse),
    }
}

fn parse_motion_i32_response(response: &[u8]) -> Result<i32, CrosEcError> {
    if response.len() != 4 {
        return Err(CrosEcError::InvalidResponse);
    }
    Ok(i32::from_le_bytes([
        response[0],
        response[1],
        response[2],
        response[3],
    ]))
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

    fn motion_sense_command(
        &self,
        version: u8,
        subcommand: u8,
        parameters: &[u8],
    ) -> Result<Vec<u8>, CrosEcError> {
        let payload = build_motion_sense_payload(subcommand, parameters)?;
        self.command(COMMAND_MOTION_SENSE, version, &payload)
    }

    /// Read the two-word Chrome EC feature bitmap.
    ///
    /// The response to `EC_CMD_GET_FEATURES` must be exactly two
    /// little-endian 32-bit words.
    ///
    /// # Returns
    ///
    /// The complete 64-bit feature bitmap, or a transport/protocol error.
    pub fn features(&self) -> Result<CrosEcFeatures, CrosEcError> {
        let response = self.command(COMMAND_GET_FEATURES, 0, &[])?;
        parse_features(&response)
    }

    /// Return whether this EC advertises motion-sensor support.
    ///
    /// # Returns
    ///
    /// `true` when the live EC feature bitmap includes motion sense.
    pub fn supports_motion_sense(&self) -> Result<bool, CrosEcError> {
        Ok(self.features()?.supports_motion_sense())
    }

    /// Return whether this EC advertises motion FIFO support.
    ///
    /// # Returns
    ///
    /// `true` when the live EC feature bitmap includes the motion FIFO.
    pub fn supports_motion_sense_fifo(&self) -> Result<bool, CrosEcError> {
        Ok(self.features()?.supports_motion_sense_fifo())
    }

    /// Return whether this EC advertises tight motion-sensor timestamps.
    ///
    /// # Returns
    ///
    /// `true` when the live EC feature bitmap includes tight timestamps.
    pub fn supports_motion_sense_tight_timestamps(&self) -> Result<bool, CrosEcError> {
        Ok(self.features()?.supports_motion_sense_tight_timestamps())
    }

    /// Read the display backlight level as a rounded integer percentage.
    ///
    /// This uses `EC_CMD_PWM_GET_DUTY` with display-light PWM type 2 and
    /// index 0.  The EC response is required to contain exactly one
    /// little-endian 16-bit duty value.
    ///
    /// # Returns
    ///
    /// The rounded display brightness in the inclusive 0..=100 range.
    pub fn get_display_backlight_percent(&self) -> Result<u8, CrosEcError> {
        let response = self.command(COMMAND_PWM_GET_DUTY, 0, &[PWM_DISPLAY_LIGHT, 0])?;
        if response.len() != 2 {
            return Err(CrosEcError::InvalidResponse);
        }
        Ok(pwm_percent_from_duty(u16::from_le_bytes([
            response[0],
            response[1],
        ])))
    }

    /// Set the display backlight to an integer percentage.
    ///
    /// The value is rounded to the nearest representable Chrome EC 16-bit PWM
    /// duty cycle.  It must be within the inclusive range 0 through 100.
    ///
    /// # Arguments
    ///
    /// * `percent` - Requested display brightness in the inclusive 0..=100
    ///   range.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the EC accepted an empty successful response, or an
    /// error if the percentage, transport, or response is invalid.
    pub fn set_display_backlight_percent(&self, percent: u8) -> Result<(), CrosEcError> {
        let duty = pwm_duty_from_percent(percent)?;
        let duty = duty.to_le_bytes();
        let payload = [duty[0], duty[1], PWM_DISPLAY_LIGHT, 0];
        let response = self.command(COMMAND_PWM_SET_DUTY, 0, &payload)?;
        if !response.is_empty() {
            return Err(CrosEcError::InvalidResponse);
        }
        Ok(())
    }

    /// Return the Chrome EC's module-wide motion-sensor state and sensor count.
    ///
    /// The method issues `MOTIONSENSE_CMD_DUMP` with `max_sensor_count = 0`,
    /// so the EC returns exactly the two-byte summary without snapshot data.
    ///
    /// # Returns
    ///
    /// The module flags and sensor count, or an error for a malformed/failed
    /// host command.
    pub fn motion_sensor_summary(&self) -> Result<CrosEcMotionSensorSummary, CrosEcError> {
        let response = self.motion_sense_command(
            MOTION_SENSE_VERSION_LEGACY,
            MOTION_SENSE_COMMAND_DUMP,
            &[0],
        )?;
        parse_motion_sensor_summary(&response)
    }

    /// Read static metadata for one Chrome EC motion sensor.
    ///
    /// INFO version 3 exposes sensor frequency and FIFO capacity.  Firmware
    /// which reports `EC_RES_INVALID_VERSION` is retried once with INFO
    /// version 1, preserving the three fields that older firmware supports.
    ///
    /// # Arguments
    ///
    /// * `sensor_num` - EC motion-sensor index obtained from the summary.
    ///
    /// # Returns
    ///
    /// The sensor metadata. Extended fields are `None` after an INFO v1
    /// fallback.
    pub fn motion_sensor_info(
        &self,
        sensor_num: u8,
    ) -> Result<CrosEcMotionSensorInfo, CrosEcError> {
        match self.motion_sense_command(
            MOTION_SENSE_VERSION_INFO_EXTENDED,
            MOTION_SENSE_COMMAND_INFO,
            &[sensor_num],
        ) {
            Ok(response) => parse_motion_sensor_info_v3(&response),
            Err(CrosEcError::EcResult(EC_RESULT_INVALID_VERSION)) => {
                let response = self.motion_sense_command(
                    MOTION_SENSE_VERSION_LEGACY,
                    MOTION_SENSE_COMMAND_INFO,
                    &[sensor_num],
                )?;
                parse_motion_sensor_info_v1(&response)
            }
            Err(error) => Err(error),
        }
    }

    /// Read a motion sensor's current output-data rate in millihertz.
    ///
    /// The query sends the Chromium EC `EC_MOTION_SENSE_NO_VALUE` sentinel
    /// rather than changing the sampling configuration.  The response is an
    /// exact little-endian signed 32-bit value.
    ///
    /// # Arguments
    ///
    /// * `sensor_num` - EC motion-sensor index to query.
    ///
    /// # Returns
    ///
    /// The current output-data rate in millihertz.
    pub fn motion_sensor_odr_millihz(&self, sensor_num: u8) -> Result<i32, CrosEcError> {
        self.motion_sensor_parameter_query(MOTION_SENSE_COMMAND_SENSOR_ODR, sensor_num)
    }

    /// Read a motion sensor's current range in its protocol-defined unit.
    ///
    /// Accelerometers use ±g and gyroscopes use ±degrees/second.  The query
    /// does not modify the configuration and requires an exact signed 32-bit
    /// little-endian response.
    ///
    /// # Arguments
    ///
    /// * `sensor_num` - EC motion-sensor index to query.
    ///
    /// # Returns
    ///
    /// The current range in the queried sensor type's protocol unit.
    pub fn motion_sensor_range(&self, sensor_num: u8) -> Result<i32, CrosEcError> {
        self.motion_sensor_parameter_query(MOTION_SENSE_COMMAND_SENSOR_RANGE, sensor_num)
    }

    fn motion_sensor_parameter_query(
        &self,
        subcommand: u8,
        sensor_num: u8,
    ) -> Result<i32, CrosEcError> {
        let mut parameters = [0u8; 8];
        parameters[0] = sensor_num;
        // roundup = 0 and reserved = 0; -1 is EC_MOTION_SENSE_NO_VALUE.
        parameters[4..8].copy_from_slice(&MOTION_SENSE_QUERY_VALUE.to_le_bytes());
        let response =
            self.motion_sense_command(MOTION_SENSE_VERSION_LEGACY, subcommand, &parameters)?;
        parse_motion_i32_response(&response)
    }

    /// Read one current three-axis sample from a Chrome EC motion sensor.
    ///
    /// The legacy motion-sense command version has the broadest firmware
    /// compatibility.  The returned response is required to be the exact
    /// eight-byte `ec_response_motion_sensor_data` wire structure.
    ///
    /// # Arguments
    ///
    /// * `sensor_num` - EC motion-sensor index to sample.
    ///
    /// # Returns
    ///
    /// One current three-axis sample with the EC-provided flags and source
    /// sensor index.
    pub fn motion_sensor_data(
        &self,
        sensor_num: u8,
    ) -> Result<CrosEcMotionSensorData, CrosEcError> {
        let response = self.motion_sense_command(
            MOTION_SENSE_VERSION_LEGACY,
            MOTION_SENSE_COMMAND_DATA,
            &[sensor_num],
        )?;
        parse_motion_sensor_data(&response)
    }

    /// Query whether motion FIFO notifications are enabled at the EC.
    ///
    /// `MOTIONSENSE_CMD_FIFO_INT_ENABLE` accepts -1 as a query and returns an
    /// exact little-endian signed 32-bit 0 or 1 state value.
    ///
    /// # Returns
    ///
    /// Whether the EC currently emits motion FIFO notifications.
    pub fn motion_fifo_interrupt_enabled(&self) -> Result<bool, CrosEcError> {
        self.motion_fifo_interrupt_enabled_raw(MOTION_FIFO_INT_QUERY)
    }

    /// Enable or disable Chrome EC MKBP motion-FIFO notifications.
    ///
    /// The returned boolean is the state confirmed by the EC after the update.
    ///
    /// # Arguments
    ///
    /// * `enabled` - Whether the EC should emit motion FIFO notifications.
    ///
    /// # Returns
    ///
    /// The state confirmed by the EC after processing the request.
    pub fn set_motion_fifo_interrupt_enabled(&self, enabled: bool) -> Result<bool, CrosEcError> {
        self.motion_fifo_interrupt_enabled_raw(i8::from(enabled))
    }

    fn motion_fifo_interrupt_enabled_raw(&self, value: i8) -> Result<bool, CrosEcError> {
        let response = self.motion_sense_command(
            MOTION_SENSE_VERSION_FIFO,
            MOTION_SENSE_COMMAND_FIFO_INT_ENABLE,
            &[value as u8],
        )?;
        parse_motion_fifo_interrupt_enabled(&response)
    }

    /// Return motion FIFO capacity, fill state, timestamp, and loss counters.
    ///
    /// The EC's variable-length response contains one 16-bit loss counter per
    /// sensor.  This method first obtains the authoritative sensor count from
    /// a zero-count `DUMP` and rejects any response with a different length.
    ///
    /// # Returns
    ///
    /// FIFO status including exactly one loss counter for every EC sensor.
    pub fn motion_fifo_info(&self) -> Result<CrosEcMotionFifoInfo, CrosEcError> {
        let summary = self.motion_sensor_summary()?;
        self.motion_fifo_info_for_sensor_count(summary.sensor_count)
    }

    /// Return motion FIFO status using a sensor count already obtained at probe.
    ///
    /// Sensor hubs that cached the immutable count from
    /// [`Self::motion_sensor_summary`] should use this method for each FIFO
    /// event: it issues only `MOTIONSENSE_CMD_FIFO_INFO` and still requires the
    /// response to contain exactly one loss counter per supplied sensor.
    ///
    /// # Arguments
    ///
    /// * `sensor_count` - Immutable count returned during sensor-hub probe.
    ///
    /// # Returns
    ///
    /// FIFO status when the exact response length matches `sensor_count`.
    pub fn motion_fifo_info_for_sensor_count(
        &self,
        sensor_count: u8,
    ) -> Result<CrosEcMotionFifoInfo, CrosEcError> {
        // Linux and Chromium EC send only the command byte for FIFO_INFO.
        // Padding this parameterless request to sizeof(ec_params_motion_sense)
        // is not accepted by every EC implementation.
        let response = self.command(
            COMMAND_MOTION_SENSE,
            MOTION_SENSE_VERSION_FIFO,
            &[MOTION_SENSE_COMMAND_FIFO_INFO],
        )?;
        parse_motion_fifo_info(&response, sensor_count)
    }

    /// Drain up to `max_vectors` motion samples from the EC FIFO.
    ///
    /// Requests larger than the bounded SPI response capacity are rejected
    /// before reaching the EC.  The response count and byte length must match
    /// exactly, preventing a malformed FIFO payload from being consumed.
    ///
    /// # Arguments
    ///
    /// * `max_vectors` - Maximum samples to read, from 0 through
    ///   [`CROS_EC_MAX_MOTION_FIFO_VECTORS`].
    ///
    /// # Returns
    ///
    /// Samples returned by the EC in FIFO order; the batch may contain fewer
    /// than `max_vectors` samples when the FIFO is empty or partially drained.
    pub fn motion_fifo_read(&self, max_vectors: u32) -> Result<CrosEcMotionFifoData, CrosEcError> {
        if max_vectors > CROS_EC_MAX_MOTION_FIFO_VECTORS {
            return Err(CrosEcError::InvalidArgument);
        }
        let response = self.motion_sense_command(
            MOTION_SENSE_VERSION_FIFO,
            MOTION_SENSE_COMMAND_FIFO_READ,
            &max_vectors.to_le_bytes(),
        )?;
        parse_motion_fifo_data(&response, max_vectors)
    }

    fn dispatch_event(&self, event_type: u8, data: &[u8]) -> bool {
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
        let mut consumed = true;
        for listener in listeners {
            if !self.active.load(Ordering::Acquire) {
                break;
            }
            consumed &= listener.on_cros_ec_event(event_type, data);
        }
        consumed
    }

    fn drain_events(&self) -> Result<(), CrosEcError> {
        for _ in 0..MAX_EVENTS_PER_DRAIN {
            let response = match self.command(COMMAND_GET_NEXT_EVENT, GET_NEXT_EVENT_VERSION, &[]) {
                Ok(response) => response,
                Err(CrosEcError::EcResult(EC_RESULT_UNAVAILABLE)) => return Ok(()),
                Err(error) => return Err(error),
            };
            let event = parse_mkbp_event(&response)?;
            if !self.dispatch_event(event.event_type, event.data) {
                return Err(CrosEcError::EventListenerFailed);
            }
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

    #[test]
    fn converts_display_pwm_percentages_without_overflow() {
        assert_eq!(pwm_duty_from_percent(0), Ok(0));
        assert_eq!(pwm_duty_from_percent(50), Ok(32_768));
        assert_eq!(pwm_duty_from_percent(100), Ok(65_535));
        assert_eq!(
            pwm_duty_from_percent(101),
            Err(CrosEcError::InvalidArgument)
        );
        for percent in 0..=100 {
            let duty = pwm_duty_from_percent(percent).unwrap();
            assert_eq!(pwm_percent_from_duty(duty), percent);
        }
    }

    #[test]
    fn parses_exact_feature_words_and_motion_feature_positions() {
        let response = [
            1 << EC_FEATURE_MOTION_SENSE,
            0,
            0,
            1 << (EC_FEATURE_MOTION_SENSE_FIFO - 24),
            1 << (EC_FEATURE_MOTION_SENSE_TIGHT_TIMESTAMPS - 32),
            0,
            0,
            0,
        ];
        let features = parse_features(&response).unwrap();
        assert!(features.supports_motion_sense());
        assert!(features.supports_motion_sense_fifo());
        assert!(features.supports_motion_sense_tight_timestamps());
        assert!(!features.has_feature(64));
        assert_eq!(
            parse_features(&response[..7]),
            Err(CrosEcError::InvalidResponse)
        );
    }

    #[test]
    fn parses_aligned_mkbp_motion_fifo_snapshot() {
        let event = [
            0xaa, 0xbb, 0xcc, // alignment bytes are not protocol data
            0x00, 0x02, // size = 512
            0x07, 0x00, // count = 7
            0x78, 0x56, 0x34, 0x12, // timestamp
            0x00, 0x00, // no losses in the compact event
        ];
        assert_eq!(
            parse_motion_fifo_event(&event),
            Ok(CrosEcMotionFifoInfo {
                size: 512,
                count: 7,
                timestamp_us: 0x1234_5678,
                total_lost: 0,
                lost_per_sensor: Vec::new(),
            })
        );
        assert_eq!(
            parse_motion_fifo_event(&event[..event.len() - 1]),
            Err(CrosEcError::InvalidResponse)
        );
    }

    #[test]
    fn encodes_full_sized_motion_requests() {
        assert_eq!(
            build_motion_sense_payload(MOTION_SENSE_COMMAND_DUMP, &[0]),
            Ok([
                MOTION_SENSE_COMMAND_DUMP,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ])
        );
        assert_eq!(
            build_motion_sense_payload(
                MOTION_SENSE_COMMAND_SENSOR_ODR,
                &[7, 0, 0, 0, 0xff, 0xff, 0xff, 0xff],
            ),
            Ok([
                MOTION_SENSE_COMMAND_SENSOR_ODR,
                7,
                0,
                0,
                0,
                0xff,
                0xff,
                0xff,
                0xff,
                0,
                0,
                0,
                0,
            ])
        );
        assert_eq!(
            build_motion_sense_payload(MOTION_SENSE_COMMAND_DATA, &[0; 13]),
            Err(CrosEcError::InvalidArgument)
        );
    }

    #[test]
    fn parses_motion_summary_info_and_sensor_data_exactly() {
        assert_eq!(
            parse_motion_sensor_summary(&[0xa5, 2]),
            Ok(CrosEcMotionSensorSummary {
                module_flags: 0xa5,
                sensor_count: 2,
            })
        );
        assert_eq!(
            parse_motion_sensor_summary(&[0xa5]),
            Err(CrosEcError::InvalidResponse)
        );
        assert_eq!(
            parse_motion_sensor_info_v1(&[0, 1, 24]),
            Ok(CrosEcMotionSensorInfo {
                sensor_type: 0,
                location: 1,
                chip: 24,
                min_frequency_millihz: None,
                max_frequency_millihz: None,
                fifo_max_event_count: None,
            })
        );
        assert_eq!(
            parse_motion_sensor_info_v3(&[
                0, 1, 24, 0, 0x10, 0, 0, 0, 0x20, 0, 0, 0, 0x30, 0, 0, 0,
            ]),
            Ok(CrosEcMotionSensorInfo {
                sensor_type: 0,
                location: 1,
                chip: 24,
                min_frequency_millihz: Some(16),
                max_frequency_millihz: Some(32),
                fifo_max_event_count: Some(48),
            })
        );
        assert_eq!(
            parse_motion_sensor_info_v3(&[0; 15]),
            Err(CrosEcError::InvalidResponse)
        );
        assert_eq!(
            parse_motion_sensor_data(&[0x80, 3, 0xfe, 0xff, 2, 0, 0, 0x80]),
            Ok(CrosEcMotionSensorData {
                flags: 0x80,
                sensor_num: 3,
                x: -2,
                y: 2,
                z: i16::MIN,
            })
        );
        let timestamp = parse_motion_sensor_data(&[
            CROS_EC_MOTION_SENSOR_FLAG_TIMESTAMP,
            3,
            0,
            0,
            0x78,
            0x56,
            0x34,
            0x12,
        ])
        .unwrap();
        assert_eq!(timestamp.timestamp_us(), Some(0x1234_5678));
        assert!(!timestamp.is_vector_sample());
        assert!(
            CrosEcMotionSensorData {
                flags: CROS_EC_MOTION_SENSOR_FLAG_WAKEUP,
                sensor_num: 0,
                x: 0,
                y: 0,
                z: 0,
            }
            .is_vector_sample()
        );
        assert_eq!(
            parse_motion_sensor_data(&[0; 7]),
            Err(CrosEcError::InvalidResponse)
        );
    }

    #[test]
    fn parses_motion_fifo_with_strict_count_and_lengths() {
        assert_eq!(CROS_EC_MAX_MOTION_FIFO_VECTORS, 256);
        let info = [0x40, 0, 3, 0, 1, 0, 0, 0, 9, 0, 2, 0, 4, 0];
        assert_eq!(
            parse_motion_fifo_info(&info, 2),
            Ok(CrosEcMotionFifoInfo {
                size: 64,
                count: 3,
                timestamp_us: 1,
                total_lost: 9,
                lost_per_sensor: vec![2, 4],
            })
        );
        assert_eq!(
            parse_motion_fifo_info(&info[..13], 2),
            Err(CrosEcError::InvalidResponse)
        );

        let data = [
            2, 0, 0, 0, 0, 1, 1, 0, 2, 0, 3, 0, 0, 2, 0xfe, 0xff, 0, 0, 0, 0,
        ];
        assert_eq!(
            parse_motion_fifo_data(&data, 2),
            Ok(CrosEcMotionFifoData {
                samples: vec![
                    CrosEcMotionSensorData {
                        flags: 0,
                        sensor_num: 1,
                        x: 1,
                        y: 2,
                        z: 3,
                    },
                    CrosEcMotionSensorData {
                        flags: 0,
                        sensor_num: 2,
                        x: -2,
                        y: 0,
                        z: 0,
                    },
                ],
            })
        );
        assert_eq!(
            parse_motion_fifo_data(&data, 1),
            Err(CrosEcError::InvalidResponse)
        );
        assert_eq!(
            parse_motion_fifo_data(&data[..19], 2),
            Err(CrosEcError::InvalidResponse)
        );
    }

    #[test]
    fn parses_exact_motion_parameter_and_fifo_interrupt_responses() {
        assert_eq!(parse_motion_i32_response(&[0xff, 0xff, 0xff, 0xff]), Ok(-1));
        assert_eq!(
            parse_motion_i32_response(&[0; 3]),
            Err(CrosEcError::InvalidResponse)
        );
        assert_eq!(
            parse_motion_fifo_interrupt_enabled(&[0, 0, 0, 0]),
            Ok(false)
        );
        assert_eq!(parse_motion_fifo_interrupt_enabled(&[1, 0, 0, 0]), Ok(true));
        assert_eq!(
            parse_motion_fifo_interrupt_enabled(&[2, 0, 0, 0]),
            Err(CrosEcError::InvalidResponse)
        );
    }
}
