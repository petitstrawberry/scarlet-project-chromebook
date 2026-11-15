// SPDX-License-Identifier: GPL-2.0-only
//! CPU address translation for uncompressed A618 TILE6_3 D32 storage.
//! Mesa fd6_tiled_memcpy.cc describes 16x4 pixel blocks, 4-channel
//! macrotiles, HBB=14 and bank-swizzle levels 2/3 for A618. Linux a6xx_gpu.c
//! deliberately retains A618's hardware defaults for this configuration.

pub(super) struct DepthReadback {
    width: u32,
    height: u32,
    pitch: u32,
    size: u64,
}

impl DepthReadback {
    pub(super) fn new(width: u32, height: u32, pitch: u32, size: u64) -> Option<Self> {
        adreno_a6xx_layout::is_depth32_tile6_3_layout(width, height, pitch, size).then_some(Self {
            width,
            height,
            pitch,
            size,
        })
    }

    /// A bounded run of one or two horizontally adjacent 32-bit texels.
    /// Offsets refer to the retained original backing, never a linear alias.
    pub(super) fn span(&self, x: u32, y: u32, width: u32) -> Option<(u64, u32)> {
        if width == 0 || x.checked_add(width)? > self.width || y >= self.height {
            return None;
        }
        let (bx, by) = (u64::from(x / 16), u64::from(y / 4));
        let macro_stride = u64::from(self.pitch / 256);
        let x_mask = (((bx & 1) * 7) ^ (bx & 2) ^ ((bx >> 1) << 3)) << 8;
        let bank_mask =
            (u64::from(macro_stride % 4 == 0) | (u64::from(macro_stride % 8 == 0) << 1)) << 2;
        let y_mask = ((((by & 1) * 6) ^ (((by >> 1) & 1) * 3)) << 8) | ((by & bank_mask) << 11);
        let (px, py) = (u64::from(x & 15), u64::from(y & 3));
        let pixel =
            (px & 1) | ((py & 1) << 1) | ((px & 2) << 1) | ((py & 2) << 2) | ((px & 12) << 2);
        let offset = (by >> 2) * macro_stride * 4096 + (x_mask ^ y_mask) + pixel * 4;
        let count = width.min(2 - (x & 1));
        (offset.checked_add(u64::from(count) * 4)? <= self.size).then_some((offset, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;

    #[test]
    fn rejects_noncanonical_layout_and_out_of_bounds_spans() {
        assert!(DepthReadback::new(73, 19, 512, 16384).is_some());
        assert!(DepthReadback::new(73, 19, 292, 5548).is_none());
        assert!(DepthReadback::new(73, 19, 512, 8192).is_none());
        let layout = DepthReadback::new(73, 19, 512, 16384).unwrap();
        for (x, y, n) in [
            (73, 0, 1),
            (0, 19, 1),
            (72, 0, 2),
            (0, 0, 0),
            (u32::MAX, 0, 2),
        ] {
            assert!(layout.span(x, y, n).is_none());
        }
    }

    #[test]
    fn mesa_microtile_and_macrotile_vectors() {
        let layout = DepthReadback::new(512, 80, 2048, 163840).unwrap();
        // Fixed coordinate vectors from the Mesa block/pixel address equations.
        for (x, y, offset) in [
            (0, 0, 0),
            (1, 0, 4),
            (2, 0, 16),
            (0, 1, 8),
            (15, 3, 252),
            (16, 0, 1792),
            (32, 0, 2560),
            (0, 4, 1536),
            (0, 8, 768),
            (0, 16, 40960),
            (0, 32, 81920),
            (0, 64, 131072),
        ] {
            assert_eq!(layout.span(x, y, 1), Some((offset, 1)), "{x},{y}");
        }
    }

    #[test]
    fn spans_pack_odd_rectangles_without_aliases_or_padding_reads() {
        for (width, height) in [(73, 19), (256, 80), (512, 80), (2200, 152)] {
            let physical = adreno_a6xx_layout::depth32_tile6_3_layout(width, height).unwrap();
            let layout =
                DepthReadback::new(width, height, physical.row_pitch, physical.layer_size).unwrap();
            let mut seen = vec![false; physical.layer_size as usize / 4];
            for y in 0..height {
                let mut x = 0;
                while x < width {
                    let (offset, count) = layout.span(x, y, width - x).unwrap();
                    assert_eq!(offset & 3, 0);
                    for i in 0..count {
                        let slot = offset as usize / 4 + i as usize;
                        assert!(!seen[slot], "aliased texel {x},{y}");
                        seen[slot] = true;
                        assert_eq!(
                            layout.span(x + i, y, 1).unwrap().0,
                            offset + u64::from(i) * 4
                        );
                    }
                    x += count;
                }
            }
            assert_eq!(
                seen.iter().filter(|v| **v).count(),
                (width * height) as usize
            );
        }
    }
}
