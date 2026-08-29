// SPDX-License-Identifier: GPL-2.0-only

//! DMA allocation helpers for Venus HFI and decode buffers.

use core::ptr;

use scarlet::{
    arch,
    device::iommu::{DmaContext, DmaMapping, IommuMapFlags},
    environment::PAGE_SIZE,
    mem::page::ContiguousPages,
    vm::vmem::MemoryAttribute,
};

fn page_count(size: usize) -> Result<usize, &'static str> {
    size.checked_add(PAGE_SIZE - 1)
        .map(|value| value / PAGE_SIZE)
        .filter(|value| *value != 0)
        .ok_or("qcom-venus-sc7180: DMA allocation size overflows")
}

/// Contiguous kernel memory with an owned Venus IOMMU mapping.
pub(crate) struct DmaAllocation {
    // The mapping must drop before the backing pages.
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
        Self::new_with_attribute(context, size, flags, MemoryAttribute::Normal)
    }

    pub(crate) fn new_noncacheable(
        context: &DmaContext,
        size: usize,
        flags: IommuMapFlags,
    ) -> Result<Self, &'static str> {
        Self::new_with_attribute(context, size, flags, MemoryAttribute::NonCacheable)
    }

    fn new_with_attribute(
        context: &DmaContext,
        size: usize,
        flags: IommuMapFlags,
        memory_attribute: MemoryAttribute,
    ) -> Result<Self, &'static str> {
        let mut pages = ContiguousPages::new(page_count(size)?)
            .ok_or("qcom-venus-sc7180: contiguous DMA allocation failed")?;
        let allocation_size = pages
            .len()
            .checked_mul(PAGE_SIZE)
            .ok_or("qcom-venus-sc7180: DMA allocation length overflows")?;
        // SAFETY: `pages` uniquely owns a writable allocation of exactly
        // `allocation_size` bytes.
        unsafe { ptr::write_bytes(pages.as_vaddr() as *mut u8, 0, allocation_size) };
        arch::clean_dcache_to_poc_range(pages.as_vaddr(), allocation_size);
        pages
            .retag_memory_attribute(memory_attribute)
            .map_err(|_| "qcom-venus-sc7180: failed to retag DMA allocation")?;
        let mapping = context
            .map_phys_owned(pages.as_paddr(), allocation_size, flags)
            .map_err(|_| "qcom-venus-sc7180: DMA mapping failed")?;
        if mapping.dma_addr() > u32::MAX as u64 {
            return Err("qcom-venus-sc7180: firmware requires a 32-bit DMA address");
        }
        Ok(Self {
            mapping,
            pages,
            requested_size: size,
        })
    }

    pub(crate) fn dma_addr(&self) -> u32 {
        self.mapping.dma_addr() as u32
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

    pub(crate) fn clean(&self, offset: usize, len: usize) -> Result<(), &'static str> {
        let end = offset
            .checked_add(len)
            .ok_or("qcom-venus-sc7180: cache clean range overflows")?;
        if end > self.allocation_size() {
            return Err("qcom-venus-sc7180: cache clean range is out of bounds");
        }
        if len != 0 && self.pages.memory_attribute() == MemoryAttribute::Normal {
            arch::clean_dcache_to_poc_range(self.vaddr() + offset, len);
        }
        arch::io_mb();
        Ok(())
    }

    pub(crate) fn invalidate(&self, offset: usize, len: usize) -> Result<(), &'static str> {
        let end = offset
            .checked_add(len)
            .ok_or("qcom-venus-sc7180: cache invalidate range overflows")?;
        if end > self.allocation_size() {
            return Err("qcom-venus-sc7180: cache invalidate range is out of bounds");
        }
        if len != 0 && self.pages.memory_attribute() == MemoryAttribute::Normal {
            arch::invalidate_dcache_to_poc_range(self.vaddr() + offset, len);
        }
        arch::io_mb();
        Ok(())
    }

    pub(crate) fn zero(&mut self) {
        // SAFETY: `&mut self` grants exclusive access to the full live page
        // allocation.
        unsafe { ptr::write_bytes(self.vaddr() as *mut u8, 0, self.allocation_size()) };
        if self.pages.memory_attribute() == MemoryAttribute::Normal {
            arch::clean_dcache_to_poc_range(self.vaddr(), self.allocation_size());
        }
        arch::io_mb();
    }
}

pub(crate) fn rw_flags() -> IommuMapFlags {
    IommuMapFlags::READ | IommuMapFlags::WRITE
}
