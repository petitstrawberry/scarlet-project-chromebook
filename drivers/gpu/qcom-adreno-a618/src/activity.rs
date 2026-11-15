// SPDX-License-Identifier: GPL-2.0-only
//! GMU busy ticks use the fixed 19.2 MHz XO clock, independent of GPU DVFS.
#[derive(Default)]
pub(crate) struct ActivityWindow {
    previous: Option<(u64, u64)>,
}
impl ActivityWindow {
    pub(crate) fn reset(&mut self) {
        self.previous = None;
    }
    pub(crate) fn sample(
        &mut self,
        busy_ticks: u64,
        time_us: u64,
    ) -> Result<(u32, u32), &'static str> {
        let previous = self.previous.replace((busy_ticks, time_us));
        let (previous_ticks, previous_us) = previous.ok_or("GPU activity baseline initialized")?;
        let busy = busy_ticks
            .checked_sub(previous_ticks)
            .ok_or("GPU activity counter reset")? as u128;
        let elapsed = time_us
            .checked_sub(previous_us)
            .ok_or("GPU activity clock reset")?;
        let total = u128::from(elapsed) * 96 / 5;
        if total == 0 {
            return Err("GPU activity interval is empty");
        }
        // Counter and host timer reads are not simultaneous. Allow at most
        // two microseconds of skew, never disguise an incorrectly configured
        // counter or a reset as valid 100% activity.
        if busy > total + 39 {
            return Err("GPU activity exceeds elapsed XO ticks");
        }
        let divisor = total.div_ceil(u128::from(u32::MAX));
        Ok(((busy.min(total) / divisor) as u32, (total / divisor) as u32))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_half_and_full_windows() {
        let mut w = ActivityWindow::default();
        assert!(w.sample(50, 100).is_err());
        assert_eq!(w.sample(50, 25100), Ok((0, 480000)));
        assert_eq!(w.sample(240050, 50100), Ok((240000, 480000)));
        assert_eq!(w.sample(720050, 75100), Ok((480000, 480000)));
    }
    #[test]
    fn reset_and_impossible_counter_do_not_publish_activity() {
        let mut w = ActivityWindow::default();
        assert!(w.sample(100, 10).is_err());
        assert!(w.sample(0, 20).is_err());
        assert!(w.sample(10000, 30).is_err());
        w.reset();
        assert!(w.sample(10000, 40).is_err());
        assert_eq!(w.sample(10000, 50), Ok((0, 192)));
    }
    #[test]
    fn long_intervals_scale_both_counters_without_overflow() {
        let mut w = ActivityWindow::default();
        let _ = w.sample(0, 0);
        let (busy, total) = w.sample(9_600_000_000, 1_000_000_000).unwrap();
        assert_eq!(busy * 2, total);
        assert!(total > 0);
    }
}
