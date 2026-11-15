//! Pure A618 clock arithmetic and bounded split-register sampling.

/// Reject torn reads when the low word wraps between MMIO accesses.
pub(super) fn read_stable(mut read: impl FnMut(bool) -> u32) -> Option<u64> {
    for _ in 0..8 {
        let high = read(true);
        let low = read(false);
        if read(true) == high {
            return Some((u64::from(high) << 32) | u64::from(low));
        }
    }
    None
}

pub(super) fn ticks_to_ns(ticks: u64) -> Option<u64> {
    // 1e9 / 19.2e6 = 625/12. Multiply before division so fractional ticks
    // accumulate correctly; 52 ns/tick would introduce systematic drift.
    u64::try_from(u128::from(ticks) * 625 / 12).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_frequency_fractional_ticks_and_overflow() {
        assert_eq!(ticks_to_ns(0), Some(0));
        assert_eq!(ticks_to_ns(1), Some(52));
        assert_eq!(ticks_to_ns(12), Some(625));
        assert_eq!(ticks_to_ns(19_200_000), Some(1_000_000_000));
        assert_eq!(ticks_to_ns(19_200_000 * 86_400), Some(86_400_000_000_000));
        assert_eq!(ticks_to_ns(u64::MAX), None);
        let last = ((u128::from(u64::MAX) * 12 + 11) / 625) as u64;
        assert!(ticks_to_ns(last).is_some());
        assert_eq!(ticks_to_ns(last + 1), None);
    }

    #[test]
    fn rollover_retries_instead_of_returning_torn_clock() {
        let mut reads = [
            (true, 7),
            (false, 0),
            (true, 8),
            (true, 8),
            (false, 3),
            (true, 8),
        ]
        .into_iter();
        assert_eq!(
            read_stable(|high| {
                let (expected, value) = reads.next().unwrap();
                assert_eq!(high, expected);
                value
            }),
            Some((8u64 << 32) | 3)
        );
        assert_eq!(reads.next(), None);
    }

    #[test]
    fn unstable_clock_is_bounded_and_does_not_fall_back_to_host_time() {
        let mut count = 0;
        assert_eq!(
            read_stable(|_| {
                count += 1;
                count
            }),
            None
        );
        assert_eq!(count, 24);
    }
}
