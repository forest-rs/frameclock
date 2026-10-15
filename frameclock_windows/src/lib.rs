// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Windows timing adapters for [`frameclock`].
//!
//! This crate owns Windows-specific timing adaptation. It reads the QPC
//! (`QueryPerformanceCounter`) host clock as [`HostTime`] / [`Timebase`],
//! builds [`FrameTick`] values for `VSync`-paced hosts via [`make_tick`], and
//! converts those ticks into [`FrameOpportunity`] values for callers that own
//! a [`frameclock::FrameDriver`].
//!
//! It intentionally does not own `HWND`s, message loops, `DwmFlush` pacing
//! threads, or swapchains. Those belong to hosts and backend crates such as
//! `subduction_backend_windows`; this crate owns the clock reads and tick
//! bookkeeping those hosts feed and poll.
//!
//! # Core Flow
//!
//! ```text
//! QueryPerformanceCounter / QueryPerformanceFrequency -> now / timebase -> HostTime
//! VSync-paced tick                                    -> make_tick      -> FrameTick
//! FrameTick                                    -> frame_opportunity -> FrameOpportunity
//! ```
//!
//! [`HostTime`]: frameclock::HostTime
//! [`Timebase`]: frameclock::time::Timebase

#![expect(
    unsafe_code,
    reason = "Windows timing adapters require QueryPerformanceCounter/Frequency FFI"
)]

mod tick;
mod time;

pub use tick::make_tick;
pub use time::{now, timebase};

use frameclock::timing::{PresentationTiming, default_commit_lead};
use frameclock::{
    DisplayTiming, Duration, FrameOpportunity, FrameTick, PresentHints, SchedulerConfig,
};

/// Returns the scheduler configuration for Windows ticks.
///
/// This is [`SchedulerConfig::estimated`] in QPC ticks: [`make_tick`]
/// predicts presents by extrapolating the refresh grid, and every duration
/// the scheduler works with is in this crate's [`timebase`].
#[must_use]
pub fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig::estimated(timebase())
}

/// Computes [`PresentHints`] from a Windows [`FrameTick`] using
/// [`default_commit_lead`].
///
/// Use [`present_hints_with_commit_lead`] when a host has a better estimate
/// of how long before a refresh its frame must be submitted.
#[must_use]
pub fn present_hints(tick: &FrameTick, fallback_refresh_interval: Duration) -> PresentHints {
    present_hints_with_commit_lead(
        tick,
        fallback_refresh_interval,
        default_commit_lead(tick.refresh_interval_or(fallback_refresh_interval)),
    )
}

/// Computes [`PresentHints`] from a Windows [`FrameTick`].
///
/// A predicted present from [`make_tick`] is extrapolated on the refresh
/// grid, so it is reported as estimated timing
/// (`PresentationTiming::Estimated`). If the prediction is missing or stale,
/// the hint falls back to pacing-only timing aimed one refresh after the
/// tick, as described by [`PresentHints::for_tick`].
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

/// Returns display timing for a Windows [`FrameTick`].
///
/// Prefers [`FrameTick::refresh_interval`], falling back to the
/// predicted-present delta and finally to `fallback_interval`. This always
/// produces fixed-rate timing.
#[must_use]
pub fn display_timing(tick: &FrameTick, fallback_interval: Duration) -> DisplayTiming {
    DisplayTiming::from_tick(tick, fallback_interval)
}

/// Builds a [`FrameOpportunity`] from a Windows tick using
/// [`default_commit_lead`].
///
/// `fallback_interval` is in QPC ticks and is used until the tick reports a
/// refresh interval.
#[must_use]
pub fn frame_opportunity(tick: FrameTick, fallback_interval: Duration) -> FrameOpportunity {
    let refresh_interval = tick.refresh_interval_or(fallback_interval);
    frame_opportunity_with_commit_lead(
        tick,
        fallback_interval,
        default_commit_lead(refresh_interval),
    )
}

/// Builds a [`FrameOpportunity`] with an explicit commit lead.
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
    use frameclock::timing::PresentationTiming;
    use frameclock::{DisplayTiming, Duration, FrameTick, HostTime, OutputId};

    /// 16.67 ms in ticks of a 10 MHz QPC.
    const REFRESH: Duration = Duration(166_666);

    fn tick(predicted_present: Option<HostTime>) -> FrameTick {
        FrameTick {
            now: HostTime(1_000_000),
            predicted_present,
            refresh_interval: Some(REFRESH),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    #[test]
    fn present_hints_with_prediction_are_estimated() {
        let hints = present_hints(&tick(Some(HostTime(1_100_000))), REFRESH);

        assert_eq!(hints.presentation_timing(), PresentationTiming::Estimated);
        assert_eq!(hints.desired_present(), Some(HostTime(1_100_000)));
        // A quarter refresh, 41,666 ticks, ahead of the present.
        assert_eq!(hints.latest_commit(), HostTime(1_058_334));
    }

    #[test]
    fn present_hints_respect_explicit_commit_lead() {
        let hints = present_hints_with_commit_lead(
            &tick(Some(HostTime(1_100_000))),
            REFRESH,
            Duration(20_000),
        );

        assert_eq!(hints.latest_commit(), HostTime(1_080_000));
    }

    #[test]
    fn present_hints_without_prediction_are_pacing_only() {
        let hints = present_hints(&tick(None), REFRESH);

        assert_eq!(hints.presentation_timing(), PresentationTiming::PacingOnly);
        assert_eq!(hints.desired_present(), None);
    }

    #[test]
    fn frame_opportunity_pairs_tick_hints_and_display_timing() {
        let tick = tick(Some(HostTime(1_100_000)));
        let opportunity = frame_opportunity(tick, Duration(83_333));

        assert_eq!(opportunity.tick, tick);
        assert_eq!(opportunity.hints, present_hints(&tick, Duration(83_333)));
        assert_eq!(opportunity.display_timing, DisplayTiming::fixed(REFRESH));
        assert_eq!(
            display_timing(&tick, Duration(83_333)),
            DisplayTiming::fixed(REFRESH)
        );
    }
}
