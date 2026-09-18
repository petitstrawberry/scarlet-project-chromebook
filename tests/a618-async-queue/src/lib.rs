// SPDX-License-Identifier: GPL-2.0-only

//! Host tests compile the exact ownership and fence logic used by the driver.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

#[path = "../../../drivers/gpu/qcom-adreno-a618/src/pending.rs"]
mod pending;

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::pending::{PendingQueue, fence_retired};

    #[derive(Debug)]
    struct Resource {
        identity: usize,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Resource {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn resource(identity: usize, drops: &Arc<AtomicUsize>) -> Arc<Resource> {
        Arc::new(Resource {
            identity,
            drops: Arc::clone(drops),
        })
    }

    #[test]
    fn queue_rejects_invalid_or_unallocatable_capacity() {
        assert!(PendingQueue::<u32>::try_new(0).is_err());
        assert!(PendingQueue::<u32>::try_new(usize::MAX).is_err());
    }

    #[test]
    fn running_work_still_consumes_capacity() {
        let mut queue = PendingQueue::try_new(1).unwrap();
        assert!(queue.has_capacity());
        queue.push(7).unwrap();
        assert_eq!(queue.start_next(), Some(7));
        assert!(!queue.has_capacity());
        assert_eq!(queue.push(8), Err(8));
        queue.finish(true);
        assert!(queue.has_capacity());
        queue.push(8).unwrap();
        assert_eq!(queue.start_next(), Some(8));
    }

    #[test]
    fn checkpoints_cannot_overtake_earlier_work() {
        #[derive(Debug, PartialEq)]
        enum Work {
            Submit(u32),
            Checkpoint,
        }

        let mut queue = PendingQueue::try_new(3).unwrap();
        queue.push(Work::Submit(10)).unwrap();
        queue.push(Work::Checkpoint).unwrap();
        queue.push(Work::Submit(11)).unwrap();
        assert_eq!(queue.start_next(), Some(Work::Submit(10)));
        assert_eq!(queue.start_next(), None);
        queue.finish(true);
        assert_eq!(queue.start_next(), Some(Work::Checkpoint));
        assert_eq!(queue.start_next(), None);
        queue.finish(true);
        assert_eq!(queue.start_next(), Some(Work::Submit(11)));
        queue.finish(true);
        assert_eq!(queue.start_next(), None);
    }

    #[test]
    fn busy_returns_exact_ownership_without_dropping_resources() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut queue = PendingQueue::try_new(1).unwrap();
        queue.push(resource(1, &drops)).unwrap();
        let rejected = resource(2, &drops);
        let identity = Arc::as_ptr(&rejected);
        let returned = queue.push(rejected).unwrap_err();
        assert_eq!(Arc::as_ptr(&returned), identity);
        assert_eq!(Arc::strong_count(&returned), 1);
        assert_eq!(returned.identity, 2);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let running = queue.start_next().unwrap();
        assert_eq!(running.identity, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        queue.finish(true);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(returned);
        drop(running);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn accepted_resources_survive_observer_drop_through_retirement() {
        let drops = Arc::new(AtomicUsize::new(0));
        let observer = resource(1, &drops);
        let mut queue = PendingQueue::try_new(1).unwrap();
        queue.push(Arc::clone(&observer)).unwrap();
        drop(observer);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let running = queue.start_next().unwrap();
        assert_eq!(Arc::strong_count(&running), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        queue.finish(true);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(running);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn each_retirement_returns_only_its_own_slot() {
        let mut queue = PendingQueue::try_new(2).unwrap();
        queue.push(1).unwrap();
        queue.push(2).unwrap();
        assert_eq!(queue.start_next(), Some(1));
        queue.finish(true);
        queue.push(3).unwrap();
        assert_eq!(queue.push(4), Err(4));
        assert_eq!(queue.start_next(), Some(2));
        queue.finish(true);
        queue.push(4).unwrap();
        assert_eq!(queue.push(5), Err(5));
        assert_eq!(queue.start_next(), Some(3));
        queue.finish(true);
        assert_eq!(queue.start_next(), Some(4));
        queue.finish(true);
        assert!(queue.has_capacity());
    }

    #[test]
    fn quarantine_retains_capacity_while_later_work_drains() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut queue = PendingQueue::try_new(2).unwrap();
        queue.push(resource(1, &drops)).unwrap();
        queue.push(resource(2, &drops)).unwrap();
        let quarantined = queue.start_next().unwrap();
        queue.finish(false);
        assert!(!queue.has_capacity());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let next = queue.start_next().unwrap();
        assert_eq!(next.identity, 2);
        queue.finish(true);
        assert!(queue.has_capacity());
        queue.push(resource(3, &drops)).unwrap();
        assert!(!queue.has_capacity());
        assert_eq!(quarantined.identity, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(next);
        drop(queue);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        // A real worker keeps this reference until reset/shutdown proves that
        // the hardware can no longer reach the quarantined resources.
        drop(quarantined);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn idle_finish_cannot_release_queued_or_quarantined_slots() {
        let mut queue = PendingQueue::try_new(1).unwrap();
        queue.finish(true);
        queue.push(1).unwrap();
        queue.finish(true);
        assert!(!queue.has_capacity());
        assert_eq!(queue.start_next(), Some(1));
        queue.finish(false);
        queue.finish(true);
        assert!(!queue.has_capacity());
        assert_eq!(queue.start_next(), None);
        assert_eq!(queue.push(2), Err(2));
    }

    #[test]
    fn cpu_reservation_drains_earlier_work_before_its_checkpoint() {
        let mut queue = PendingQueue::try_new(4).unwrap();
        queue.push(1).unwrap();
        queue.push(2).unwrap();
        assert_eq!(queue.start_next(), Some(1));
        assert!(queue.try_begin_cpu_access());
        assert!(!queue.try_begin_cpu_access());
        // A spare slot exists, but ordinary work cannot enter the CPU access
        // interval. Only its ordered checkpoint may be admitted.
        assert!(!queue.has_capacity());
        assert_eq!(queue.push(3), Err(3));
        queue.push_cpu_checkpoint(99).unwrap();
        assert_eq!(queue.start_next(), None);
        queue.finish(true);
        assert_eq!(queue.start_next(), Some(2));
        queue.finish(true);
        assert_eq!(queue.start_next(), Some(99));
        queue.finish(true);
        // Fence retirement authorizes the CPU copy, not new GPU admission.
        assert!(!queue.has_capacity());
        assert_eq!(queue.push(3), Err(3));
        assert_eq!(queue.start_next(), None);
        queue.end_cpu_access();
        assert!(queue.has_capacity());
        queue.push(3).unwrap();
        assert_eq!(queue.start_next(), Some(3));
    }

    #[test]
    fn cpu_checkpoint_full_returns_exact_ownership_until_prior_work_retires() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut queue = PendingQueue::try_new(1).unwrap();
        queue.push(resource(1, &drops)).unwrap();
        let running = queue.start_next().unwrap();
        assert!(queue.try_begin_cpu_access());
        let checkpoint = resource(2, &drops);
        let identity = Arc::as_ptr(&checkpoint);
        let checkpoint = queue.push_cpu_checkpoint(checkpoint).unwrap_err();
        assert_eq!(Arc::as_ptr(&checkpoint), identity);
        assert_eq!(Arc::strong_count(&checkpoint), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        queue.finish(true);
        queue.push_cpu_checkpoint(checkpoint).unwrap();
        let checkpoint = queue.start_next().unwrap();
        assert_eq!(Arc::as_ptr(&checkpoint), identity);
        queue.finish(true);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(!queue.has_capacity());
        queue.end_cpu_access();
        assert!(queue.has_capacity());
        drop((running, checkpoint));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cpu_checkpoint_requires_a_live_reservation() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut queue = PendingQueue::try_new(1).unwrap();
        let checkpoint = resource(1, &drops);
        let identity = Arc::as_ptr(&checkpoint);
        let checkpoint = queue.push_cpu_checkpoint(checkpoint).unwrap_err();
        assert_eq!(Arc::as_ptr(&checkpoint), identity);
        assert!(queue.has_capacity());
        assert_eq!(queue.start_next().map(|value| value.identity), None);
        assert!(queue.try_begin_cpu_access());
        queue.end_cpu_access();
        let checkpoint = queue.push_cpu_checkpoint(checkpoint).unwrap_err();
        assert_eq!(Arc::as_ptr(&checkpoint), identity);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        queue.push(checkpoint).unwrap();
        assert!(!queue.has_capacity());
        assert_eq!(queue.start_next().unwrap().identity, 1);
        queue.finish(true);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ending_failed_cpu_access_restores_only_unoccupied_capacity() {
        let mut queue = PendingQueue::try_new(2).unwrap();
        queue.push(1).unwrap();
        assert_eq!(queue.start_next(), Some(1));
        queue.finish(false);
        assert!(queue.try_begin_cpu_access());
        queue.push_cpu_checkpoint(2).unwrap();
        assert_eq!(queue.push_cpu_checkpoint(3), Err(3));
        // Abandoning the CPU operation opens ordinary admission again, while
        // preserving accepted checkpoint and quarantined-work accounting.
        queue.end_cpu_access();
        assert!(!queue.has_capacity());
        assert_eq!(queue.start_next(), Some(2));
        queue.finish(true);
        assert!(queue.has_capacity());
        assert!(queue.try_begin_cpu_access());
        // Simulate failure before the checkpoint can be constructed.
        queue.end_cpu_access();
        assert!(queue.has_capacity());
        queue.push(4).unwrap();
        assert!(!queue.has_capacity());
        assert_eq!(queue.push(5), Err(5));
    }

    #[test]
    fn exact_dma_sequence_and_all_corroboration_prove_retirement() {
        assert!(fence_retired(42, 42, 42, 16, 16, true));
        assert!(!fence_retired(42, 42, 41, 16, 16, true));
        assert!(!fence_retired(42, 42, 42, 15, 16, true));
        assert!(!fence_retired(42, 42, 42, 16, 16, false));
    }

    #[test]
    fn stale_or_future_dma_sequence_cannot_be_replaced_by_other_signals() {
        // IRQ status is deliberately absent from the predicate. Even fully
        // matching scratch/pointer/idle signals cannot replace the DMA fence.
        assert!(!fence_retired(42, 0, 42, 16, 16, true));
        assert!(!fence_retired(42, 41, 42, 16, 16, true));
        assert!(!fence_retired(42, 43, 42, 16, 16, true));
    }

    #[test]
    fn sequence_wrap_requires_a_nonzero_exact_match() {
        assert!(fence_retired(u32::MAX, u32::MAX, u32::MAX, 0, 0, true));
        assert!(!fence_retired(0, 0, 0, 0, 0, true));
        assert!(!fence_retired(1, 0, 1, 0, 0, true));
        assert!(!fence_retired(1, u32::MAX, 1, 0, 0, true));
        assert!(fence_retired(1, 1, 1, 0, 0, true));
    }
}
