//! Bounded logical admission and ordered Adreno native dispatch, independent of syscalls.

extern crate alloc;

use alloc::{collections::VecDeque, vec::Vec};

pub(crate) const MAX_SUBMISSIONS: usize = 16;
pub(crate) const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
const MAX_NATIVE_IN_FLIGHT: usize = 16;

pub(crate) enum DispatchError<E> {
    Busy,
    Failed(E),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AdmissionError<E> {
    Busy,
    TooLarge,
    OutOfMemory,
    Failed(E),
}

pub(crate) trait Transport {
    type Chunk;
    type Owner;
    type Receipt;
    type Signal;
    type Error: Copy;

    fn size(chunk: &Self::Chunk) -> usize;
    fn ready(&self, chunk: &Self::Chunk) -> Result<bool, Self::Error>;
    fn submit(
        &self,
        owner: &Self::Owner,
        chunk: &Self::Chunk,
    ) -> Result<Self::Receipt, DispatchError<Self::Error>>;
    fn poll(&self, receipt: &Self::Receipt) -> Result<bool, Self::Error>;
    fn complete(&self, signal: &Self::Signal, result: Result<(), Self::Error>);
}

struct Job<T: Transport> {
    chunks: Vec<T::Chunk>,
    owner: T::Owner,
    signal: T::Signal,
    next: usize,
    receipts: VecDeque<T::Receipt>,
    bytes: usize,
}

pub(crate) struct Scheduler<T: Transport> {
    jobs: VecDeque<Job<T>>,
    bytes: usize,
    failure: Option<T::Error>,
}

impl<T: Transport> Scheduler<T> {
    pub(crate) fn new() -> Self {
        Self {
            jobs: VecDeque::new(),
            bytes: 0,
            failure: None,
        }
    }

    pub(crate) fn failure(&self) -> Option<T::Error> {
        self.failure
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub(crate) fn receipts(&self) -> impl Iterator<Item = &T::Receipt> {
        self.jobs.iter().flat_map(|job| job.receipts.iter())
    }

    pub(crate) fn last_signal(&self) -> Option<&T::Signal> {
        self.jobs.back().map(|job| &job.signal)
    }

    // All fallible allocation precedes publication. No transport operation is
    // performed here, even when hardware currently has no free descriptors.
    pub(crate) fn enqueue(
        &mut self,
        chunks: Vec<T::Chunk>,
        owner: T::Owner,
        signal: T::Signal,
    ) -> Result<(), AdmissionError<T::Error>> {
        if let Some(error) = self.failure {
            return Err(AdmissionError::Failed(error));
        }
        let bytes = chunks
            .iter()
            .try_fold(0usize, |size, chunk| size.checked_add(T::size(chunk)))
            .filter(|size| *size <= MAX_PENDING_BYTES)
            .ok_or(AdmissionError::TooLarge)?;
        if self.jobs.len() == MAX_SUBMISSIONS
            || self.bytes.saturating_add(bytes) > MAX_PENDING_BYTES
        {
            return Err(AdmissionError::Busy);
        }
        let mut receipts = VecDeque::new();
        receipts
            .try_reserve(chunks.len().min(MAX_NATIVE_IN_FLIGHT))
            .map_err(|_| AdmissionError::OutOfMemory)?;
        self.jobs
            .try_reserve(1)
            .map_err(|_| AdmissionError::OutOfMemory)?;
        self.jobs.push_back(Job {
            chunks,
            owner,
            signal,
            next: 0,
            receipts,
            bytes,
        });
        self.bytes += bytes;
        Ok(())
    }

    pub(crate) fn fail(&mut self, transport: &T, error: T::Error) {
        self.failure = Some(error);
        for job in self.jobs.drain(..) {
            transport.complete(&job.signal, Err(error));
        }
        self.bytes = 0;
    }

    // Called only by the queue worker. Busy leaves the exact next chunk in
    // place; accepted chunks are never replayed and later jobs cannot pass it.
    // Nothing here waits for GPU completion.
    pub(crate) fn advance(&mut self, transport: &T) -> bool {
        let mut progressed = false;
        for job in &mut self.jobs {
            // Every receipt participates in the worker's wait set. Inspect all
            // of them so a later completion cannot leave an already-ready
            // descriptor spinning behind a pending earlier receipt.
            let mut index = 0;
            while let Some(receipt) = job.receipts.get(index) {
                match transport.poll(receipt) {
                    Ok(true) => {
                        job.receipts.remove(index);
                        progressed = true;
                    }
                    Ok(false) => index += 1,
                    Err(error) => {
                        self.fail(transport, error);
                        return true;
                    }
                }
            }
        }
        // Even a later fence observed first cannot certify its queue prefix.
        while self
            .jobs
            .front()
            .is_some_and(|job| job.next == job.chunks.len() && job.receipts.is_empty())
        {
            if let Some(job) = self.jobs.pop_front() {
                self.bytes -= job.bytes;
                transport.complete(&job.signal, Ok(()));
                progressed = true;
            }
        }

        let mut in_flight = self.receipts().count();
        for job in &mut self.jobs {
            while let Some(chunk) = job.chunks.get(job.next) {
                if in_flight == MAX_NATIVE_IN_FLIGHT {
                    return progressed;
                }
                match transport.ready(chunk) {
                    Ok(true) => {}
                    Ok(false) => return progressed,
                    Err(error) => {
                        self.fail(transport, error);
                        return true;
                    }
                }
                match transport.submit(&job.owner, chunk) {
                    Ok(receipt) => {
                        job.receipts.push_back(receipt);
                        job.next += 1;
                        in_flight += 1;
                        progressed = true;
                    }
                    Err(DispatchError::Busy) => return progressed,
                    Err(DispatchError::Failed(error)) => {
                        self.fail(transport, error);
                        return true;
                    }
                }
            }
        }
        progressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::{Cell, RefCell};

    type Signal = Rc<Cell<Option<Result<(), u32>>>>;

    #[derive(Default)]
    struct Fake {
        accepted: RefCell<Vec<u32>>,
        fences: RefCell<Vec<Rc<Cell<Option<Result<(), u32>>>>>>,
        busy: Cell<bool>,
        ready: Cell<bool>,
        fail_submit: Cell<bool>,
        busy_after: Cell<Option<usize>>,
    }

    impl Transport for Fake {
        type Chunk = (u32, usize);
        type Owner = Rc<()>;
        type Receipt = Signal;
        type Signal = Signal;
        type Error = u32;
        fn size(chunk: &Self::Chunk) -> usize {
            chunk.1
        }
        fn ready(&self, _: &Self::Chunk) -> Result<bool, u32> {
            Ok(self.ready.get())
        }
        fn submit(&self, _: &Rc<()>, chunk: &Self::Chunk) -> Result<Signal, DispatchError<u32>> {
            if self.busy.get() {
                return Err(DispatchError::Busy);
            }
            if self
                .busy_after
                .get()
                .is_some_and(|limit| self.accepted.borrow().len() >= limit)
            {
                return Err(DispatchError::Busy);
            }
            if self.fail_submit.get() {
                return Err(DispatchError::Failed(7));
            }
            self.accepted.borrow_mut().push(chunk.0);
            let fence = Rc::new(Cell::new(None));
            self.fences.borrow_mut().push(Rc::clone(&fence));
            Ok(fence)
        }
        fn poll(&self, receipt: &Signal) -> Result<bool, u32> {
            receipt.get().transpose().map(|value| value.is_some())
        }
        fn complete(&self, signal: &Signal, result: Result<(), u32>) {
            signal.set(Some(result));
        }
    }

    fn add(queue: &mut Scheduler<Fake>, chunks: Vec<(u32, usize)>) -> Signal {
        let signal = Rc::new(Cell::new(None));
        queue
            .enqueue(chunks, Rc::new(()), Rc::clone(&signal))
            .unwrap();
        signal
    }

    fn ready() -> Fake {
        Fake {
            ready: Cell::new(true),
            ..Fake::default()
        }
    }

    #[test]
    fn large_logical_stream_is_admitted_once_then_dispatched_in_order() {
        let native = ready();
        let mut queue = Scheduler::new();
        let a = add(&mut queue, (0..40).map(|id| (id, 64 * 1024)).collect());
        let b = add(&mut queue, alloc::vec![(40, 0)]);
        assert!(Rc::ptr_eq(queue.last_signal().unwrap(), &b));
        assert!(native.accepted.borrow().is_empty());
        queue.advance(&native);
        assert_eq!(*native.accepted.borrow(), (0..16).collect::<Vec<_>>());
        assert_eq!(a.get(), None);
        for _ in 0..4 {
            for fence in native.fences.borrow().iter() {
                fence.set(Some(Ok(())));
            }
            queue.advance(&native);
        }
        assert_eq!(*native.accepted.borrow(), (0..41).collect::<Vec<_>>());
        assert_eq!(a.get(), Some(Ok(())));
        assert_eq!(b.get(), Some(Ok(())));
        assert!(queue.is_empty());
    }

    #[test]
    fn native_busy_and_dependency_pressure_never_replay_or_overtake() {
        let native = ready();
        let mut queue = Scheduler::new();
        add(&mut queue, alloc::vec![(1, 1), (2, 1)]);
        add(&mut queue, alloc::vec![(3, 1)]);
        native.busy.set(true);
        assert!(!queue.advance(&native));
        assert!(native.accepted.borrow().is_empty());
        native.busy.set(false);
        native.busy_after.set(Some(1));
        queue.advance(&native);
        assert_eq!(*native.accepted.borrow(), [1]);
        assert!(!queue.advance(&native));
        native.busy_after.set(None);
        native.ready.set(false);
        assert!(!queue.advance(&native));
        native.ready.set(true);
        queue.advance(&native);
        queue.advance(&native);
        assert_eq!(*native.accepted.borrow(), [1, 2, 3]);
    }

    #[test]
    fn dropped_receipt_and_owner_do_not_abandon_queued_work() {
        let native = ready();
        let mut queue = Scheduler::new();
        let owner = Rc::new(());
        let retained = Rc::downgrade(&owner);
        let signal = Rc::new(Cell::new(None));
        queue.enqueue(alloc::vec![(1, 1)], owner, signal).unwrap();
        assert!(retained.upgrade().is_some());
        queue.advance(&native);
        assert!(retained.upgrade().is_some());
        native.fences.borrow()[0].set(Some(Ok(())));
        queue.advance(&native);
        assert!(retained.upgrade().is_none());
    }

    #[test]
    fn byte_and_slot_pressure_are_side_effect_free_admission() {
        let mut queue = Scheduler::<Fake>::new();
        let signal = Rc::new(Cell::new(None));
        assert_eq!(
            queue.enqueue(
                alloc::vec![(1, MAX_PENDING_BYTES + 1)],
                Rc::new(()),
                Rc::clone(&signal)
            ),
            Err(AdmissionError::TooLarge)
        );
        add(&mut queue, alloc::vec![(1, MAX_PENDING_BYTES)]);
        assert_eq!(
            queue.enqueue(alloc::vec![(2, 1)], Rc::new(()), Rc::clone(&signal)),
            Err(AdmissionError::Busy)
        );
        let mut queue = Scheduler::<Fake>::new();
        for _ in 0..MAX_SUBMISSIONS {
            add(&mut queue, alloc::vec![(1, 0)]);
        }
        assert_eq!(
            queue.enqueue(alloc::vec![], Rc::new(()), signal),
            Err(AdmissionError::Busy)
        );
    }

    #[test]
    fn later_completion_does_not_certify_an_unfinished_prefix() {
        let native = ready();
        let mut queue = Scheduler::new();
        let a = add(&mut queue, alloc::vec![(1, 1)]);
        let b = add(&mut queue, alloc::vec![(2, 1)]);
        queue.advance(&native);
        native.fences.borrow()[1].set(Some(Ok(())));
        queue.advance(&native);
        assert_eq!((a.get(), b.get()), (None, None));
        native.fences.borrow()[0].set(Some(Ok(())));
        queue.advance(&native);
        assert_eq!((a.get(), b.get()), (Some(Ok(())), Some(Ok(()))));
    }

    #[test]
    fn later_completed_receipts_leave_wait_set_without_completing_job() {
        let native = ready();
        let mut queue = Scheduler::new();
        let signal = add(&mut queue, alloc::vec![(1, 1), (2, 1), (3, 1)]);
        queue.advance(&native);
        native.fences.borrow()[1].set(Some(Ok(())));
        native.fences.borrow()[2].set(Some(Ok(())));
        assert!(queue.advance(&native));
        assert_eq!(queue.receipts().count(), 1);
        assert!(Rc::ptr_eq(
            queue.receipts().next().unwrap(),
            &native.fences.borrow()[0]
        ));
        assert_eq!(signal.get(), None);
        assert!(!queue.advance(&native));
        native.fences.borrow()[0].set(Some(Ok(())));
        assert!(queue.advance(&native));
        assert_eq!(signal.get(), Some(Ok(())));
        assert!(queue.is_empty());
    }

    #[test]
    fn later_failed_receipt_poisoning_does_not_wait_for_first_receipt() {
        let native = ready();
        let mut queue = Scheduler::new();
        let a = add(&mut queue, alloc::vec![(1, 1), (2, 1)]);
        let b = add(&mut queue, alloc::vec![(3, 1)]);
        queue.advance(&native);
        native.fences.borrow()[1].set(Some(Err(9)));
        assert_eq!(native.fences.borrow()[0].get(), None);
        assert!(queue.advance(&native));
        assert_eq!((a.get(), b.get()), (Some(Err(9)), Some(Err(9))));
        assert_eq!(queue.failure(), Some(9));
        assert_eq!(queue.receipts().count(), 0);
        assert!(queue.is_empty());
    }

    #[test]
    fn late_dispatch_and_completion_failures_poison_all_following_work() {
        for dispatch in [false, true] {
            let native = ready();
            let mut queue = Scheduler::new();
            let a = add(&mut queue, (0..17).map(|id| (id, 1)).collect());
            let b = add(&mut queue, alloc::vec![(18, 1)]);
            queue.advance(&native);
            if dispatch {
                native.fences.borrow()[0].set(Some(Ok(())));
                native.fail_submit.set(true);
            } else {
                native.fences.borrow()[0].set(Some(Err(7)));
            }
            queue.advance(&native);
            assert_eq!((a.get(), b.get()), (Some(Err(7)), Some(Err(7))));
            assert_eq!(native.accepted.borrow().len(), 16);
            assert_eq!(queue.failure(), Some(7));
            assert!(queue.is_empty());
            assert_eq!(
                queue.enqueue(alloc::vec![], Rc::new(()), a),
                Err(AdmissionError::Failed(7))
            );
        }
    }
}
