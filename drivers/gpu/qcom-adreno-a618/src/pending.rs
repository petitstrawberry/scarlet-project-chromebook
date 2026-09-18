// SPDX-License-Identifier: GPL-2.0-only

//! Allocation-free admission and FIFO scheduling after queue construction.

use alloc::collections::VecDeque;

/// Counts all accepted work, including the value held by the worker and any
/// values quarantined by that worker because DMA retirement was not proven.
///
/// The caller supplies synchronization. Moving a value out with `start_next`
/// transfers ownership to the worker without releasing its admission slot.
/// The worker must retain a quarantined value for as long as DMA may reach it;
/// `finish(false)` preserves the slot but does not take ownership of that value.
pub(crate) struct PendingQueue<T> {
    queued: VecDeque<T>,
    outstanding: usize,
    running: bool,
    capacity: usize,
    cpu_access: bool,
}

impl<T> PendingQueue<T> {
    pub(crate) fn try_new(capacity: usize) -> Result<Self, &'static str> {
        if capacity == 0 {
            return Err("qcom-adreno-a618: pending queue capacity must be nonzero");
        }
        let mut queued = VecDeque::new();
        queued
            .try_reserve_exact(capacity)
            .map_err(|_| "qcom-adreno-a618: pending queue allocation failed")?;
        Ok(Self {
            queued,
            outstanding: 0,
            running: false,
            capacity,
            cpu_access: false,
        })
    }

    pub(crate) fn has_capacity(&self) -> bool {
        !self.cpu_access && self.has_slot()
    }

    fn has_slot(&self) -> bool {
        self.outstanding < self.capacity
    }

    /// Reserve the device for a managed CPU access. Already accepted work may
    /// still drain, but ordinary admission stays blocked until the owner ends
    /// the reservation after its checkpoint and CPU memory access complete.
    pub(crate) fn try_begin_cpu_access(&mut self) -> bool {
        if self.cpu_access {
            return false;
        }
        self.cpu_access = true;
        true
    }

    pub(crate) fn end_cpu_access(&mut self) {
        self.cpu_access = false;
    }

    /// Rejection returns the original value without allocating or dropping it.
    /// Accepted insertion cannot allocate: queued work never exceeds the full
    /// admission capacity reserved during construction.
    pub(crate) fn push(&mut self, value: T) -> Result<(), T> {
        if !self.has_capacity() {
            return Err(value);
        }
        self.push_admitted(value);
        Ok(())
    }

    /// Admit the reservation owner's checkpoint behind previously accepted
    /// work. The CPU reservation does not waive the outstanding-work ceiling.
    /// The caller must ensure only the reservation owner uses this method.
    pub(crate) fn push_cpu_checkpoint(&mut self, value: T) -> Result<(), T> {
        if !self.cpu_access || !self.has_slot() {
            return Err(value);
        }
        self.push_admitted(value);
        Ok(())
    }

    fn push_admitted(&mut self, value: T) {
        self.queued.push_back(value);
        self.outstanding += 1;
    }

    pub(crate) fn start_next(&mut self) -> Option<T> {
        if self.running {
            return None;
        }
        let value = self.queued.pop_front()?;
        self.running = true;
        Some(value)
    }

    /// Ends the active scheduling turn. Only proven DMA retirement releases
    /// capacity; an unretired operation permanently occupies its slot until
    /// this queue is replaced after a safe hardware reset or shutdown.
    pub(crate) fn finish(&mut self, retired: bool) {
        if self.running {
            self.running = false;
            if retired {
                self.outstanding -= 1;
            }
        }
    }
}

/// A generic interrupt cannot retire a particular submission. The DMA fence
/// must match its nonzero sequence, corroborated by the scratch register, the
/// consumed ring pointer and hardware idle state.
pub(crate) fn fence_retired(
    expected: u32,
    dma_fence: u32,
    scratch: u32,
    read_pointer: u32,
    target_pointer: u32,
    idle: bool,
) -> bool {
    expected != 0
        && dma_fence == expected
        && scratch == expected
        && read_pointer == target_pointer
        && idle
}
