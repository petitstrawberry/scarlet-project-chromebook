//! Owned observation of a complete logical submission and its native chunks.

use alloc::sync::Arc;
use core::{fmt, time::Duration};

use gpu_raw::{
    GPU_ABI_VERSION, GPU_COMPLETION_COMPLETE, GPU_COMPLETION_FAILED,
    GPU_COMPLETION_FAILURE_ABANDONED, GPU_COMPLETION_FAILURE_DEVICE_LOST,
    GPU_COMPLETION_FAILURE_EXECUTION, GPU_COMPLETION_FAILURE_NONE, GPU_COMPLETION_PENDING,
    GPU_RESULT_SUCCESS, GpuCompletionInfo,
};
use sgfx_core::backend::{Completion, CompletionStatus};

use crate::IrSubmitError;
use crate::dispatch::Signal;

/// Owned completion receipt covering every chunk of an accepted logical stream.
///
/// Clones share the worker's completion signal. The receipt does not borrow its
/// executor or command data. Dropping it neither waits nor cancels work; the
/// worker and kernel retain queued commands and resources independently.
/// Completion does not acknowledge presentation, CPU visibility, or external
/// leases. Failure never certifies quiescence or permits shared-buffer reuse.
#[derive(Clone)]
pub struct Submission {
    signal: Arc<Signal>,
}

impl Submission {
    pub(crate) fn new(signal: Arc<Signal>) -> Self {
        Self { signal }
    }
}

impl fmt::Debug for Submission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Submission")
            .field("status", &self.signal.poll())
            .finish()
    }
}

impl Completion for Submission {
    type Error = IrSubmitError;

    /// Observe whether the worker has retired every covered native chunk.
    ///
    /// # Returns
    ///
    /// Complete only after all chunks and earlier queue work succeed, otherwise
    /// Pending or an observation/execution error. Dispatch progresses even when
    /// the caller never polls or drops this receipt immediately after admission.
    fn poll(&self) -> Result<CompletionStatus, IrSubmitError> {
        self.signal.poll()
    }

    /// Wait on the whole-stream signal until completion or the caller deadline.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum requested duration; zero polls and None has no
    ///   deadline. A timeout neither cancels work nor invalidates this receipt.
    ///
    /// # Returns
    ///
    /// Complete, Pending after timeout, or an observation/execution error.
    fn wait(&self, timeout: Option<Duration>) -> Result<CompletionStatus, IrSubmitError> {
        if timeout == Some(Duration::ZERO) {
            self.poll()
        } else {
            self.signal.wait(timeout)
        }
    }
}

pub(crate) fn completion_status(
    info: GpuCompletionInfo,
) -> Result<CompletionStatus, IrSubmitError> {
    if info.abi_version != GPU_ABI_VERSION
        || info.result != GPU_RESULT_SUCCESS
        || info.reserved != 0
        || info.reserved2 != 0
    {
        return Err(IrSubmitError::CompletionUnavailable);
    }
    match (info.state, info.failure) {
        (GPU_COMPLETION_PENDING, GPU_COMPLETION_FAILURE_NONE) => Ok(CompletionStatus::Pending),
        (GPU_COMPLETION_COMPLETE, GPU_COMPLETION_FAILURE_NONE) => Ok(CompletionStatus::Complete),
        (
            GPU_COMPLETION_FAILED,
            failure @ (GPU_COMPLETION_FAILURE_DEVICE_LOST
            | GPU_COMPLETION_FAILURE_ABANDONED
            | GPU_COMPLETION_FAILURE_EXECUTION),
        ) => Err(IrSubmitError::CompletionFailed(failure)),
        _ => Err(IrSubmitError::CompletionUnavailable),
    }
}

pub(crate) fn remaining_timeout_ns(timeout: Option<Duration>, started: u64, now: u64) -> i64 {
    match timeout {
        None => -1,
        Some(timeout) => timeout
            .as_nanos()
            .saturating_sub(u128::from(now.saturating_sub(started)))
            .min(i64::MAX as u128) as i64,
    }
}

#[cfg(feature = "std")]
pub(crate) fn monotonic_time_ns() -> u64 {
    scarlet_os::time::monotonic_time_ns()
}

#[cfg(not(feature = "std"))]
pub(crate) fn monotonic_time_ns() -> u64 {
    use std::syscall::{Syscall, syscall0};
    // SAFETY: This fixed clock query has no arguments or userspace memory effects.
    (unsafe { syscall0(Syscall::MonotonicTime) }) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(state: u32, failure: u32) -> GpuCompletionInfo {
        GpuCompletionInfo {
            state,
            failure,
            ..GpuCompletionInfo::new()
        }
    }

    #[test]
    fn completion_validates_state_and_failure_together() {
        assert_eq!(
            completion_status(info(GPU_COMPLETION_PENDING, 0)).unwrap(),
            CompletionStatus::Pending,
        );
        assert_eq!(
            completion_status(info(GPU_COMPLETION_COMPLETE, 0)).unwrap(),
            CompletionStatus::Complete,
        );
        for failure in [
            GPU_COMPLETION_FAILURE_DEVICE_LOST,
            GPU_COMPLETION_FAILURE_ABANDONED,
            GPU_COMPLETION_FAILURE_EXECUTION,
        ] {
            assert!(matches!(
                completion_status(info(GPU_COMPLETION_FAILED, failure)),
                Err(IrSubmitError::CompletionFailed(reason)) if reason == failure
            ));
            for state in [GPU_COMPLETION_PENDING, GPU_COMPLETION_COMPLETE] {
                assert!(matches!(
                    completion_status(info(state, failure)),
                    Err(IrSubmitError::CompletionUnavailable)
                ));
            }
        }
        for (state, failure) in [
            (GPU_COMPLETION_FAILED, 0),
            (GPU_COMPLETION_FAILED, u32::MAX),
            (u32::MAX, 0),
        ] {
            assert!(matches!(
                completion_status(info(state, failure)),
                Err(IrSubmitError::CompletionUnavailable)
            ));
        }
    }

    #[test]
    fn completion_rejects_malformed_query_metadata() {
        let valid = info(GPU_COMPLETION_COMPLETE, 0);
        for malformed in [
            GpuCompletionInfo {
                abi_version: GPU_ABI_VERSION + 1,
                ..valid
            },
            GpuCompletionInfo {
                result: u32::MAX,
                ..valid
            },
            GpuCompletionInfo {
                reserved: 1,
                ..valid
            },
            GpuCompletionInfo {
                reserved2: 1,
                ..valid
            },
        ] {
            assert!(matches!(
                completion_status(malformed),
                Err(IrSubmitError::CompletionUnavailable)
            ));
        }
    }

    #[test]
    fn timeout_conversion_preserves_deadline_and_never_wraps_to_infinite() {
        assert_eq!(remaining_timeout_ns(None, 100, 120), -1);
        assert_eq!(remaining_timeout_ns(Some(Duration::ZERO), 100, 100), 0);
        let timeout = Some(Duration::from_nanos(100));
        assert_eq!(remaining_timeout_ns(timeout, 100, 100), 100);
        assert_eq!(remaining_timeout_ns(timeout, 100, 125), 75);
        assert_eq!(remaining_timeout_ns(timeout, 100, 200), 0);
        assert_eq!(remaining_timeout_ns(timeout, 100, 250), 0);
        assert_eq!(
            remaining_timeout_ns(Some(Duration::MAX), 0, u64::MAX),
            i64::MAX
        );
    }
}
