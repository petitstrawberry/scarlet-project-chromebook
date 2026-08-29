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
