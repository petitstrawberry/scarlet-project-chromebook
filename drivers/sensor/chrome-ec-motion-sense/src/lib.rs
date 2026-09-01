// SPDX-License-Identifier: GPL-2.0-only

#![no_std]
#![cfg_attr(not(target_os = "none"), allow(dead_code))]

//! Chrome EC motion-sense devices and MKBP FIFO delivery.
//!
//! Sensors are discovered through Chrome EC host commands. FIFO-capable ECs
//! are drained only from `EC_MKBP_EVENT_SENSOR_FIFO` callbacks; firmware
//! without FIFO support gets one direct sample at probe and is never polled.

extern crate alloc;

use alloc::{vec, vec::Vec};

#[cfg(target_os = "none")]
use alloc::sync::Arc;
#[cfg(target_os = "none")]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "none")]
use scarlet::{
    device::{
        Device,
        manager::DeviceManager,
        sensor::{
            SENSOR_EVENT_FLAG_FLUSH, SENSOR_EVENT_FLAG_TIMESTAMP_APPROXIMATE,
            SENSOR_EVENT_FLAG_WAKEUP, SensorDevice, SensorInfo, SensorLocation, SensorType,
        },
    },
    early_println,
    sync::{IrqSpinLock, Mutex},
    time,
};
#[cfg(target_os = "none")]
use scarlet_driver_cros_ec_spi::{
    CROS_EC_MAX_MOTION_FIFO_VECTORS, CROS_EC_MOTION_SENSOR_FLAG_WAKEUP, CrosEcEventListener,
    CrosEcMotionFifoInfo, CrosEcSpi, get_primary_cros_ec_spi, parse_motion_fifo_event,
};

const EC_MKBP_EVENT_SENSOR_FIFO: u8 = 2;
const MAX_SENSOR_COUNT: u8 = 32;

const FLAG_FLUSH: u8 = 1 << 0;
const FLAG_TIMESTAMP: u8 = 1 << 1;
const FLAG_WAKEUP: u8 = 1 << 2;
const FLAG_TABLET_MODE: u8 = 1 << 3;
const FLAG_ODR: u8 = 1 << 4;
const FLAG_BYPASS_FIFO: u8 = 1 << 7;
const KNOWN_FLAGS: u8 =
    FLAG_FLUSH | FLAG_TIMESTAMP | FLAG_WAKEUP | FLAG_TABLET_MODE | FLAG_ODR | FLAG_BYPASS_FIFO;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Vector {
    flags: u8,
    sensor: u8,
    values: [i32; 3],
}

impl Vector {
    fn timestamp_us(self) -> u32 {
        (self.values[1] as u16 as u32) | ((self.values[2] as u16 as u32) << 16)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VectorKind {
    Sample,
    Timestamp,
    Flush,
    Odr,
    TabletMode,
    Invalid,
}

fn classify(flags: u8) -> VectorKind {
    if flags & !KNOWN_FLAGS != 0 {
        return VectorKind::Invalid;
    }
    let annotations = flags & (FLAG_WAKEUP | FLAG_BYPASS_FIFO);
    let metadata = flags & !(FLAG_WAKEUP | FLAG_BYPASS_FIFO);
    if metadata != 0 && annotations != 0 {
        return VectorKind::Invalid;
    }
    match metadata {
        0 => VectorKind::Sample,
        FLAG_TIMESTAMP => VectorKind::Timestamp,
        FLAG_FLUSH => VectorKind::Flush,
        FLAG_ODR => VectorKind::Odr,
        FLAG_TABLET_MODE => VectorKind::TabletMode,
        _ => VectorKind::Invalid,
    }
}

/// Return the signed shortest delta between wrapping EC microsecond counters.
fn wrapping_delta_us(sample_us: u32, anchor_us: u32) -> i64 {
    i64::from(sample_us.wrapping_sub(anchor_us) as i32)
}

fn anchored_timestamp_ns(arrival_ns: u64, anchor_us: u32, sample_us: u32) -> u64 {
    let delta_ns = wrapping_delta_us(sample_us, anchor_us).saturating_mul(1_000);
    let timestamp = if delta_ns >= 0 {
        arrival_ns.saturating_add(delta_ns as u64)
    } else {
        arrival_ns.saturating_sub(delta_ns.unsigned_abs())
    };
    timestamp.min(arrival_ns)
}

fn fifo_remaining_after_progress(remaining: u32, requested: u32, progress: u32) -> Result<u32, ()> {
    if progress == 0 || progress > requested || progress > remaining {
        return Err(());
    }
    Ok(remaining - progress)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSample {
    values: [i32; 3],
    flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputEvent {
    sensor: usize,
    timestamp_ns: u64,
    values: [i32; 3],
    flags: u32,
    lost: u32,
    flush: bool,
}

#[derive(Default)]
struct SensorState {
    odr_millihz: u32,
    last_timestamp_ns: Option<u64>,
    pending_lost: u32,
}

struct Processor {
    states: Vec<SensorState>,
    present: Vec<bool>,
    tight_timestamps: bool,
    next_timestamp_ns: Vec<Option<u64>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessError {
    LostCountMismatch,
    LostSensorCountMismatch,
}

struct ProcessResult {
    events: Vec<OutputEvent>,
    discarded: u32,
    unattributed_lost: u32,
}

impl Processor {
    fn new(odrs: &[u32], present: &[bool], tight_timestamps: bool) -> Self {
        let states = odrs
            .iter()
            .copied()
            .map(|odr_millihz| SensorState {
                odr_millihz,
                ..SensorState::default()
            })
            .collect();
        Self {
            states,
            present: present.to_vec(),
            tight_timestamps,
            next_timestamp_ns: vec![None; odrs.len()],
        }
    }

    fn normalize_timestamp(&mut self, sensor: usize, timestamp_ns: u64) -> u64 {
        let state = &mut self.states[sensor];
        let timestamp_ns = state
            .last_timestamp_ns
            .map_or(timestamp_ns, |previous| previous.max(timestamp_ns));
        state.last_timestamp_ns = Some(timestamp_ns);
        timestamp_ns
    }

    fn take_lost(&mut self, sensor: usize) -> u32 {
        core::mem::take(&mut self.states[sensor].pending_lost)
    }

    fn emit_pending(
        &mut self,
        pending: &mut [Vec<PendingSample>],
        sensor: usize,
        marker_ns: u64,
        approximate: bool,
        force_approximate: bool,
        events: &mut Vec<OutputEvent>,
    ) {
        let count = pending[sensor].len();
        if count == 0 {
            if !approximate {
                let _ = self.normalize_timestamp(sensor, marker_ns);
            }
            return;
        }

        let previous = self.states[sensor].last_timestamp_ns;
        let period_ns = if self.states[sensor].odr_millihz == 0 {
            0
        } else {
            1_000_000_000_000u64 / u64::from(self.states[sensor].odr_millihz)
        };
        let drained = core::mem::take(&mut pending[sensor]);
        for (index, sample) in drained.into_iter().enumerate() {
            let timestamp_ns = if approximate {
                marker_ns
            } else if let Some(previous) = previous {
                let span = marker_ns.saturating_sub(previous);
                let numerator = span.saturating_mul((index + 1) as u64);
                previous.saturating_add(numerator / count as u64)
            } else {
                let samples_after = count.saturating_sub(index + 1) as u64;
                marker_ns.saturating_sub(period_ns.saturating_mul(samples_after))
            };
            let timestamp_ns = self.normalize_timestamp(sensor, timestamp_ns);
            events.push(OutputEvent {
                sensor,
                timestamp_ns,
                values: sample.values,
                flags: sample.flags
                    | if approximate || force_approximate {
                        FLAG_TIMESTAMP_APPROXIMATE_EVENT
                    } else {
                        0
                    },
                lost: self.take_lost(sensor),
                flush: false,
            });
        }
    }

    fn process(
        &mut self,
        arrival_ns: u64,
        anchor_us: u32,
        total_lost: u16,
        lost_per_sensor: &[u16],
        vectors: &[Vector],
    ) -> Result<ProcessResult, ProcessError> {
        if lost_per_sensor.len() != self.states.len() {
            return Err(ProcessError::LostSensorCountMismatch);
        }
        let lost_sum = lost_per_sensor
            .iter()
            .fold(0u32, |sum, lost| sum.saturating_add(u32::from(*lost)));
        // The EC's total also counts timestamp markers evicted from the FIFO,
        // while its per-sensor counters count only data vectors.  A total
        // smaller than the per-sensor sum is impossible; a larger one is valid.
        if lost_sum > u32::from(total_lost) {
            return Err(ProcessError::LostCountMismatch);
        }
        let unattributed_lost = u32::from(total_lost) - lost_sum;
        if unattributed_lost != 0 {
            for sensor in 0..self.states.len() {
                self.states[sensor].last_timestamp_ns = None;
                self.next_timestamp_ns[sensor] = None;
            }
        }
        for (sensor, lost) in lost_per_sensor.iter().copied().enumerate() {
            if lost != 0 {
                self.states[sensor].pending_lost = self.states[sensor]
                    .pending_lost
                    .saturating_add(u32::from(lost));
                self.states[sensor].last_timestamp_ns = None;
                self.next_timestamp_ns[sensor] = None;
            }
        }

        let mut pending: Vec<Vec<PendingSample>> =
            (0..self.states.len()).map(|_| Vec::new()).collect();
        let mut events = Vec::with_capacity(vectors.len());
        let mut discarded = 0u32;
        for vector in vectors.iter().copied() {
            let sensor = usize::from(vector.sensor);
            match classify(vector.flags) {
                VectorKind::Timestamp => {
                    let marker_ns =
                        anchored_timestamp_ns(arrival_ns, anchor_us, vector.timestamp_us());
                    if self.tight_timestamps && sensor < self.states.len() && self.present[sensor] {
                        self.emit_pending(
                            &mut pending,
                            sensor,
                            arrival_ns,
                            true,
                            unattributed_lost != 0,
                            &mut events,
                        );
                        self.next_timestamp_ns[sensor] = Some(marker_ns);
                    } else {
                        for index in 0..self.states.len() {
                            if self.present[index] {
                                self.emit_pending(
                                    &mut pending,
                                    index,
                                    marker_ns,
                                    false,
                                    unattributed_lost != 0,
                                    &mut events,
                                );
                            }
                        }
                    }
                }
                VectorKind::Sample => {
                    if sensor >= self.states.len() || !self.present[sensor] {
                        discarded = discarded.saturating_add(1);
                        continue;
                    }
                    let sample = PendingSample {
                        values: vector.values,
                        flags: if vector.flags & FLAG_WAKEUP != 0 {
                            FLAG_WAKEUP_EVENT
                        } else {
                            0
                        },
                    };
                    if self.tight_timestamps {
                        if let Some(timestamp_ns) = self.next_timestamp_ns[sensor].take() {
                            let timestamp_ns = self.normalize_timestamp(sensor, timestamp_ns);
                            events.push(OutputEvent {
                                sensor,
                                timestamp_ns,
                                values: sample.values,
                                flags: sample.flags
                                    | if unattributed_lost != 0 {
                                        FLAG_TIMESTAMP_APPROXIMATE_EVENT
                                    } else {
                                        0
                                    },
                                lost: self.take_lost(sensor),
                                flush: false,
                            });
                        } else {
                            pending[sensor].push(sample);
                        }
                    } else {
                        pending[sensor].push(sample);
                    }
                }
                VectorKind::Flush => {
                    if sensor >= self.states.len() || !self.present[sensor] {
                        discarded = discarded.saturating_add(1);
                        continue;
                    }
                    let marker_ns =
                        anchored_timestamp_ns(arrival_ns, anchor_us, vector.timestamp_us());
                    self.emit_pending(
                        &mut pending,
                        sensor,
                        marker_ns,
                        false,
                        unattributed_lost != 0,
                        &mut events,
                    );
                    let timestamp_ns = self.normalize_timestamp(sensor, marker_ns);
                    events.push(OutputEvent {
                        sensor,
                        timestamp_ns,
                        values: [0; 3],
                        flags: (if vector.flags & FLAG_WAKEUP != 0 {
                            FLAG_WAKEUP_EVENT
                        } else {
                            0
                        }) | if unattributed_lost != 0 {
                            FLAG_TIMESTAMP_APPROXIMATE_EVENT
                        } else {
                            0
                        },
                        lost: self.take_lost(sensor),
                        flush: true,
                    });
                }
                VectorKind::Odr => {
                    if sensor >= self.states.len() || !self.present[sensor] {
                        discarded = discarded.saturating_add(1);
                        continue;
                    }
                    let marker_ns =
                        anchored_timestamp_ns(arrival_ns, anchor_us, vector.timestamp_us());
                    self.emit_pending(
                        &mut pending,
                        sensor,
                        marker_ns,
                        false,
                        unattributed_lost != 0,
                        &mut events,
                    );
                    self.states[sensor].last_timestamp_ns = None;
                    self.next_timestamp_ns[sensor] = None;
                }
                VectorKind::TabletMode => {}
                VectorKind::Invalid => discarded = discarded.saturating_add(1),
            }
        }
        for sensor in 0..self.states.len() {
            if self.present[sensor] {
                self.emit_pending(
                    &mut pending,
                    sensor,
                    arrival_ns,
                    true,
                    unattributed_lost != 0,
                    &mut events,
                );
            }
        }
        Ok(ProcessResult {
            events,
            discarded,
            unattributed_lost,
        })
    }
}

#[cfg(target_os = "none")]
const FLAG_WAKEUP_EVENT: u32 = SENSOR_EVENT_FLAG_WAKEUP;
#[cfg(not(target_os = "none"))]
const FLAG_WAKEUP_EVENT: u32 = 1 << 2;
#[cfg(target_os = "none")]
const FLAG_TIMESTAMP_APPROXIMATE_EVENT: u32 = SENSOR_EVENT_FLAG_TIMESTAMP_APPROXIMATE;
#[cfg(not(target_os = "none"))]
const FLAG_TIMESTAMP_APPROXIMATE_EVENT: u32 = 1 << 3;

#[cfg(target_os = "none")]
mod runtime {
    use super::*;
    use alloc::vec;

    struct ChromeEcMotionHub {
        ec: Arc<CrosEcSpi>,
        sensors: Vec<Option<Arc<SensorDevice>>>,
        processor: Mutex<Processor>,
        sensor_count: u8,
        consecutive_errors: AtomicU64,
    }

    impl ChromeEcMotionHub {
        fn note_error(&self, reason: &'static str) {
            let count = self.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
            if count.is_power_of_two() {
                early_println!(
                    "[chrome-ec-motion-sense] FIFO error count={} reason={}",
                    count,
                    reason
                );
            }
        }

        fn note_success(&self) {
            self.consecutive_errors.store(0, Ordering::Relaxed);
        }

        fn push_outputs(&self, result: ProcessResult) -> Result<(), &'static str> {
            let unattributed_lost = result.unattributed_lost;
            let mut had_error = result.discarded != 0 || unattributed_lost != 0;
            for output in result.events {
                let Some(Some(device)) = self.sensors.get(output.sensor) else {
                    had_error = true;
                    continue;
                };
                let result = if output.flush {
                    device.push_event_at(
                        output.timestamp_ns,
                        [0; 3],
                        SENSOR_EVENT_FLAG_FLUSH | output.flags,
                        output.lost,
                    )
                } else {
                    device.push_sample_at(
                        output.timestamp_ns,
                        output.values,
                        output.flags,
                        output.lost,
                    )
                };
                if result.is_err() {
                    had_error = true;
                }
            }
            if had_error {
                if unattributed_lost != 0 {
                    Err("unattributed FIFO timestamp loss")
                } else {
                    Err("discarded invalid FIFO event")
                }
            } else {
                Ok(())
            }
        }

        fn event_fifo_info(&self, data: &[u8]) -> Result<CrosEcMotionFifoInfo, &'static str> {
            let mut info =
                parse_motion_fifo_event(data).map_err(|_| "malformed MKBP FIFO event")?;
            if info.total_lost != 0 {
                // The compact MKBP event omits the flexible per-sensor loss
                // array. Match Linux and ask for the full structure only when
                // the event reports that something was lost.
                return self
                    .ec
                    .motion_fifo_info_for_sensor_count(self.sensor_count)
                    .map_err(|_| "FIFO_INFO command failed");
            }
            info.lost_per_sensor = vec![0; usize::from(self.sensor_count)];
            Ok(info)
        }

        fn drain_fifo(&self, arrival_ns: u64, data: &[u8]) -> Result<(), &'static str> {
            let info = self.event_fifo_info(data)?;
            validate_fifo_info(&info, usize::from(self.sensor_count))?;
            let mut remaining = u32::from(info.count);
            let maximum = u32::from(info.size);
            let mut vectors = Vec::with_capacity(usize::from(info.count));
            while remaining != 0 {
                if vectors.len() as u32 >= maximum {
                    return Err("FIFO drain exceeded advertised capacity");
                }
                let requested = remaining.min(CROS_EC_MAX_MOTION_FIFO_VECTORS);
                let batch = self
                    .ec
                    .motion_fifo_read(requested)
                    .map_err(|_| "FIFO_READ command failed")?;
                let progress =
                    u32::try_from(batch.samples.len()).map_err(|_| "FIFO_READ count overflow")?;
                remaining = fifo_remaining_after_progress(remaining, requested, progress)
                    .map_err(|_| "FIFO_READ made invalid progress")?;
                for sample in batch.samples {
                    vectors.push(Vector {
                        flags: sample.flags,
                        sensor: sample.sensor_num,
                        values: [
                            i32::from(sample.x),
                            i32::from(sample.y),
                            i32::from(sample.z),
                        ],
                    });
                }
            }
            let result = self
                .processor
                .lock()
                .process(
                    arrival_ns,
                    info.timestamp_us,
                    info.total_lost,
                    &info.lost_per_sensor,
                    &vectors,
                )
                .map_err(|_| "FIFO loss counters are inconsistent")?;
            self.push_outputs(result)
        }
    }

    impl CrosEcEventListener for ChromeEcMotionHub {
        fn on_cros_ec_event(&self, event_type: u8, data: &[u8]) -> bool {
            if event_type != EC_MKBP_EVENT_SENSOR_FIFO {
                return true;
            }
            let arrival_ns = time::current_time_ns();
            match self.drain_fifo(arrival_ns, data) {
                Ok(()) => {
                    self.note_success();
                    true
                }
                Err(reason) => {
                    self.note_error(reason);
                    false
                }
            }
        }
    }

    fn validate_fifo_info(
        info: &CrosEcMotionFifoInfo,
        sensor_count: usize,
    ) -> Result<(), &'static str> {
        if info.count > info.size {
            return Err("FIFO_INFO count exceeds capacity");
        }
        if info.lost_per_sensor.len() != sensor_count {
            return Err("FIFO_INFO sensor loss count mismatch");
        }
        let sum = info
            .lost_per_sensor
            .iter()
            .fold(0u32, |sum, lost| sum.saturating_add(u32::from(*lost)));
        if sum > u32::from(info.total_lost) {
            return Err("FIFO_INFO total loss mismatch");
        }
        Ok(())
    }

    static HUB: IrqSpinLock<Option<Arc<ChromeEcMotionHub>>> = IrqSpinLock::new(None);

    fn map_sensor_type(value: u8) -> Option<SensorType> {
        match value {
            0 => Some(SensorType::Accelerometer),
            1 => Some(SensorType::Gyroscope),
            2 => Some(SensorType::Magnetometer),
            _ => None,
        }
    }

    fn map_location(value: u8) -> SensorLocation {
        match value {
            0 => SensorLocation::Base,
            1 => SensorLocation::Lid,
            2 => SensorLocation::Camera,
            _ => SensorLocation::Unknown,
        }
    }

    fn initialize() {
        let Some(ec) = get_primary_cros_ec_spi() else {
            early_println!("[chrome-ec-motion-sense] primary Chrome EC unavailable");
            return;
        };
        let features = match ec.features() {
            Ok(features) if features.supports_motion_sense() => features,
            Ok(_) => {
                early_println!("[chrome-ec-motion-sense] EC has no motion-sense feature");
                return;
            }
            Err(_) => {
                early_println!("[chrome-ec-motion-sense] GET_FEATURES failed");
                return;
            }
        };
        let summary = match ec.motion_sensor_summary() {
            Ok(summary) if summary.sensor_count <= MAX_SENSOR_COUNT => summary,
            Ok(summary) => {
                early_println!(
                    "[chrome-ec-motion-sense] invalid sensor count {}",
                    summary.sensor_count
                );
                return;
            }
            Err(_) => {
                early_println!("[chrome-ec-motion-sense] motion DUMP failed");
                return;
            }
        };

        let count = usize::from(summary.sensor_count);
        let mut sensors = vec![None; count];
        let mut present = vec![false; count];
        let mut odrs = vec![0u32; count];
        for sensor_num in 0..summary.sensor_count {
            let info = match ec.motion_sensor_info(sensor_num) {
                Ok(info) => info,
                Err(_) => {
                    early_println!(
                        "[chrome-ec-motion-sense] sensor {} INFO failed; skipped",
                        sensor_num
                    );
                    continue;
                }
            };
            // Query every enumerated sensor completely before deciding whether
            // its type is exportable, so EC discovery failures are explicit.
            let range_result = ec.motion_sensor_range(sensor_num);
            let odr_result = ec.motion_sensor_odr_millihz(sensor_num);
            let range = match range_result {
                Ok(range) if range > 0 => range as u32,
                _ => {
                    early_println!(
                        "[chrome-ec-motion-sense] sensor {} RANGE failed; skipped",
                        sensor_num
                    );
                    continue;
                }
            };
            let odr = match odr_result {
                Ok(odr) if odr >= 0 => odr as u32,
                _ => {
                    early_println!(
                        "[chrome-ec-motion-sense] sensor {} ODR failed; skipped",
                        sensor_num
                    );
                    continue;
                }
            };
            let Some(sensor_type) = map_sensor_type(info.sensor_type) else {
                early_println!(
                    "[chrome-ec-motion-sense] sensor {} type {} unsupported; skipped",
                    sensor_num,
                    info.sensor_type
                );
                continue;
            };
            let min_frequency = info.min_frequency_millihz.unwrap_or(0);
            let max_frequency = info.max_frequency_millihz.unwrap_or(odr.max(1));
            let metadata = match SensorInfo::new(
                sensor_type,
                map_location(info.location),
                (u32::from(info.chip) << 8) | u32::from(sensor_num),
                3,
                i32::from(i16::MIN),
                i32::from(i16::MAX),
                range,
                16,
                min_frequency,
                max_frequency,
                odr,
                info.fifo_max_event_count.unwrap_or(0),
            ) {
                Ok(metadata) => metadata,
                Err(error) => {
                    early_println!(
                        "[chrome-ec-motion-sense] sensor {} metadata invalid: {}; skipped",
                        sensor_num,
                        error
                    );
                    continue;
                }
            };
            let device = match SensorDevice::new(metadata) {
                Ok(device) => Arc::new(device),
                Err(error) => {
                    early_println!(
                        "[chrome-ec-motion-sense] sensor {} device failed: {}; skipped",
                        sensor_num,
                        error
                    );
                    continue;
                }
            };
            let name = device.get_name().into();
            let registered: Arc<dyn Device> = device.clone();
            DeviceManager::get_manager().register_device_with_name(name, registered);
            early_println!(
                "[chrome-ec-motion-sense] registered {} ec_sensor={} type={:?} location={:?} odr={}mHz fifo={}",
                device.get_name(),
                sensor_num,
                sensor_type,
                map_location(info.location),
                odr,
                info.fifo_max_event_count.unwrap_or(0)
            );
            present[usize::from(sensor_num)] = true;
            odrs[usize::from(sensor_num)] = odr;
            sensors[usize::from(sensor_num)] = Some(device);
        }

        let hub = Arc::new(ChromeEcMotionHub {
            ec: ec.clone(),
            sensors,
            processor: Mutex::new(Processor::new(
                &odrs,
                &present,
                features.supports_motion_sense_tight_timestamps(),
            )),
            sensor_count: summary.sensor_count,
            consecutive_errors: AtomicU64::new(0),
        });
        // Devices and the listener allocation must have strong owners before
        // the EC is allowed to generate the first FIFO notification.
        *HUB.lock() = Some(hub.clone());

        if features.supports_motion_sense_fifo() {
            let listener: Arc<dyn CrosEcEventListener> = hub.clone();
            let _listener_id = ec.register_event_listener(Arc::downgrade(&listener));
            match ec.set_motion_fifo_interrupt_enabled(true) {
                Ok(true) => early_println!(
                    "[chrome-ec-motion-sense] FIFO IRQ enabled sensors={} tight_timestamps={}",
                    summary.sensor_count,
                    features.supports_motion_sense_tight_timestamps()
                ),
                Ok(false) => hub.note_error("EC did not enable motion FIFO interrupt"),
                Err(_) => hub.note_error("FIFO interrupt enable command failed"),
            }
        } else {
            early_println!(
                "[chrome-ec-motion-sense] FIFO unsupported; taking one direct sample, no polling"
            );
            let now_ns = time::current_time_ns();
            for sensor_num in 0..summary.sensor_count {
                let Some(Some(device)) = hub.sensors.get(usize::from(sensor_num)) else {
                    continue;
                };
                match ec.motion_sensor_data(sensor_num) {
                    Ok(sample) if sample.sensor_num == sensor_num && sample.is_vector_sample() => {
                        let flags = if sample.flags & CROS_EC_MOTION_SENSOR_FLAG_WAKEUP != 0 {
                            SENSOR_EVENT_FLAG_WAKEUP
                        } else {
                            0
                        };
                        let _ = device.push_sample_at(
                            now_ns,
                            [
                                i32::from(sample.x),
                                i32::from(sample.y),
                                i32::from(sample.z),
                            ],
                            flags | SENSOR_EVENT_FLAG_TIMESTAMP_APPROXIMATE,
                            0,
                        );
                    }
                    Ok(_) => hub.note_error("direct DATA sample is malformed"),
                    Err(_) => hub.note_error("direct DATA sample failed"),
                }
            }
        }
    }

    scarlet::late_initcall!(initialize);
}

#[used]
static SCARLET_DRIVER_CHROME_EC_MOTION_SENSE_ANCHOR: fn() = force_link;

/// Keep the external motion-sense driver linked into Scarlet module builds.
#[inline(never)]
pub fn force_link() {}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn sample(sensor: u8, value: i32) -> Vector {
        Vector {
            flags: 0,
            sensor,
            values: [value, value + 1, value + 2],
        }
    }

    fn timestamp(timestamp_us: u32) -> Vector {
        timestamp_for(0, timestamp_us)
    }

    fn timestamp_for(sensor: u8, timestamp_us: u32) -> Vector {
        Vector {
            flags: FLAG_TIMESTAMP,
            sensor,
            values: [
                0,
                timestamp_us as u16 as i32,
                (timestamp_us >> 16) as u16 as i32,
            ],
        }
    }

    #[test]
    fn signed_u32_wrap_delta_and_future_clamp() {
        assert_eq!(wrapping_delta_us(2, u32::MAX - 2), 5);
        assert_eq!(wrapping_delta_us(u32::MAX - 2, 2), -5);
        assert_eq!(anchored_timestamp_ns(1_000_000, 100, 90), 990_000);
        assert_eq!(anchored_timestamp_ns(1_000_000, 100, 110), 1_000_000);
    }

    #[test]
    fn timestamp_distribution_is_monotonic() {
        let mut processor = Processor::new(&[100_000], &[true], false);
        let result = processor
            .process(
                2_000_000_000,
                2_000_000,
                0,
                &[0],
                &[sample(0, 1), sample(0, 2), timestamp(2_000_000)],
            )
            .unwrap();
        let times: Vec<u64> = result
            .events
            .iter()
            .map(|event| event.timestamp_ns)
            .collect();
        assert_eq!(times, vec![1_990_000_000, 2_000_000_000]);
        assert!(times.windows(2).all(|window| window[0] <= window[1]));
        assert_eq!(result.events[0].flags & FLAG_TIMESTAMP_APPROXIMATE_EVENT, 0);
    }

    #[test]
    fn interleaved_sensors_are_distributed_independently() {
        let mut processor = Processor::new(&[50_000, 100_000], &[true, true], false);
        let result = processor
            .process(
                1_000_000_000,
                10_000,
                0,
                &[0, 0],
                &[
                    sample(0, 1),
                    sample(1, 10),
                    sample(1, 20),
                    timestamp(10_000),
                ],
            )
            .unwrap();
        assert_eq!(result.events.len(), 3);
        assert_eq!(result.events[0].sensor, 0);
        assert_eq!(result.events[0].timestamp_ns, 1_000_000_000);
        assert_eq!(result.events[1].sensor, 1);
        assert_eq!(result.events[1].timestamp_ns, 990_000_000);
        assert_eq!(result.events[2].timestamp_ns, 1_000_000_000);
    }

    #[test]
    fn tight_timestamp_markers_apply_to_following_interleaved_samples() {
        let mut processor = Processor::new(&[50_000, 100_000], &[true, true], true);
        let result = processor
            .process(
                1_000_000_000,
                10_000,
                0,
                &[0, 0],
                &[
                    timestamp_for(1, 9_990),
                    sample(1, 10),
                    timestamp_for(0, 9_995),
                    sample(0, 20),
                ],
            )
            .unwrap();
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].sensor, 1);
        assert_eq!(result.events[0].timestamp_ns, 999_990_000);
        assert_eq!(result.events[1].sensor, 0);
        assert_eq!(result.events[1].timestamp_ns, 999_995_000);
        assert!(
            result
                .events
                .iter()
                .all(|event| event.flags & FLAG_TIMESTAMP_APPROXIMATE_EVENT == 0)
        );
    }

    #[test]
    fn metadata_vectors_are_classified_and_flush_is_exported() {
        assert_eq!(classify(FLAG_TIMESTAMP), VectorKind::Timestamp);
        assert_eq!(classify(FLAG_ODR), VectorKind::Odr);
        assert_eq!(classify(FLAG_FLUSH), VectorKind::Flush);
        assert_eq!(classify(FLAG_TABLET_MODE), VectorKind::TabletMode);
        assert_eq!(classify(FLAG_BYPASS_FIFO), VectorKind::Sample);
        assert_eq!(classify(FLAG_FLUSH | FLAG_WAKEUP), VectorKind::Invalid);
        assert_eq!(classify(FLAG_FLUSH | FLAG_ODR), VectorKind::Invalid);

        let mut processor = Processor::new(&[100_000], &[true], false);
        let result = processor
            .process(
                5_000,
                5,
                0,
                &[0],
                &[
                    sample(0, 1),
                    Vector {
                        flags: FLAG_ODR,
                        sensor: 0,
                        values: [0; 3],
                    },
                    Vector {
                        flags: FLAG_FLUSH,
                        sensor: 0,
                        values: [0; 3],
                    },
                ],
            )
            .unwrap();
        assert_eq!(result.events.len(), 2);
        assert!(!result.events[0].flush);
        assert_eq!(result.events[0].flags & FLAG_TIMESTAMP_APPROXIMATE_EVENT, 0);
        assert!(result.events[1].flush);
        assert_eq!(result.events[1].flags & FLAG_WAKEUP_EVENT, 0);
    }

    #[test]
    fn lost_samples_propagate_once_and_reset_interpolation() {
        let mut processor = Processor::new(&[100_000], &[true], false);
        let first = processor.process(100, 0, 3, &[3], &[sample(0, 1)]).unwrap();
        assert_eq!(first.events[0].lost, 3);
        let second = processor.process(200, 0, 0, &[0], &[sample(0, 2)]).unwrap();
        assert_eq!(second.events[0].lost, 0);
        assert_eq!(
            processor.process(300, 0, 0, &[1], &[]).err(),
            Some(ProcessError::LostCountMismatch)
        );
    }

    #[test]
    fn timestamp_marker_loss_is_valid_but_forces_approximation() {
        let mut processor = Processor::new(&[100_000], &[true], false);
        let result = processor.process(100, 0, 3, &[2], &[sample(0, 1)]).unwrap();
        assert_eq!(result.unattributed_lost, 1);
        assert_eq!(result.events[0].lost, 2);
        assert!(result.events[0].flags & FLAG_TIMESTAMP_APPROXIMATE_EVENT != 0);
        assert_eq!(
            processor.process(200, 0, 1, &[2], &[]).err(),
            Some(ProcessError::LostCountMismatch)
        );
    }

    #[test]
    fn malformed_and_out_of_range_vectors_are_bounded_discards() {
        let mut processor = Processor::new(&[100_000], &[true], false);
        let result = processor
            .process(
                100,
                0,
                0,
                &[0],
                &[
                    sample(4, 1),
                    Vector {
                        flags: 0x40,
                        sensor: 0,
                        values: [0; 3],
                    },
                ],
            )
            .unwrap();
        assert_eq!(result.discarded, 2);
        assert!(result.events.is_empty());
    }

    #[test]
    fn zero_progress_is_rejected_by_fifo_bounds_contract() {
        assert_eq!(fifo_remaining_after_progress(4, 4, 0), Err(()));
        assert_eq!(fifo_remaining_after_progress(4, 4, 5), Err(()));
        assert_eq!(fifo_remaining_after_progress(4, 4, 2), Ok(2));
    }
}
