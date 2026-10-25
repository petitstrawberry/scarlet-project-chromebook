// SPDX-License-Identifier: GPL-2.0-only
//! Immutable NV12 snapshots. A lease owns its pages until the last GPU/user
//! handle is released; the reusable frontend decode buffer never escapes.
use alloc::{sync::Arc, vec::Vec};
use scarlet::{
    arch,
    device::graphics::{GpuBackingSegment, shared_image::*},
    mem::page::ContiguousPages,
};

struct Frame {
    descriptor: SharedImageDescriptor,
    segments: Arc<[GpuBackingSegment]>,
    _pages: Vec<ContiguousPages>,
}
// SAFETY: all pages are initialized and cleaned before publication. Neither
// the decoder nor userspace has a writable reference to the snapshot.
unsafe impl SharedImageBacking for Frame {
    fn descriptor(&self) -> SharedImageDescriptor {
        self.descriptor
    }
    fn buffer_segments(&self, index: usize) -> Arc<[GpuBackingSegment]> {
        if index == 0 {
            self.segments.clone()
        } else {
            Arc::from([])
        }
    }
}

/// Caller supplies a completed, cache-invalidated, bounds-checked linear frame.
pub(super) fn snapshot(
    source: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    source_pitch: u32,
    source_uv: usize,
) -> Result<Arc<SharedImage>, &'static str> {
    let pitch = width.checked_add(127).ok_or("NV12 pitch overflow")? & !127;
    let y_size = u64::from(pitch) * u64::from(height);
    let uv_offset = (y_size + 4095) & !4095;
    let uv_size = u64::from(pitch) * u64::from(height.div_ceil(2));
    let size =
        usize::try_from((uv_offset + uv_size + 4095) & !4095).map_err(|_| "NV12 size overflow")?;
    let mut pages = Vec::new();
    let mut segments = Vec::new();
    let mut base = 0usize;
    while base < size {
        let count = ((size - base) / 4096).min(64);
        let chunk = ContiguousPages::new(count).ok_or("NV12 snapshot allocation failed")?;
        let bytes = count * 4096;
        // SAFETY: this allocation is uniquely owned and not yet published.
        let dest = unsafe { core::slice::from_raw_parts_mut(chunk.as_vaddr() as *mut u8, bytes) };
        dest.fill(0);
        for (start, rows, src_start) in [
            (
                0usize,
                height,
                y as usize * source_pitch as usize + x as usize,
            ),
            (
                uv_offset as usize,
                height.div_ceil(2),
                source_uv + (y / 2) as usize * source_pitch as usize + x as usize,
            ),
        ] {
            for row in 0..rows as usize {
                let dst_row = start + row * pitch as usize;
                let lo = base.max(dst_row);
                let hi = (base + bytes).min(dst_row + width as usize);
                if lo < hi {
                    let src = src_start + row * source_pitch as usize + lo - dst_row;
                    dest[lo - base..hi - base].copy_from_slice(
                        source
                            .get(src..src + hi - lo)
                            .ok_or("NV12 crop exceeds source")?,
                    );
                }
            }
        }
        arch::clean_dcache_to_poc_range(chunk.as_vaddr(), bytes);
        segments.push(GpuBackingSegment::new(chunk.as_paddr(), bytes));
        pages.push(chunk);
        base += bytes;
    }
    let mut descriptor = SharedImageDescriptor {
        version: SHARED_IMAGE_ABI_VERSION,
        format: IMAGE_FORMAT_NV12,
        width,
        height,
        visible: ImageRect {
            x: 0,
            y: 0,
            width,
            height,
        },
        buffer_count: 1,
        plane_count: 2,
        buffer_sizes: [size as u64, 0, 0, 0],
        ..Default::default()
    };
    descriptor.planes[0] = SharedImagePlane {
        row_pitch: pitch,
        size: y_size,
        ..Default::default()
    };
    descriptor.planes[1] = SharedImagePlane {
        row_pitch: pitch,
        offset: uv_offset,
        size: uv_size,
        ..Default::default()
    };
    SharedImage::new(Arc::new(Frame {
        descriptor,
        segments: segments.into(),
        _pages: pages,
    }))
    .map(Arc::new)
}
