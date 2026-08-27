// SPDX-License-Identifier: GPL-2.0-only

//! RAII allocations mapped into an A618 or GMU DMA context.

use core::ptr;

use scarlet::{
    arch,
    device::iommu::{DmaContext, DmaMapping, IommuMapFlags},
    environment::PAGE_SIZE,
    mem::page::ContiguousPages,
    vm::vmem::MemoryAttribute,
};

pub(crate) fn page_count(size: usize) -> Result<usize, &'static str> {
    size.checked_add(PAGE_SIZE - 1)
        .map(|value| value / PAGE_SIZE)
        .filter(|value| *value != 0)
        .ok_or("qcom-adreno-a618: allocation size overflows")
}

/// Contiguous kernel memory with an owned device mapping.
pub(crate) struct DmaAllocation {
    // Mapping drops before pages, ensuring no IOMMU PTE can outlive its memory.
    mapping: DmaMapping,
    pages: ContiguousPages,
    requested_size: usize,
}

impl DmaAllocation {
    pub(crate) fn new(
        context: &DmaContext,
        size: usize,
        flags: IommuMapFlags,
    ) -> Result<Self, &'static str> {
        Self::new_with_cpu_attribute(context, size, flags, MemoryAttribute::Normal)
    }

    /// Allocate a device-owned status page through a CPU non-cacheable alias.
    ///
    /// Linux maps the A6xx ring memptrs (including the completion fence) as
    /// `MSM_BO_WC`.  Normal-NC is the corresponding AArch64 memory type and is
    /// required for a word that the GPU repeatedly overwrites while the CPU
    /// polls it; a normal WB direct-map alias can retain a stale cache line
    /// across consecutive CACHE_FLUSH_TS events on this non-coherent device.
    pub(crate) fn new_cpu_noncacheable(
        context: &DmaContext,
        size: usize,
        flags: IommuMapFlags,
    ) -> Result<Self, &'static str> {
        Self::new_with_cpu_attribute(context, size, flags, MemoryAttribute::NonCacheable)
    }

    fn new_with_cpu_attribute(
        context: &DmaContext,
        size: usize,
        flags: IommuMapFlags,
        cpu_attribute: MemoryAttribute,
    ) -> Result<Self, &'static str> {
        let mut pages = ContiguousPages::new(page_count(size)?)
            .ok_or("qcom-adreno-a618: contiguous DMA allocation failed")?;
        let allocation_size = pages
            .len()
            .checked_mul(PAGE_SIZE)
            .ok_or("qcom-adreno-a618: allocation byte size overflows")?;
        // SAFETY: `pages` exclusively owns a writable contiguous allocation of
        // exactly `allocation_size` bytes.
        unsafe { ptr::write_bytes(pages.as_vaddr() as *mut u8, 0, allocation_size) };
        arch::clean_dcache_to_poc_range(pages.as_vaddr(), allocation_size);
        if cpu_attribute != MemoryAttribute::Normal {
            pages
                .retag_memory_attribute(cpu_attribute)
                .map_err(|_| "qcom-adreno-a618: failed to retag DMA allocation")?;
        }
        let mapping = context
            .map_phys_owned(pages.as_paddr(), allocation_size, flags)
            .map_err(|_| "qcom-adreno-a618: IOMMU mapping failed")?;
        Ok(Self {
            mapping,
            pages,
            requested_size: size,
        })
    }

    pub(crate) fn dma_addr(&self) -> u64 {
        self.mapping.dma_addr()
    }

    pub(crate) fn paddr(&self) -> usize {
        self.pages.as_paddr()
    }

    pub(crate) fn vaddr(&self) -> usize {
        self.pages.as_vaddr()
    }

    pub(crate) fn requested_size(&self) -> usize {
        self.requested_size
    }

    pub(crate) fn allocation_size(&self) -> usize {
        self.pages.len() * PAGE_SIZE
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        // SAFETY: the allocation remains owned by `self`; the requested slice
        // is bounded by the page-rounded allocation.
        unsafe { core::slice::from_raw_parts(self.vaddr() as *const u8, self.requested_size) }
    }

    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `&mut self` grants exclusive access to the live allocation.
        unsafe { core::slice::from_raw_parts_mut(self.vaddr() as *mut u8, self.requested_size) }
    }

    pub(crate) fn as_words(&self) -> &[u32] {
        let len = self.requested_size / core::mem::size_of::<u32>();
        // SAFETY: PMM pages are page aligned, and `len` stays inside the
        // requested byte range.
        unsafe { core::slice::from_raw_parts(self.vaddr() as *const u32, len) }
    }

    pub(crate) fn as_words_mut(&mut self) -> &mut [u32] {
        let len = self.requested_size / core::mem::size_of::<u32>();
        // SAFETY: same bounds/alignment as `as_words`, with exclusive access.
        unsafe { core::slice::from_raw_parts_mut(self.vaddr() as *mut u32, len) }
    }

    /// Read a device-owned word after the caller has invalidated the CPU
    /// cache.  Volatile access is required because the device can update this
    /// allocation without a Rust-visible store.
    pub(crate) fn read_word_volatile(&self, index: usize) -> Option<u32> {
        let len = self.requested_size / core::mem::size_of::<u32>();
        if index >= len {
            return None;
        }
        // SAFETY: `index` was checked against the requested allocation and the
        // page backing is naturally aligned for `u32`.
        Some(unsafe { ptr::read_volatile((self.vaddr() as *const u32).add(index)) })
    }

    pub(crate) fn clean_for_device(&self) {
        arch::clean_dcache_to_poc_range(self.vaddr(), self.allocation_size());
    }

    pub(crate) fn invalidate_from_device(&self) {
        arch::invalidate_dcache_to_poc_range(self.vaddr(), self.allocation_size());
    }
}

pub(crate) fn bidirectional_flags() -> IommuMapFlags {
    IommuMapFlags::READ | IommuMapFlags::WRITE
}
