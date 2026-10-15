// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Present feedback contracts and queueing.
//!
//! This path is for a backend that owns presentation — one that issues
//! `PresentPixmap` and correlates the resulting `PresentCompleteNotify`
//! (kind `Pixmap`) and `PresentIdleNotify` events back to its submissions. A
//! host that lets a swapchain owner (for example wgpu/Vulkan) present does not
//! receive these per-pixmap completions on its own connection and instead paces
//! from the [`PresentTicker`](crate::PresentTicker) vblank stream alone.

use crate::queue::BoundedQueue;
use crate::time::ust_to_host_time;
use frameclock::{HostTime, OutputId};

/// Unique identity for one `PresentPixmap` submission.
///
/// A backend chooses the `serial` it passes to `PresentPixmap` and matches it
/// against the `serial` echoed by the `PresentCompleteNotify` and
/// `PresentIdleNotify` events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubmissionId(pub u32);

/// Per-submission Present feedback event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresentEvent {
    /// A pixmap request completed (`PresentCompleteNotify`, kind `Pixmap`).
    ///
    /// Completion may mean presentation or a skipped pixmap. Check
    /// [`Self::actual_present`] before treating it as presentation evidence.
    Complete {
        /// Identity of the submission this event corresponds to.
        id: SubmissionId,
        /// Completion UST timestamp in host-time ticks.
        ///
        /// This is not a presentation timestamp for skipped or unknown modes.
        completed_at: HostTime,
        /// Media stream counter (vblank count) at presentation.
        msc: u64,
        /// Output associated with the request, if known.
        output: Option<OutputId>,
        /// Raw `CompleteMode` value (`Copy`, `Flip`, `Skip`, …).
        mode: u8,
    },
    /// A submitted buffer became idle and can be reused
    /// (`PresentIdleNotify`).
    Idle {
        /// Identity of the submission whose buffer is now idle.
        id: SubmissionId,
    },
}

impl PresentEvent {
    /// Builds a [`PresentEvent::Complete`] from raw `PresentCompleteNotify`
    /// (kind `Pixmap`) fields, converting `ust` microseconds to host ticks.
    #[must_use]
    pub const fn completed(
        id: SubmissionId,
        ust_micros: u64,
        msc: u64,
        output: Option<OutputId>,
        mode: u8,
    ) -> Self {
        Self::Complete {
            id,
            completed_at: ust_to_host_time(ust_micros),
            msc,
            output,
            mode,
        }
    }

    /// Returns presentation evidence for a known successful completion mode.
    ///
    /// `Copy` (0), `Flip` (1), and `SuboptimalCopy` (3) report presentation.
    /// `Skip` (2), unknown modes, and idle notifications return `None`. A host
    /// must handle skipped frames separately when resolving submission feedback.
    #[must_use]
    pub const fn actual_present(self) -> Option<HostTime> {
        match self {
            Self::Complete {
                completed_at,
                mode: 0 | 1 | 3,
                ..
            } => Some(completed_at),
            _ => None,
        }
    }
}

/// Bounded FIFO queue for [`PresentEvent`] values.
///
/// Overflow policy is `drop_oldest`: when full, pushing a new event removes the
/// oldest queued event first, keeping the newest feedback available to the
/// backend under backpressure.
#[derive(Debug, Clone)]
pub struct PresentEventQueue {
    inner: BoundedQueue<PresentEvent>,
}

impl PresentEventQueue {
    /// Default queue capacity used by [`Default`].
    pub const DEFAULT_CAPACITY: usize = 64;

    /// Creates a queue with an explicit capacity.
    ///
    /// `capacity == 0` is promoted to `1`.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: BoundedQueue::with_capacity(capacity),
        }
    }

    /// Enqueues one Present feedback event.
    pub fn push(&mut self, event: PresentEvent) {
        self.inner.push(event);
    }

    /// Pops the oldest queued event, if any.
    pub fn pop(&mut self) -> Option<PresentEvent> {
        self.inner.pop()
    }

    /// Returns the current queue length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` when no events are queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Number of events dropped due to queue overflow.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped_count()
    }
}

impl Default for PresentEventQueue {
    fn default() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::{PresentEvent, PresentEventQueue, SubmissionId};
    use crate::time::ust_to_host_time;
    use frameclock::OutputId;

    #[test]
    fn completed_helper_converts_ust_to_host_ticks() {
        let event = PresentEvent::completed(SubmissionId(9), 1_500_000, 42, Some(OutputId(1)), 1);
        assert_eq!(
            event,
            PresentEvent::Complete {
                id: SubmissionId(9),
                completed_at: ust_to_host_time(1_500_000),
                msc: 42,
                output: Some(OutputId(1)),
                mode: 1,
            }
        );
    }

    #[test]
    fn skipped_pixmap_is_not_reported_as_presented() {
        let event = PresentEvent::completed(SubmissionId(9), 1_500_000, 42, None, 2);
        assert_eq!(event.actual_present(), None);
        assert!(matches!(event, PresentEvent::Complete { mode: 2, .. }));
    }

    #[test]
    fn only_known_presentation_modes_provide_actual_present() {
        for mode in 0..=u8::MAX {
            let event = PresentEvent::completed(SubmissionId(9), 1_500_000, 42, None, mode);
            let expected = matches!(mode, 0 | 1 | 3).then_some(ust_to_host_time(1_500_000));
            assert_eq!(event.actual_present(), expected, "mode {mode}");
        }
        assert_eq!(
            PresentEvent::Idle {
                id: SubmissionId(9)
            }
            .actual_present(),
            None
        );
    }

    #[test]
    fn queue_overflow_drops_oldest_event() {
        let mut queue = PresentEventQueue::with_capacity(2);
        queue.push(PresentEvent::Idle {
            id: SubmissionId(1),
        });
        queue.push(PresentEvent::Idle {
            id: SubmissionId(2),
        });
        queue.push(PresentEvent::Idle {
            id: SubmissionId(3),
        });

        assert_eq!(
            queue.pop(),
            Some(PresentEvent::Idle {
                id: SubmissionId(2)
            })
        );
        assert_eq!(
            queue.pop(),
            Some(PresentEvent::Idle {
                id: SubmissionId(3)
            })
        );
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.dropped_count(), 1);
    }

    #[test]
    fn zero_capacity_is_promoted_to_one() {
        let mut queue = PresentEventQueue::with_capacity(0);
        queue.push(PresentEvent::Idle {
            id: SubmissionId(1),
        });
        queue.push(PresentEvent::Idle {
            id: SubmissionId(2),
        });

        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.pop(),
            Some(PresentEvent::Idle {
                id: SubmissionId(2)
            })
        );
        assert_eq!(queue.dropped_count(), 1);
    }
}
