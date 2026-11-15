//! Opt-in CPU service-time windows for tracked Adreno submissions.
//!
//! One fixed-size process-wide accumulator emits at most once per second, on a
//! submission return (no idle flush or per-draw/spike logs). All attempts count,
//! including Busy/rejected work; byte counts describe prepared work, not GPU
//! traffic. No retained command/resource references or per-submission history.
//! commands counts input IR commands; chunks counts packed dispatcher entries
//! (PM4, CPU writes, empty-stream barriers). pm4_chunks/pm4_words and generated
//! upload bytes count successful prepare_chunk results, even if later rejected;
//! compile_attempts also counts failed/retried preparations, whose discarded
//! bytes are unavailable here. Busy retries therefore count their CPU work again.
//!
//! These are analogous to, not identical with, VirGL submission boundaries:
//! materialize_plan interleaves cache materialization and IR normalization;
//! pm4_compile_prepare includes generated-object placement and wire-size
//! preflight, including failed compile attempts before splitting again. split
//! includes retry/chunk bookkeeping. upload_ownership includes buffer lookup,
//! validation and the borrowed-to-owned memcpy. Generated upload conversion and
//! copying happen inside pm4_compile_prepare. pack_chunks builds bindings and
//! moves owned upload vectors, while wire_encode measures the encoder itself.
//! Resource snapshots are resource_retention; there is no VirGL shadow snapshot,
//! events checkpoint, retirement or wait_idle measurement here. Actual coherent
//! upload copies, worker submission and GPU completion are outside this scope.
//!
//! Durations include syscall/lock waits and local cleanup, but exclude logging.
//! Profile drops only after submission helpers (and their GPU/admission locks)
//! return. Stderr output is best effort and occurs outside the profile mutex.

use std::fmt;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Validate,
    QueryLimits,
    MaterializePlan,
    Split,
    Pm4CompilePrepare,
    UploadOwnership,
    ArenaPreparation,
    PackChunks,
    WireEncode,
    ResourceRetention,
    Enqueue,
}

const PHASE_NAMES: [&str; 11] = [
    "validate",
    "query_limits",
    "materialize_plan",
    "split",
    "pm4_compile_prepare",
    "upload_ownership",
    "arena_preparation",
    "pack_chunks",
    "wire_encode",
    "resource_retention",
    "enqueue",
];

#[derive(Clone, Copy)]
pub(crate) enum Count {
    Commands,
    Chunks,
    Pm4Chunks,
    BufferChunks,
    BarrierChunks,
    CompileAttempts,
    Pm4Words,
    WireBytes,
    BufferUploadBytes,
    GeneratedUploadBytes,
}

const COUNT_NAMES: [&str; 10] = [
    "commands",
    "chunks",
    "pm4_chunks",
    "buffer_chunks",
    "barrier_chunks",
    "compile_attempts",
    "pm4_words",
    "wire_bytes",
    "buffer_upload_bytes",
    "generated_upload_bytes",
];

#[derive(Clone, Copy, Default)]
pub(crate) enum Outcome {
    Accepted,
    Busy,
    #[default]
    Rejected,
}

#[derive(Clone, Copy, Default)]
struct Sample {
    total: Duration,
    phases: [Duration; PHASE_NAMES.len()],
    counts: [u64; COUNT_NAMES.len()],
    outcome: Outcome,
}

struct Timing {
    started: Instant,
    stage_started: Instant,
    phase: Phase,
}

impl Timing {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            stage_started: now,
            phase: Phase::Validate,
        }
    }

    fn advance(&mut self, sample: &mut Sample, phase: Phase, now: Instant) {
        // Clamp defensively so a regressed clock cannot double-count later.
        let now = now.max(self.stage_started);
        let elapsed = now.saturating_duration_since(self.stage_started);
        let stage = &mut sample.phases[self.phase as usize];
        *stage = stage.saturating_add(elapsed);
        sample.total = now.saturating_duration_since(self.started);
        self.stage_started = now;
        self.phase = phase;
    }
}

pub(crate) struct Profile {
    timing: Timing,
    sample: Sample,
}

impl Profile {
    pub(crate) fn new(commands: usize) -> Self {
        let mut profile = Self {
            timing: Timing::new(Instant::now()),
            sample: Sample::default(),
        };
        profile.count(Count::Commands, commands);
        profile
    }

    pub(crate) fn enter(&mut self, phase: Phase) {
        self.timing.advance(&mut self.sample, phase, Instant::now());
    }

    pub(crate) fn count(&mut self, counter: Count, amount: usize) {
        let count = &mut self.sample.counts[counter as usize];
        *count = count.saturating_add(amount as u64);
    }

    pub(crate) fn outcome(&mut self, outcome: Outcome) {
        self.sample.outcome = outcome;
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let now = Instant::now();
        self.timing
            .advance(&mut self.sample, self.timing.phase, now);
        static WINDOW: OnceLock<Mutex<Window>> = OnceLock::new();
        let report = {
            let Ok(mut window) = WINDOW
                .get_or_init(|| Mutex::new(Window::new(self.timing.started)))
                .lock()
            else {
                return;
            };
            window.record(self.sample, now)
        };
        if let Some(report) = report {
            let _ = writeln!(std::io::stderr().lock(), "{report}");
        }
    }
}

const PERIOD: Duration = Duration::from_secs(1);

struct Window {
    started: Instant,
    totals: Sample,
    outcomes: [u64; 3],
    max_total: Duration,
}

impl Window {
    fn new(started: Instant) -> Self {
        Self {
            started,
            totals: Sample::default(),
            outcomes: [0; 3],
            max_total: Duration::ZERO,
        }
    }

    fn record(&mut self, sample: Sample, now: Instant) -> Option<Report> {
        let outcome = &mut self.outcomes[sample.outcome as usize];
        *outcome = outcome.saturating_add(1);
        self.totals.total = self.totals.total.saturating_add(sample.total);
        self.max_total = self.max_total.max(sample.total);
        for (sum, duration) in self.totals.phases.iter_mut().zip(sample.phases) {
            *sum = sum.saturating_add(duration);
        }
        for (sum, count) in self.totals.counts.iter_mut().zip(sample.counts) {
            *sum = sum.saturating_add(count);
        }
        let period = now.saturating_duration_since(self.started);
        if period < PERIOD {
            return None;
        }
        let report = Report {
            period,
            totals: self.totals,
            outcomes: self.outcomes,
            max_total: self.max_total,
        };
        *self = Self::new(now);
        Some(report)
    }
}

struct Report {
    period: Duration,
    totals: Sample,
    outcomes: [u64; 3],
    max_total: Duration,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let calls = self
            .outcomes
            .iter()
            .fold(0u64, |sum, count| sum.saturating_add(*count));
        write!(
            f,
            "SGFX_ADRENO_SUBMIT_WINDOW kind=async period_us={} calls={} accepted={} busy={} rejected={} total_us={} max_total_us={}",
            self.period.as_micros(),
            calls,
            self.outcomes[0],
            self.outcomes[1],
            self.outcomes[2],
            self.totals.total.as_micros(),
            self.max_total.as_micros()
        )?;
        for (name, duration) in PHASE_NAMES.iter().zip(self.totals.phases) {
            write!(f, " {name}_us={}", duration.as_micros())?;
        }
        for (name, count) in COUNT_NAMES.iter().zip(self.totals.counts) {
            write!(f, " {name}={count}")?;
        }
        // Payload bytes only: no arena capacity/alignment padding or PM4 wire
        // bytes. Includes shader/state objects as well as texture pixel data.
        let upload_bytes = self.totals.counts[Count::BufferUploadBytes as usize]
            .saturating_add(self.totals.counts[Count::GeneratedUploadBytes as usize]);
        write!(f, " upload_bytes={upload_bytes}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_compile_split_and_copy_phases_partition_service_time() {
        let start = Instant::now();
        let mut timing = Timing::new(start);
        let mut sample = Sample::default();
        for (phase, us) in [
            (Phase::QueryLimits, 10),
            (Phase::MaterializePlan, 15),
            (Phase::Split, 40),
            (Phase::Pm4CompilePrepare, 50),
            (Phase::Split, 80),
            (Phase::Pm4CompilePrepare, 90),
            (Phase::UploadOwnership, 130),
            (Phase::Enqueue, 150),
            (Phase::Enqueue, 155),
        ] {
            timing.advance(&mut sample, phase, start + Duration::from_micros(us));
        }
        assert_eq!(sample.phases[Phase::Split as usize].as_micros(), 20);
        assert_eq!(
            sample.phases[Phase::Pm4CompilePrepare as usize].as_micros(),
            70
        );
        assert_eq!(
            sample.phases[Phase::UploadOwnership as usize].as_micros(),
            20
        );
        assert_eq!(sample.phases.iter().sum::<Duration>(), sample.total);
        assert_eq!(sample.total.as_micros(), 155);
        timing.advance(&mut sample, Phase::Enqueue, start);
        timing.advance(
            &mut sample,
            Phase::Enqueue,
            start + Duration::from_micros(160),
        );
        assert_eq!(sample.total.as_micros(), 160);
        assert_eq!(sample.phases.iter().sum::<Duration>(), sample.total);
    }

    #[test]
    fn windows_include_all_outcomes_preserve_submicroseconds_and_reset() {
        let start = Instant::now();
        let mut window = Window::new(start);
        let mut sample = Sample {
            total: Duration::from_nanos(600),
            ..Sample::default()
        };
        sample.phases[Phase::Validate as usize] = sample.total;
        sample.counts[Count::Commands as usize] = 2;
        sample.counts[Count::WireBytes as usize] = 64;
        assert!(window.record(sample, start).is_none()); // Rejected by default.
        sample.outcome = Outcome::Busy;
        assert!(
            window
                .record(sample, start + PERIOD - Duration::from_nanos(1))
                .is_none()
        );
        sample.outcome = Outcome::Accepted;
        let report = window.record(sample, start + PERIOD).unwrap();
        assert_eq!(report.outcomes, [1, 1, 1]);
        assert_eq!(report.totals.total, Duration::from_nanos(1800));
        assert_eq!(
            report.totals.phases[Phase::Validate as usize],
            report.totals.total
        );
        assert_eq!(report.totals.counts[Count::Commands as usize], 6);
        assert_eq!(report.totals.counts[Count::WireBytes as usize], 192);
        assert_eq!(report.max_total, sample.total);
        assert!(window.record(sample, start + PERIOD).is_none());
        let report = window.record(sample, start + PERIOD * 2).unwrap();
        assert_eq!(report.outcomes, [2, 0, 0]);
        assert_eq!(report.totals.counts[Count::Commands as usize], 4);
    }

    #[test]
    fn counters_saturate_and_reports_distinguish_uploads_from_wire() {
        let start = Instant::now();
        let mut window = Window::new(start);
        let mut sample = Sample {
            total: Duration::MAX,
            ..Sample::default()
        };
        sample.counts[Count::Commands as usize] = u64::MAX;
        sample.counts[Count::BufferUploadBytes as usize] = 32;
        sample.counts[Count::GeneratedUploadBytes as usize] = 96;
        sample.counts[Count::WireBytes as usize] = 48;
        assert!(window.record(sample, start).is_none());
        let report = window.record(sample, start + PERIOD * 3).unwrap();
        assert_eq!(report.totals.total, Duration::MAX);
        assert_eq!(report.totals.counts[Count::Commands as usize], u64::MAX);
        let text = report.to_string();
        assert!(text.starts_with("SGFX_ADRENO_SUBMIT_WINDOW kind=async period_us=3000000 calls=2"));
        assert!(text.contains(" wire_bytes=96 "));
        assert!(text.contains(" buffer_upload_bytes=64 generated_upload_bytes=192"));
        assert!(text.ends_with(" upload_bytes=256"));
        assert!(!text.contains('\n'));
        for name in PHASE_NAMES {
            assert!(text.contains(&format!(" {name}_us=")));
        }
        // Even a very slow call produces only one report, never a burst of
        // catch-up windows or a separate per-submission spike log.
        assert!(
            window
                .record(Sample::default(), start + PERIOD * 3)
                .is_none()
        );
    }
}
