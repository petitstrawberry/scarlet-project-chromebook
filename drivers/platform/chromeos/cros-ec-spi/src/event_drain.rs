// SPDX-License-Identifier: GPL-2.0-only

/// Drain a bounded batch of events already removed from the EC FIFO.
/// Consumer failure cannot put an event back. Continue to subsequent events,
/// especially button releases, and reserve transport backoff for read errors.
/// `false` means the budget ran out with more traffic pending, not a failure.
pub(super) fn drain_batch<T, E>(
    budget: usize,
    mut read: impl FnMut() -> Result<Option<(T, bool)>, E>,
    mut dispatch: impl FnMut(T) -> bool,
) -> Result<bool, E> {
    for _ in 0..budget {
        let Some((event, more)) = read()? else {
            return Ok(true);
        };
        let _ = dispatch(event);
        if !more {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_sensor_does_not_delay_button_release() {
        let mut events = [(1, true), (2, true), (0, false)].into_iter();
        let mut received = Vec::new();
        let result = drain_batch(
            64,
            || Ok::<_, ()>(events.next()),
            |event| {
                received.push(event);
                event != 2 // Sensor callback fails between press and release.
            },
        );
        assert_eq!(result, Ok(true));
        assert_eq!(received, [1, 2, 0]);
    }

    #[test]
    fn busy_fifo_yields_without_entering_error_backoff() {
        let mut delivered = 0;
        assert_eq!(
            drain_batch(
                64,
                || Ok::<_, ()>(Some(((), true))),
                |_| {
                    delivered += 1;
                    false
                }
            ),
            Ok(false)
        );
        assert_eq!(delivered, 64);
    }

    #[test]
    fn transport_failure_still_stops_the_batch() {
        assert_eq!(drain_batch::<(), _>(64, || Err(7), |_| panic!()), Err(7));
        assert_eq!(
            drain_batch::<(), ()>(64, || Ok(None), |_| panic!()),
            Ok(true)
        );
    }
}
