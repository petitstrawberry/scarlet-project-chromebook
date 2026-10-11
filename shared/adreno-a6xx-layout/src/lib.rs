//! Shared immutable image-layout rules for Adreno A6xx backends.
//!
//! These values form the private contract between the CoachZ kernel backend,
//! SGFX userspace, and the command validator.  Keeping the layout calculation
//! here prevents any one layer from silently treating tiled depth memory as a
//! linear image.

#![no_std]

/// Backend-specific modifier for an uncompressed A6xx TILE6_3 D32 image.
///
/// The value is the little-endian ASCII tag `A6D3TILE`.  It is intentionally
/// distinct from the backend-neutral linear modifier (`0`).
pub const IMAGE_MODIFIER_TILE6_3_DEPTH: u64 = u64::from_le_bytes(*b"A6D3TILE");

/// Private uncompressed TILE6_3 color layout. Tiled CCU storage uses native
/// channel ordering; texture views must use WZYX, as in Mesa fd6_pipe2swap.
pub const IMAGE_MODIFIER_TILE6_3_COLOR: u64 = u64::from_le_bytes(*b"A6C3TILE");

/// Hardware tile-mode value used for A6xx depth/stencil surfaces.
pub const TILE_MODE_3: u32 = 3;
/// Byte alignment of a 32-bit TILE6_3 row pitch.
pub const DEPTH32_PITCH_ALIGNMENT: u32 = 256;
/// Row alignment of a 32-bit TILE6_3 level.
pub const DEPTH32_HEIGHT_ALIGNMENT: u32 = 16;
/// Byte alignment of the base and layer size of a TILE6_3 image.
pub const DEPTH32_LAYER_ALIGNMENT: u64 = 4096;

/// Complete single-level, single-layer D32 TILE6_3 layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Depth32Tile6Layout {
    /// Bytes between adjacent hardware block rows.
    pub row_pitch: u32,
    /// Height rounded to the hardware tile height.
    pub padded_height: u32,
    /// Page-aligned byte footprint of the level/layer.
    pub layer_size: u64,
}

const fn align_up_u32(value: u32, alignment: u32) -> Option<u32> {
    match value.checked_add(alignment - 1) {
        Some(value) => Some(value & !(alignment - 1)),
        None => None,
    }
}

const fn align_up_u64(value: u64, alignment: u64) -> Option<u64> {
    match value.checked_add(alignment - 1) {
        Some(value) => Some(value & !(alignment - 1)),
        None => None,
    }
}

/// A6xx linear texture storage is layer-first: each layer contains its entire
/// mip chain. Pitches halve from the base pitch, rounded to 64 bytes; only the
/// final level needs four rows of padding. Layers start on 4 KiB boundaries.
/// See Mesa `src/freedreno/fdl/fd6_layout.c` and `fdl_pitch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinearTextureLayout {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub row_pitch: u32,
    pub layer_stride: u32,
    pub total_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinearSubresource {
    pub offset: u64,
    pub width: u32,
    pub height: u32,
    pub row_pitch: u32,
    /// Addressable rows, excluding padding and subsequent mip levels.
    pub size: u64,
}

impl LinearTextureLayout {
    pub fn new(width: u32, height: u32, mip_levels: u32, array_layers: u32) -> Option<Self> {
        if width == 0
            || height == 0
            || width > 0x7fff
            || height > 0x7fff
            || mip_levels == 0
            || mip_levels > 32 - width.max(height).leading_zeros()
            || array_layers == 0
            || array_layers > 2048
        {
            return None;
        }
        let row_pitch = align_up_u32(width.checked_mul(4)?, 64)?;
        let mut chain_size = 0u64;
        for level in 0..mip_levels {
            let pitch = align_up_u32((row_pitch >> level).max(1), 64)?;
            let mut rows = (height >> level).max(1);
            if level == mip_levels - 1 {
                rows = align_up_u32(rows, 4)?;
            }
            chain_size = chain_size.checked_add(u64::from(pitch) * u64::from(rows))?;
        }
        let layer_stride = u32::try_from(align_up_u64(chain_size, 4096)?).ok()?;
        let total_size = u64::from(layer_stride).checked_mul(u64::from(array_layers))?;
        Some(Self {
            width,
            height,
            mip_levels,
            array_layers,
            row_pitch,
            layer_stride,
            total_size,
        })
    }

    /// Byte range for a mip/layer selected by a transfer or texture view.
    pub fn subresource(self, mip_level: u32, array_layer: u32) -> Option<LinearSubresource> {
        if mip_level >= self.mip_levels || array_layer >= self.array_layers {
            return None;
        }
        let mut offset = u64::from(array_layer) * u64::from(self.layer_stride);
        for level in 0..mip_level {
            let pitch = align_up_u32((self.row_pitch >> level).max(1), 64)?;
            offset += u64::from(pitch) * u64::from((self.height >> level).max(1));
        }
        let row_pitch = align_up_u32((self.row_pitch >> mip_level).max(1), 64)?;
        let height = (self.height >> mip_level).max(1);
        Some(LinearSubresource {
            offset,
            width: (self.width >> mip_level).max(1),
            height,
            row_pitch,
            size: u64::from(row_pitch) * u64::from(height),
        })
    }
}

/// Calculate the canonical single-level D32 TILE6_3 layout.
pub const fn depth32_tile6_3_layout(width: u32, height: u32) -> Option<Depth32Tile6Layout> {
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = match width.checked_mul(4) {
        Some(value) => value,
        None => return None,
    };
    let row_pitch = match align_up_u32(row_bytes, DEPTH32_PITCH_ALIGNMENT) {
        Some(value) => value,
        None => return None,
    };
    let padded_height = match align_up_u32(height, DEPTH32_HEIGHT_ALIGNMENT) {
        Some(value) => value,
        None => return None,
    };
    let unaligned_size = match (row_pitch as u64).checked_mul(padded_height as u64) {
        Some(value) => value,
        None => return None,
    };
    let layer_size = match align_up_u64(unaligned_size, DEPTH32_LAYER_ALIGNMENT) {
        Some(value) => value,
        None => return None,
    };
    Some(Depth32Tile6Layout {
        row_pitch,
        padded_height,
        layer_size,
    })
}

/// Validate externally supplied metadata against the canonical layout.
pub const fn is_depth32_tile6_3_layout(
    width: u32,
    height: u32,
    row_pitch: u32,
    layer_size: u64,
) -> bool {
    match depth32_tile6_3_layout(width, height) {
        Some(layout) => layout.row_pitch == row_pitch && layout.layer_size == layer_size,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Depth32Tile6Layout, depth32_tile6_3_layout, is_depth32_tile6_3_layout};

    #[test]
    fn linear_mips_follow_freedreno_pitch_halving_and_layer_alignment() {
        let layout = super::LinearTextureLayout::new(65, 17, 4, 6).unwrap();
        assert_eq!(
            (layout.row_pitch, layout.layer_stride, layout.total_size),
            (320, 8192, 49152)
        );
        let expected = [
            (0, 65, 17, 320),
            (5440, 32, 8, 192),
            (6976, 16, 4, 128),
            (7488, 8, 2, 64),
        ];
        for layer in 0..6 {
            for (level, &(offset, width, height, pitch)) in expected.iter().enumerate() {
                let sub = layout.subresource(level as u32, layer).unwrap();
                assert_eq!(
                    (sub.offset, sub.width, sub.height, sub.row_pitch),
                    (offset + u64::from(layer) * 8192, width, height, pitch)
                );
                assert!(sub.offset + sub.size <= u64::from(layer + 1) * 8192);
            }
        }
        assert!(layout.subresource(4, 0).is_none());
        assert!(layout.subresource(0, 6).is_none());
    }

    #[test]
    fn linear_texture_bounds_and_last_level_padding_are_checked() {
        assert_eq!(
            super::LinearTextureLayout::new(1, 1, 1, 1)
                .unwrap()
                .layer_stride,
            4096
        );
        for args in [
            (0, 8, 1, 1),
            (8, 0, 1, 1),
            (8, 8, 0, 1),
            (8, 8, 5, 1),
            (8, 8, 1, 0),
            (8, 8, 1, 2049),
            (u32::MAX, 1, 1, 1),
        ] {
            assert!(super::LinearTextureLayout::new(args.0, args.1, args.2, args.3).is_none());
        }
    }

    #[test]
    fn d32_layout_matches_freedreno_a6xx_alignment() {
        assert_eq!(
            depth32_tile6_3_layout(16, 16),
            Some(Depth32Tile6Layout {
                row_pitch: 256,
                padded_height: 16,
                layer_size: 4096,
            })
        );
        assert_eq!(
            depth32_tile6_3_layout(2_200, 1_520),
            Some(Depth32Tile6Layout {
                row_pitch: 8_960,
                padded_height: 1_520,
                layer_size: 13_619_200,
            })
        );
        assert!(is_depth32_tile6_3_layout(16, 16, 256, 4096));
        assert!(!is_depth32_tile6_3_layout(16, 16, 64, 1024));
    }
}
