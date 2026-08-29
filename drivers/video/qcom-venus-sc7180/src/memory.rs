// SPDX-License-Identifier: GPL-2.0-only

//! DMA allocation helpers for Venus HFI and decode buffers.

use alloc::vec::Vec;
use core::ptr;

use scarlet::{
    arch,
    device::iommu::{DmaContext, DmaMapping, IommuMapFlags},
    environment::PAGE_SIZE,
    mem::page::ContiguousPages,
    vm::vmem::MemoryAttribute,
};

const MAX_SCATTER_CHUNK_PAGES: usize = 64;

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
}

impl DmaAllocation {
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
        Ok(Self { mapping, pages })
    }

    pub(crate) fn dma_addr(&self) -> u32 {
        self.mapping.dma_addr() as u32
    }

    pub(crate) fn vaddr(&self) -> usize {
        self.pages.as_vaddr()
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

/// Page-backed memory exposed to Venus through one contiguous IOVA range.
///
/// Decode scratch and DPB buffers do not need a contiguous CPU virtual
/// address. Allocating them in bounded physical chunks avoids exhausting the
/// PMM's high-order blocks while the SMMU still presents the linear address
/// range required by HFI.
pub(crate) struct DmaPagedAllocation {
    // The mapping must drop before the backing chunks.
    mapping: DmaMapping,
    _chunks: Vec<ContiguousPages>,
    requested_size: usize,
}

impl DmaPagedAllocation {
    pub(crate) fn new(
        context: &DmaContext,
        size: usize,
        flags: IommuMapFlags,
    ) -> Result<Self, &'static str> {
        let mut remaining_pages = page_count(size)?;
        let mut chunks = Vec::new();
        let mut segments = Vec::new();

        while remaining_pages != 0 {
            let mut chunk_pages = remaining_pages.min(MAX_SCATTER_CHUNK_PAGES);
            let chunk = loop {
                if let Some(chunk) = ContiguousPages::new(chunk_pages) {
                    break chunk;
                }
                if chunk_pages == 1 {
                    return Err("qcom-venus-sc7180: paged DMA allocation failed");
                }
                chunk_pages = (chunk_pages / 2).max(1);
            };
            let chunk_len = chunk
                .len()
                .checked_mul(PAGE_SIZE)
                .ok_or("qcom-venus-sc7180: paged DMA length overflows")?;
            arch::clean_dcache_to_poc_range(chunk.as_vaddr(), chunk_len);
            segments.push((chunk.as_paddr(), chunk_len));
            remaining_pages -= chunk.len();
            chunks.push(chunk);
        }
        arch::io_mb();

        let mapping = context
            .map_phys_segments_owned(&segments, flags)
            .map_err(|_| "qcom-venus-sc7180: paged DMA mapping failed")?;
        if mapping.dma_addr() > u32::MAX as u64 {
            return Err("qcom-venus-sc7180: firmware requires a 32-bit DMA address");
        }

        Ok(Self {
            mapping,
            _chunks: chunks,
            requested_size: size,
        })
    }

    pub(crate) fn dma_addr(&self) -> u32 {
        self.mapping.dma_addr() as u32
    }

    pub(crate) fn requested_size(&self) -> usize {
        self.requested_size
    }
}

pub(crate) fn rw_flags() -> IommuMapFlags {
    IommuMapFlags::READ | IommuMapFlags::WRITE
}
