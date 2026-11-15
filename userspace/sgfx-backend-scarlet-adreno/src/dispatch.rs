//! Autonomous context-ordered dispatch and owned logical completion signals.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
#[cfg(feature = "std")]
use std::sync::Condvar;
use std::sync::{Mutex, MutexGuard};

use gpu_raw::{
    GPU_ABI_VERSION, GPU_RESULT_SUCCESS, GpuCompletion, GpuQueue, GpuQueueAsyncInfo, GpuSubmitError,
};
#[cfg(feature = "std")]
use scarlet_os::{
    ipc::pipe,
    poll::{POLLIN, PollHandle, poll},
};
#[cfg(not(feature = "std"))]
use std::{
    ipc::pipe,
    poll::{POLLIN, PollHandle, poll},
};

use crate::asynchronous::{DispatchOwner, UploadArena};
use crate::completion::completion_status;
#[cfg(not(feature = "std"))]
use crate::completion::{monotonic_time_ns, remaining_timeout_ns};
use crate::resource::RawBuffer;
use crate::scheduler::{
    AdmissionError, DispatchError, Job, MAX_PENDING_BYTES, MAX_SUBMISSIONS, Scheduler, Transport,
};
use crate::{Handle, HandleError, HandleResult, IrSubmitError};
use sgfx_core::backend::CompletionStatus;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    #[cfg(feature = "std")]
    {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    #[cfg(not(feature = "std"))]
    {
        mutex.lock()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    Backend(HandleError),
    Completion(u32),
    Unavailable,
}

impl From<Failure> for IrSubmitError {
    fn from(value: Failure) -> Self {
        match value {
            Failure::Backend(error) => Self::Backend(error),
            Failure::Completion(reason) => Self::CompletionFailed(reason),
            Failure::Unavailable => Self::CompletionUnavailable,
        }
    }
}

fn native_status(completion: &GpuCompletion) -> Result<bool, Failure> {
    let info = completion.query().map_err(|_| Failure::Unavailable)?;
    completion_status(info)
        .map(|status| status == CompletionStatus::Complete)
        .map_err(|error| match error {
            IrSubmitError::CompletionFailed(reason) => Failure::Completion(reason),
            _ => Failure::Unavailable,
        })
}

// One coalesced byte is sufficient and cannot fill the pipe. Queue wakeups have
// one reader; terminal signals are never consumed, so all waiters see readiness.
struct Event {
    reader: Handle,
    writer: Handle,
    signalled: AtomicBool,
}

impl Event {
    fn new() -> HandleResult<Self> {
        let (reader, writer) = pipe().map_err(|_| HandleError::OutOfResources)?;
        Ok(Self {
            reader,
            writer,
            signalled: AtomicBool::new(false),
        })
    }

    fn notify(&self) {
        if !self.signalled.swap(true, Ordering::AcqRel) {
            let result = self
                .writer
                .as_stream()
                .and_then(|stream| stream.write(&[1]).map_err(|_| HandleError::SystemError(-1)));
            if result != Ok(1) {
                self.signalled.store(false, Ordering::Release);
            }
        }
    }

    fn consume(&self) -> HandleResult<()> {
        // A notifier publishes the flag before writing the byte. Never block
        // if that thread has not finished the write yet. Check the pipe itself
        // so an uncertain write response cannot leave an unread wake spinning.
        if poll(&mut [self.poll_handle()], 0).map_err(HandleError::SystemError)? != 0 {
            let mut byte = [0];
            let length = self
                .reader
                .as_stream()?
                .read(&mut byte)
                .map_err(|_| HandleError::SystemError(-1))?;
            if length != 1 {
                return Err(HandleError::SystemError(-1));
            }
            self.signalled.store(false, Ordering::Release);
        }
        Ok(())
    }

    fn poll_handle(&self) -> PollHandle {
        PollHandle::new(self.reader.as_raw() as u32, POLLIN)
    }
}

pub(crate) struct Signal {
    bytes: usize,
    result: Mutex<Option<Result<(), Failure>>>,
    #[cfg(feature = "std")]
    changed: Condvar,
    #[cfg(not(feature = "std"))]
    event: Event,
}

impl Signal {
    #[cfg(test)]
    fn new() -> HandleResult<Self> {
        Self::with_bytes(0)
    }

    fn with_bytes(bytes: usize) -> HandleResult<Self> {
        Ok(Self {
            bytes,
            result: Mutex::new(None),
            #[cfg(feature = "std")]
            changed: Condvar::new(),
            #[cfg(not(feature = "std"))]
            event: Event::new()?,
        })
    }

    pub(crate) fn poll(&self) -> Result<CompletionStatus, IrSubmitError> {
        match *lock(&self.result) {
            None => Ok(CompletionStatus::Pending),
            Some(Ok(())) => Ok(CompletionStatus::Complete),
            Some(Err(error)) => Err(error.into()),
        }
    }

    #[cfg(feature = "std")]
    pub(crate) fn wait(
        &self,
        timeout: Option<Duration>,
    ) -> Result<CompletionStatus, IrSubmitError> {
        let started = std::time::Instant::now();
        let mut result = lock(&self.result);
        loop {
            match *result {
                Some(Ok(())) => return Ok(CompletionStatus::Complete),
                Some(Err(error)) => return Err(error.into()),
                None => {}
            }
            // Receipts need no pollable object. Test and sleep
            // under the completion mutex so no notification can be lost.
            result = if let Some(limit) = timeout {
                let remaining = limit.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Ok(CompletionStatus::Pending);
                }
                self.changed
                    .wait_timeout(result, remaining)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0
            } else {
                self.changed
                    .wait(result)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            };
        }
    }

    #[cfg(not(feature = "std"))]
    pub(crate) fn wait(
        &self,
        timeout: Option<Duration>,
    ) -> Result<CompletionStatus, IrSubmitError> {
        let started = monotonic_time_ns();
        loop {
            let status = self.poll()?;
            if status == CompletionStatus::Complete {
                return Ok(status);
            }
            let remaining = remaining_timeout_ns(timeout, started, monotonic_time_ns());
            if remaining == 0 {
                return Ok(CompletionStatus::Pending);
            }
            // A lost pipe notification cannot strand terminal observation.
            // Normal completion wakes immediately; this bounded check is only
            // a notification-error fallback and preserves the caller deadline.
            let duration = if remaining < 0 {
                100_000_000
            } else {
                remaining.min(100_000_000)
            };
            poll(&mut [self.event.poll_handle()], duration)
                .map_err(|error| IrSubmitError::Backend(HandleError::SystemError(error)))?;
        }
    }

    fn complete(&self, result: Result<(), Failure>) {
        // Publish the
        // result and wake waiters without formatting or writing to a stream:
        // stdout can block on a terminal, pipe or filesystem and would delay
        // both completion observation and admission of the next frame.
        *lock(&self.result) = Some(result);
        #[cfg(feature = "std")]
        self.changed.notify_all();
        #[cfg(not(feature = "std"))]
        self.event.notify();
    }
}

pub(crate) enum Chunk {
    Commands {
        commands: Vec<u8>,
        arena: Arc<UploadArena>,
        uploads: Vec<(u64, Vec<u8>)>,
    },
    // Only byte-unaligned writes use this fallback. Aligned writes are native
    // Commands with receipt-protected upload storage, not CPU queue barriers.
    WriteBuffer {
        buffer: Arc<RawBuffer>,
        offset: u64,
        data: Vec<u8>,
    },
    Barrier,
}

enum Receipt {
    Native(Arc<GpuCompletion>),
    Cpu,
}

struct Native {
    shared: Arc<Shared>,
}

impl Transport for Native {
    type Chunk = Chunk;
    // Retain the attachment owner, as well as the queue, until every queued
    // chunk has been submitted and retired. Session/receipt drop cannot detach
    // resources needed by chunks that have not reached the kernel yet.
    type Owner = Arc<DispatchOwner>;
    type Receipt = Receipt;
    type Signal = Arc<Signal>;
    type Error = Failure;

    fn size(chunk: &Chunk) -> usize {
        match chunk {
            Chunk::Commands {
                commands, uploads, ..
            } => uploads.iter().fold(commands.len(), |size, (_, bytes)| {
                size.saturating_add(bytes.len())
            }),
            Chunk::WriteBuffer { data, .. } => data.len(),
            Chunk::Barrier => 0,
        }
    }

    fn requires_idle(chunk: &Chunk) -> bool {
        !matches!(chunk, Chunk::Commands { .. })
    }

    fn ready(&self, chunk: &Chunk) -> Result<bool, Failure> {
        let Chunk::Commands { arena, .. } = chunk else {
            return Ok(true);
        };
        let previous = lock(&arena.completion).clone();
        previous
            .as_ref()
            .map_or(Ok(true), |completion| native_status(completion))
    }

    fn submit(
        &self,
        owner: &Arc<DispatchOwner>,
        chunk: &Chunk,
    ) -> Result<Receipt, DispatchError<Failure>> {
        let (commands, arena, uploads) = match chunk {
            Chunk::Commands {
                commands,
                arena,
                uploads,
            } => (commands, arena, uploads),
            Chunk::WriteBuffer {
                buffer,
                offset,
                data,
            } => {
                // Only the worker writes, after every earlier native receipt
                // retired. Admission keeps owned bytes; subsequent draws cannot
                // pass this update. RawBuffer is CPU/GPU coherent on A618.
                buffer
                    .write(*offset, data)
                    .map_err(|error| DispatchError::Failed(Failure::Backend(error)))?;
                return Ok(Receipt::Cpu);
            }
            Chunk::Barrier => return Ok(Receipt::Cpu),
        };
        for (offset, bytes) in uploads {
            arena
                .buffer
                .write(*offset, bytes)
                .map_err(|error| DispatchError::Failed(Failure::Backend(error)))?;
        }
        match owner.queue.submit_async(commands) {
            Ok(completion) => {
                let completion = Arc::new(completion);
                *lock(&arena.completion) = Some(Arc::clone(&completion));
                Ok(Receipt::Native(completion))
            }
            Err(GpuSubmitError::Busy) => Err(DispatchError::Busy),
            Err(GpuSubmitError::Rejected(error) | GpuSubmitError::Failed { error, .. }) => {
                // Acceptance happened at logical enqueue. Any later rejection
                // or uncertain native acceptance fails the receipt and poisons
                // the queue, never replays it. Kernel-owned references protect
                // possibly accepted work after these observation handles drop.
                Err(DispatchError::Failed(Failure::Backend(error)))
            }
            Err(_) => Err(DispatchError::Failed(Failure::Unavailable)),
        }
    }

    fn poll(&self, receipt: &Receipt) -> Result<bool, Failure> {
        match receipt {
            Receipt::Native(completion) => native_status(completion),
            Receipt::Cpu => Ok(true),
        }
    }
    fn complete(&self, signal: &Arc<Signal>, result: Result<(), Failure>) {
        self.shared.finish_signal(signal, result);
    }
}

// Only admission accounting is shared with producers. GPU syscalls and receipt
// polling run on the worker's private Scheduler, outside this short mutex.
struct Admission {
    pending: VecDeque<Job<Native>>,
    count: usize,
    bytes: usize,
    last: Option<Arc<Signal>>,
    failure: Option<Failure>,
}

struct Shared {
    admission: Mutex<Admission>,
    wake: Event,
    closing: AtomicBool,
    failed: AtomicBool,
}

impl Shared {
    fn finish_signal(&self, signal: &Arc<Signal>, result: Result<(), Failure>) {
        {
            let mut state = lock(&self.admission);
            state.count -= 1;
            state.bytes -= signal.bytes;
            if let Err(error) = result {
                state.failure = Some(error);
                self.failed.store(true, Ordering::Release);
            }
            if state.count == 0 {
                state.last = None;
            }
        }
        signal.complete(result);
    }
}

// A618 capacity is a constant, not an available-slot count. Scarlet freezes
// the async command limit at queue creation. Retain one queue identity rather
// than a raw handle number (which can be reused after close); never cache errors.
struct QueueLimits<Q> {
    last: Option<(Arc<Q>, usize)>,
}

impl<Q> QueueLimits<Q> {
    fn new() -> Self {
        Self { last: None }
    }

    fn get_or_query(
        &mut self,
        queue: &Arc<Q>,
        query: impl FnOnce() -> HandleResult<GpuQueueAsyncInfo>,
    ) -> Result<usize, IrSubmitError> {
        if let Some((cached, limit)) = &self.last {
            if Arc::ptr_eq(cached, queue) {
                return Ok(*limit);
            }
        }
        let info = query().map_err(|_| IrSubmitError::AsyncUnsupported)?;
        if info.abi_version != GPU_ABI_VERSION
            || info.result != GPU_RESULT_SUCCESS
            || info.reserved != 0
            || info.reserved2 != 0
            || info.max_pending_submissions == 0
            || info.max_opaque_command_size == 0
        {
            return Err(IrSubmitError::AsyncUnsupported);
        }
        let limit = usize::try_from(info.max_opaque_command_size)
            .map_err(|_| IrSubmitError::AsyncUnsupported)?;
        self.last = Some((Arc::clone(queue), limit));
        Ok(limit)
    }
}

pub(crate) struct NativeScheduler {
    shared: Arc<Shared>,
    limits: Mutex<QueueLimits<GpuQueue>>,
}

impl NativeScheduler {
    pub(crate) fn new() -> HandleResult<Self> {
        let mut pending = VecDeque::new();
        pending
            .try_reserve(MAX_SUBMISSIONS)
            .map_err(|_| HandleError::OutOfResources)?;
        let mut scheduler = Scheduler::new();
        scheduler
            .reserve_admission()
            .map_err(|_| HandleError::OutOfResources)?;
        let shared = Arc::new(Shared {
            admission: Mutex::new(Admission {
                pending,
                count: 0,
                bytes: 0,
                last: None,
                failure: None,
            }),
            wake: Event::new()?,
            closing: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        let worker = Worker {
            native: Native {
                shared: Arc::clone(&shared),
            },
            scheduler,
        };
        std::thread::Builder::new()
            .spawn(move || worker.run())
            .map_err(|_| HandleError::OutOfResources)?;
        Ok(Self {
            shared,
            limits: Mutex::new(QueueLimits::new()),
        })
    }

    /// Cache only queue-lifetime limits, never Busy, health, or completion.
    /// A distinct mutex keeps the cold syscall out of producer admission and
    /// worker progress; concurrent capability checks share the first success.
    pub(crate) fn async_command_limit(
        &self,
        queue: &Arc<GpuQueue>,
    ) -> Result<usize, IrSubmitError> {
        lock(&self.limits).get_or_query(queue, || queue.query_async())
    }

    pub(crate) fn enqueue(
        &self,
        owner: Arc<DispatchOwner>,
        chunks: Vec<Chunk>,
    ) -> Result<Arc<Signal>, AdmissionError<Failure>> {
        let bytes = chunks
            .iter()
            .try_fold(0usize, |n, chunk| n.checked_add(Native::size(chunk)))
            .filter(|n| *n <= MAX_PENDING_BYTES)
            .ok_or(AdmissionError::TooLarge)?;
        let signal = Arc::new(Signal::with_bytes(bytes).map_err(|_| AdmissionError::OutOfMemory)?);
        let job = Job::prepare(chunks, owner, Arc::clone(&signal))?;
        {
            let mut state = lock(&self.shared.admission);
            if let Some(error) = state.failure {
                return Err(AdmissionError::Failed(error));
            }
            if state.count == MAX_SUBMISSIONS
                || (state.count != 0
                    && (bytes > MAX_PENDING_BYTES
                        || state.bytes.saturating_add(bytes) > MAX_PENDING_BYTES))
            {
                return Err(AdmissionError::Busy);
            }
            // Capacity was reserved at construction. Global admission includes
            // both this inbox and the worker's active streams.
            state.pending.push_back(job);
            state.count += 1;
            state.bytes += bytes;
            state.last = Some(Arc::clone(&signal));
        }
        self.shared.wake.notify();
        Ok(signal)
    }

    pub(crate) fn wait_idle(&self) -> HandleResult<()> {
        let signal = {
            let state = lock(&self.shared.admission);
            if state.failure.is_some() {
                return Err(HandleError::SystemError(-1));
            }
            state.last.clone()
        };
        if let Some(signal) = signal
            && signal
                .wait(None)
                .map_err(|_| HandleError::SystemError(-1))?
                != CompletionStatus::Complete
        {
            return Err(HandleError::SystemError(-1));
        }
        Ok(())
    }

    pub(crate) fn is_idle(&self) -> HandleResult<bool> {
        let state = lock(&self.shared.admission);
        if state.failure.is_some() {
            return Err(HandleError::SystemError(-1));
        }
        Ok(state.count == 0)
    }

    pub(crate) fn check(&self) -> HandleResult<()> {
        if self.shared.failed.load(Ordering::Acquire) {
            Err(HandleError::SystemError(-1))
        } else {
            Ok(())
        }
    }
}

impl Drop for NativeScheduler {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify();
    }
}

struct Worker {
    native: Native,
    scheduler: Scheduler<Native>,
}

impl Worker {
    fn fail(&mut self, error: Failure) {
        let pending = {
            let mut state = lock(&self.native.shared.admission);
            state.failure = Some(error);
            self.native.shared.failed.store(true, Ordering::Release);
            core::mem::take(&mut state.pending)
        };
        self.scheduler.fail(&self.native, error);
        for job in pending {
            self.native.complete(&job.signal, Err(error));
        }
    }

    fn run(mut self) {
        let mut handles = Vec::new();
        if handles.try_reserve_exact(17).is_err() {
            self.fail(Failure::Backend(HandleError::OutOfResources));
            return;
        }
        loop {
            if let Err(error) = self.native.shared.wake.consume() {
                self.fail(Failure::Backend(error));
                return;
            }
            loop {
                let job = { lock(&self.native.shared.admission).pending.pop_front() };
                let Some(job) = job else {
                    break;
                };
                let signal = Arc::clone(&job.signal);
                // The shared admission already reserved capacity and owns all
                // allocations. Transfer preserves FIFO without a GPU syscall.
                if self.scheduler.enqueue_prepared(job).is_err() {
                    self.native.complete(&signal, Err(Failure::Unavailable));
                    self.fail(Failure::Unavailable);
                    return;
                }
            }
            let progressed = self.scheduler.advance(&self.native);
            if let Some(error) = self.scheduler.failure() {
                self.fail(error);
                return;
            }
            if self.scheduler.is_empty()
                && self.native.shared.closing.load(Ordering::Acquire)
                && lock(&self.native.shared.admission).count == 0
            {
                return;
            }
            if progressed {
                continue;
            }
            handles.clear();
            handles.push(self.native.shared.wake.poll_handle());
            handles.extend(
                self.scheduler
                    .receipts()
                    .filter_map(|receipt| match receipt {
                        Receipt::Native(completion) => Some(PollHandle::new(
                            completion.as_handle().as_raw() as u32,
                            POLLIN,
                        )),
                        Receipt::Cpu => None,
                    }),
            );
            let timeout = if !self.scheduler.is_empty() && handles.len() == 1 {
                1_000_000
            } else {
                100_000_000
            };
            if let Err(error) = poll(&mut handles, timeout) {
                self.fail(Failure::Backend(HandleError::SystemError(error)));
                return;
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let pending = { lock(&self.native.shared.admission).count };
        if !self.scheduler.is_empty() || pending != 0 {
            self.fail(Failure::Unavailable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GPU_ABI_VERSION, GPU_RESULT_SUCCESS, QueueLimits};
    use alloc::sync::Arc;
    use gpu_raw::GpuQueueAsyncInfo;

    fn supported_limits() -> GpuQueueAsyncInfo {
        GpuQueueAsyncInfo {
            abi_version: GPU_ABI_VERSION,
            result: GPU_RESULT_SUCCESS,
            max_pending_submissions: 16,
            max_opaque_command_size: 64 * 1024,
            reserved: 0,
            reserved2: 0,
        }
    }

    #[test]
    fn queue_limit_cache_reuses_success_but_not_recycled_handle_identity() {
        let mut cache = QueueLimits::new();
        let queue = Arc::new(42);
        let retained = Arc::downgrade(&queue);
        let calls = core::cell::Cell::new(0);
        for _ in 0..128 {
            let clone = Arc::clone(&queue);
            assert_eq!(
                cache
                    .get_or_query(&clone, || {
                        calls.set(calls.get() + 1);
                        Ok(supported_limits())
                    })
                    .unwrap(),
                64 * 1024
            );
        }
        assert_eq!(calls.get(), 1);
        drop(queue);
        assert!(
            retained.upgrade().is_some(),
            "cache owns identity until replacement"
        );
        // Same numeric handle, different owning Arc: must not inherit old limits.
        let replacement = Arc::new(42);
        let mut smaller = supported_limits();
        smaller.max_opaque_command_size = 4096;
        assert_eq!(
            cache
                .get_or_query(&replacement, || {
                    calls.set(calls.get() + 1);
                    Ok(smaller)
                })
                .unwrap(),
            4096
        );
        assert_eq!(calls.get(), 2);
        assert!(
            retained.upgrade().is_none(),
            "bounded cache releases replaced queue"
        );
    }

    #[test]
    fn unsupported_failed_and_malformed_queue_queries_are_never_cached() {
        use super::{HandleError, IrSubmitError};
        let queue = Arc::new(1);
        let mut cache = QueueLimits::new();
        assert!(matches!(
            cache.get_or_query(&queue, || Err(HandleError::Unsupported)),
            Err(IrSubmitError::AsyncUnsupported)
        ));
        for field in 0..6 {
            let mut invalid = supported_limits();
            match field {
                0 => invalid.abi_version ^= 1,
                1 => invalid.result = GPU_RESULT_SUCCESS.wrapping_add(1),
                2 => invalid.max_pending_submissions = 0,
                3 => invalid.max_opaque_command_size = 0,
                4 => invalid.reserved = 1,
                _ => invalid.reserved2 = 1,
            }
            assert!(matches!(
                cache.get_or_query(&queue, || Ok(invalid)),
                Err(IrSubmitError::AsyncUnsupported)
            ));
        }
        assert_eq!(
            cache
                .get_or_query(&queue, || Ok(supported_limits()))
                .unwrap(),
            64 * 1024
        );
        // A failed query on another queue cannot invalidate or alias this one.
        let other = Arc::new(2);
        assert!(
            cache
                .get_or_query(&other, || Err(HandleError::Unsupported))
                .is_err()
        );
        assert_eq!(
            cache
                .get_or_query(&queue, || panic!("successful queue requeried"))
                .unwrap(),
            64 * 1024
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn concurrent_capability_checks_share_one_successful_query() {
        use super::{Mutex, lock};
        use core::sync::atomic::{AtomicUsize, Ordering};
        let cache = Arc::new(Mutex::new(QueueLimits::new()));
        let queue = Arc::new(3);
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(std::sync::Barrier::new(8));
        let threads: alloc::vec::Vec<_> = (0..8)
            .map(|_| {
                let (cache, queue, calls, start) =
                    (cache.clone(), queue.clone(), calls.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..64 {
                        assert_eq!(
                            lock(&cache)
                                .get_or_query(&queue, || {
                                    calls.fetch_add(1, Ordering::Relaxed);
                                    Ok(supported_limits())
                                })
                                .unwrap(),
                            64 * 1024
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[cfg(feature = "std")]
    #[test]
    fn receipt_wait_is_level_triggered_and_observable_by_multiple_waiters() {
        use super::*;
        let signal = Arc::new(Signal::new().unwrap());
        let entered = Arc::new(std::sync::Barrier::new(3));
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let signal = Arc::clone(&signal);
                let entered = Arc::clone(&entered);
                std::thread::spawn(move || {
                    entered.wait();
                    signal.wait(Some(Duration::from_secs(2)))
                })
            })
            .collect();
        entered.wait();
        signal.complete(Ok(()));
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap().unwrap(), CompletionStatus::Complete);
        }
        assert_eq!(
            signal.wait(Some(Duration::ZERO)).unwrap(),
            CompletionStatus::Complete
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn receipt_timeout_does_not_consume_later_completion_or_failure() {
        use super::*;
        let signal = Signal::new().unwrap();
        assert_eq!(
            signal.wait(Some(Duration::from_millis(1))).unwrap(),
            CompletionStatus::Pending
        );
        signal.complete(Err(Failure::Unavailable));
        for _ in 0..2 {
            assert!(matches!(
                signal.wait(Some(Duration::ZERO)),
                Err(IrSubmitError::CompletionUnavailable)
            ));
        }
    }
}
