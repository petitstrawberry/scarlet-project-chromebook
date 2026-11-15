// SPDX-License-Identifier: GPL-2.0-only
//! Bounded polling around short completions; IRQ-driven sleep for actual work.
//! Decisions here never authorize retirement: the caller must check the DMA
//! fence, scratch sequence, ring read pointer and hardware idle together.

const SHORT_POLL_US: u64 = 50;
const TAIL_RECHECK_NS: u64 = 100_000;
const MISSING_IRQ_WATCHDOG_NS: u64 = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Poll,
    Interrupt { timeout_ns: u64 },
    Tail { timeout_ns: u64 },
    Deadline,
}

pub(super) struct CompletionWait {
    deadline_us: u64,
    first_irq_us: Option<u64>,
}

impl CompletionWait {
    pub(super) fn new(deadline_us: u64) -> Self {
        Self {
            deadline_us,
            first_irq_us: None,
        }
    }

    pub(super) fn action(&mut self, elapsed_us: u64, completion_irq: bool) -> Action {
        if elapsed_us >= self.deadline_us {
            return Action::Deadline;
        }
        if completion_irq && self.first_irq_us.is_none() {
            self.first_irq_us = Some(elapsed_us);
        }
        if elapsed_us < SHORT_POLL_US
            || self
                .first_irq_us
                .is_some_and(|first| elapsed_us.saturating_sub(first) < SHORT_POLL_US)
        {
            return Action::Poll;
        }
        let remaining_ns = (self.deadline_us - elapsed_us).saturating_mul(1_000);
        if self.first_irq_us.is_some() {
            // The timestamp IRQ can precede DMA visibility/CP idle; there may
            // be no second IRQ. Bound that exceptional tail without 1ms slack.
            Action::Tail {
                timeout_ns: remaining_ns.min(TAIL_RECHECK_NS),
            }
        } else {
            // Recover a lost/masked IRQ without delaying completed work for
            // the entire device timeout. Normally the IRQ wakes this wait.
            Action::Interrupt {
                timeout_ns: remaining_ns.min(MISSING_IRQ_WATCHDOG_NS),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_completion_needs_no_timer_but_irq_alone_never_retires() {
        let mut wait = CompletionWait::new(1_000_000);
        for elapsed in 0..50 {
            assert_eq!(wait.action(elapsed, false), Action::Poll);
        }
        assert_eq!(
            wait.action(50, false),
            Action::Interrupt {
                timeout_ns: 10_000_000
            }
        );
        assert_eq!(wait.action(200, true), Action::Poll);
        assert_eq!(wait.action(249, true), Action::Poll);
        assert_eq!(
            wait.action(250, true),
            Action::Tail {
                timeout_ns: 100_000
            }
        );
    }

    #[test]
    fn repeated_irqs_do_not_restart_poll_budget_or_extend_deadline() {
        let mut wait = CompletionWait::new(1_000);
        assert_eq!(wait.action(100, true), Action::Poll);
        for elapsed in [150, 400, 800] {
            assert_eq!(
                wait.action(elapsed, true),
                Action::Tail {
                    timeout_ns: 100_000
                }
            );
        }
        assert_eq!(wait.action(999, true), Action::Tail { timeout_ns: 1_000 });
        assert_eq!(wait.action(1_000, true), Action::Deadline);
        assert_eq!(wait.action(u64::MAX, true), Action::Deadline);
    }

    #[test]
    fn lost_irq_is_bounded_by_original_gpu_deadline() {
        let mut wait = CompletionWait::new(1_000);
        assert_eq!(
            wait.action(60, false),
            Action::Interrupt {
                timeout_ns: 940_000
            }
        );
        assert_eq!(
            wait.action(900, false),
            Action::Interrupt {
                timeout_ns: 100_000
            }
        );
        assert_eq!(wait.action(1_000, false), Action::Deadline);
    }

    #[test]
    fn missing_irq_watchdog_rechecks_dma_before_device_timeout() {
        let mut wait = CompletionWait::new(1_000_000);
        assert_eq!(
            wait.action(100, false),
            Action::Interrupt {
                timeout_ns: 10_000_000
            }
        );
        assert_eq!(
            wait.action(10_100, false),
            Action::Interrupt {
                timeout_ns: 10_000_000
            }
        );
        assert_eq!(
            wait.action(999_999, false),
            Action::Interrupt { timeout_ns: 1_000 }
        );
    }
}
