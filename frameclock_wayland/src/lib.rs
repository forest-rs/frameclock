// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Wayland timing adapters for [`frameclock`].
//!
//! This crate owns Wayland-specific timing adaptation. It selects and reads
//! the compositor-aligned presentation clock as [`HostTime`], converts
//! `wl_surface.frame` callback completions into [`FrameTick`] values via
//! [`TickerState`], and carries `wp_presentation` feedback facts as
//! [`PresentEvent`] values.
//!
//! It intentionally does not own `wl_surface` objects, event queues, buffers,
//! registries, or protocol dispatch. Protocol I/O belongs to hosts and backend
//! crates; this crate owns the timing bookkeeping those hosts feed and poll.
//!
//! # Core Flow
//!
//! ```text
//! wl_surface.frame done           -> TickerState -> FrameTick
//! wp_presentation_feedback events -> PresentEvent -> PresentEventQueue
//! wp_presentation.clock_id        -> Clock -> HostTime reads
//! ```
//!
//! A host's frame-callback dispatch path has this shape:
//!
//! ```rust,ignore
//! use frameclock::OutputId;
//! use frameclock_wayland::{Clock, TickerState};
//!
//! let mut ticker = TickerState::new();
//! let clock = Clock::Monotonic;
//!
//! // Claim the single in-flight slot before sending a wl_surface.frame request:
//! if ticker.mark_callback_requested() {
//!     // send the wl_surface.frame request
//! }
//!
//! // When the matching wl_callback.done event arrives:
//! ticker.on_callback_done(clock, OutputId(0));
//!
//! // After dispatch, drain the queued ticks:
//! while let Some(tick) = ticker.poll_tick() {
//!     // Build a FrameOpportunity and plan the frame.
//!     _ = tick;
//! }
//! ```
//!
//! All `HostTime` values are nanosecond ticks. When the compositor advertises
//! `wp_presentation`, map its `clock_id` event to a [`Clock`] with
//! [`Clock::from_presentation_clock_id`] and read timing facts from that clock
//! so feedback timestamps and tick times stay in one time domain.
//!
//! This crate keeps its implementation `no_std` (with `alloc`), but reading
//! clocks requires an operating system. It is intended to be validated on
//! Linux targets, not on generic no-std targets such as `x86_64-unknown-none`.
//!
//! [`HostTime`]: frameclock::HostTime
//! [`FrameTick`]: frameclock::FrameTick

#![no_std]

extern crate alloc;

mod presentation;
mod queue;
mod tick;
mod time;

pub use presentation::{
    PresentEvent, PresentEventQueue, SubmissionId, presentation_time_to_host_time,
};
pub use tick::TickerState;
pub use time::{Clock, now, timebase};

use frameclock::timing::{PresentationTiming, default_commit_lead};
use frameclock::{
    DisplayTiming, Duration, FrameOpportunity, FrameTick, PresentHints, SchedulerConfig,
};

/// Returns the scheduler configuration for Wayland ticks.
///
/// This is [`SchedulerConfig::estimated`] in this crate's nanosecond
/// [`timebase`]: once presentation feedback establishes a cadence,
/// [`frame_opportunity`] builds estimated hints from a client-side
/// extrapolation of the refresh grid.
#[must_use]
pub const fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig::estimated(timebase())
}

/// Computes [`PresentHints`] from a Wayland [`FrameTick`] using
/// [`default_commit_lead`].
///
/// Use [`present_hints_with_commit_lead`] when a host has a platform-specific
/// commit lead estimate.
#[must_use]
pub fn present_hints(tick: &FrameTick, fallback_refresh_interval: Duration) -> PresentHints {
    present_hints_with_commit_lead(
        tick,
        fallback_refresh_interval,
        default_commit_lead(tick.refresh_interval_or(fallback_refresh_interval)),
    )
}

/// Computes [`PresentHints`] from a Wayland [`FrameTick`].
///
/// A predicted present time derived from `wp_presentation` feedback (see
/// [`TickerState`]) is a client-side extrapolation of the vsync grid, so it is
/// reported as estimated timing (`PresentationTiming::Estimated`) rather than
/// predictive. If the prediction is missing or stale, the hint falls back to
/// pacing-only timing aimed one refresh after the tick, as described by
/// [`PresentHints::for_tick`]. The scheduler applies its own learned build
/// margin later.
#[must_use]
pub fn present_hints_with_commit_lead(
    tick: &FrameTick,
    fallback_refresh_interval: Duration,
    commit_lead: Duration,
) -> PresentHints {
    PresentHints::for_tick(
        tick,
        PresentationTiming::Estimated,
        fallback_refresh_interval,
        commit_lead,
    )
}

/// Returns display timing for a Wayland [`FrameTick`].
///
/// Prefers [`FrameTick::refresh_interval`] when present (the compositor's
/// reported cadence), falling back to the predicted-present delta and finally to
/// `fallback_interval`. Wayland's `wp_presentation` does not expose a
/// variable-refresh range, so this always produces fixed-rate timing.
#[must_use]
pub fn display_timing(tick: &FrameTick, fallback_interval: Duration) -> DisplayTiming {
    DisplayTiming::from_tick(tick, fallback_interval)
}

/// Builds a [`FrameOpportunity`] from a Wayland frame-callback tick.
///
/// This pairs Wayland's estimated/pacing presentation hints with display
/// timing derived from the same tick, falling back to `fallback_interval` until
/// presentation feedback has established a cadence.
#[must_use]
pub fn frame_opportunity(tick: FrameTick, fallback_interval: Duration) -> FrameOpportunity {
    let refresh_interval = tick.refresh_interval_or(fallback_interval);
    frame_opportunity_with_commit_lead(
        tick,
        fallback_interval,
        default_commit_lead(refresh_interval),
    )
}

/// Builds a [`FrameOpportunity`] with an explicit platform commit lead.
#[must_use]
pub fn frame_opportunity_with_commit_lead(
    tick: FrameTick,
    fallback_interval: Duration,
    commit_lead: Duration,
) -> FrameOpportunity {
    let hints = present_hints_with_commit_lead(&tick, fallback_interval, commit_lead);
    let display_timing = display_timing(&tick, fallback_interval);
    FrameOpportunity::new(tick, hints, display_timing)
}

#[cfg(test)]
mod tests {
    use super::{display_timing, frame_opportunity, present_hints, present_hints_with_commit_lead};
    use frameclock::OutputId;
    use frameclock::timing::PresentationTiming;
    use frameclock::{DisplayTiming, Duration, FrameTick, HostTime};

    fn tick(predicted_present: Option<HostTime>) -> FrameTick {
        FrameTick {
            now: HostTime(1_000_000),
            predicted_present,
            refresh_interval: Some(Duration(16_666_667)),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    #[test]
    fn present_hints_with_prediction_are_estimated() {
        let hints = present_hints(&tick(Some(HostTime(20_000_000))), Duration(16_666_667));

        assert_eq!(hints.presentation_timing(), PresentationTiming::Estimated);
        assert_eq!(hints.desired_present(), Some(HostTime(20_000_000)));
        assert_eq!(hints.latest_commit(), HostTime(15_833_334));
    }

    #[test]
    fn present_hints_respect_explicit_commit_lead() {
        let hints = present_hints_with_commit_lead(
            &tick(Some(HostTime(20_000_000))),
            Duration(16_666_667),
            Duration(2_000_000),
        );

        assert_eq!(hints.presentation_timing(), PresentationTiming::Estimated);
        assert_eq!(hints.desired_present(), Some(HostTime(20_000_000)));
        assert_eq!(hints.latest_commit(), HostTime(18_000_000));
    }

    #[test]
    fn present_hints_without_prediction_are_pacing_only() {
        let hints = present_hints(&tick(None), Duration(16_666_667));

        assert_eq!(hints.presentation_timing(), PresentationTiming::PacingOnly);
        assert_eq!(hints.desired_present(), None);
        assert_eq!(hints.latest_commit(), HostTime(13_500_001));
    }

    #[test]
    fn present_hints_ignore_stale_prediction() {
        let stale_tick = FrameTick {
            now: HostTime(2_000_000),
            predicted_present: Some(HostTime(1_900_000)),
            refresh_interval: Some(Duration(16_666_667)),
            output: OutputId(0),
            prev_actual_present: None,
        };
        let hints = present_hints(&stale_tick, Duration(16_666_667));

        assert_eq!(hints.presentation_timing(), PresentationTiming::PacingOnly);
        assert_eq!(hints.desired_present(), None);
        assert_eq!(hints.latest_commit(), HostTime(14_500_001));
    }

    #[test]
    fn display_timing_prefers_reported_refresh_interval() {
        assert_eq!(
            display_timing(&tick(Some(HostTime(2_000_000))), Duration(8_333_333)),
            DisplayTiming::fixed(Duration(16_666_667))
        );
    }

    #[test]
    fn frame_opportunity_pairs_tick_hints_and_display_timing() {
        let tick = tick(Some(HostTime(20_000_000)));
        let opportunity = frame_opportunity(tick, Duration(8_333_333));

        assert_eq!(opportunity.tick, tick);
        assert_eq!(
            opportunity.hints.presentation_timing(),
            PresentationTiming::Estimated
        );
        assert_eq!(
            opportunity.hints.desired_present(),
            Some(HostTime(20_000_000))
        );
        assert_eq!(
            opportunity.display_timing,
            DisplayTiming::fixed(Duration(16_666_667))
        );
    }
}
