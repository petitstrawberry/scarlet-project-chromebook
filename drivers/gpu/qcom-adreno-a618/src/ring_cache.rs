//! Byte ranges dirtied by one circular ring write, excluding untouched slots.

pub(super) fn dirty_ranges(
    start: usize,
    words: usize,
    capacity: usize,
) -> Option<[(usize, usize); 2]> {
    if !capacity.is_power_of_two() || start >= capacity || words >= capacity {
        return None;
    }
    capacity.checked_mul(core::mem::size_of::<u32>())?;
    let first = words.min(capacity - start);
    Some([(start * 4, first * 4), (0, (words - first) * 4)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn circular_write_and_cache_ranges_cover_identical_bytes() {
        // Compare against the per-word writer at every ring position, including
        // partial cache lines, exact end and wrapped 17-word completion tails.
        for capacity in [8, 64, 8192] {
            for start in 0..capacity {
                for words in [0, 1, 2, 7, 17, 31].into_iter().filter(|n| *n < capacity) {
                    let ranges = dirty_ranges(start, words, capacity).unwrap();
                    let bytes: Vec<_> = ranges
                        .into_iter()
                        .flat_map(|(offset, len)| offset..offset + len)
                        .collect();
                    let written: Vec<_> = (0..words)
                        .flat_map(|n| {
                            let offset = ((start + n) & (capacity - 1)) * 4;
                            offset..offset + 4
                        })
                        .collect();
                    assert_eq!(bytes, written);
                }
            }
        }
    }

    #[test]
    fn wraps_split_only_the_modified_tail_and_head() {
        assert_eq!(dirty_ranges(8190, 17, 8192), Some([(32760, 8), (0, 60)]));
        assert_eq!(dirty_ranges(8175, 17, 8192), Some([(32700, 68), (0, 0)]));
        assert_eq!(dirty_ranges(0, 17, 8192), Some([(0, 68), (0, 0)]));
    }

    #[test]
    fn malformed_ring_bounds_are_rejected_before_cache_maintenance() {
        for (start, words, capacity) in [
            (0, 0, 0),
            (0, 1, 3),
            (8, 1, 8),
            (0, 8, 8),
            (0, usize::MAX, 8),
            (0, 1, 1usize << (usize::BITS - 1)),
        ] {
            assert_eq!(dirty_ranges(start, words, capacity), None);
        }
    }
}
