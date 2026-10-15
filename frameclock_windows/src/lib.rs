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

/// Returns fixed 60 Hz display timing in QPC ticks.
///
/// Pass it to [`frame_opportunity`] when nothing better is known about the
/// output: fixed timing is refined from each tick's refresh interval. For a
/// variable-refresh output, pass [`DisplayTiming::variable`] with its range
/// instead.
#[must_use]
pub fn default_display_timing() -> DisplayTiming {
    DisplayTiming::fixed(Duration::from_nanos(16_666_667, timebase()))
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
///
/// The hints [start each frame at a tick](PresentHints::starting_at_tick),
/// so a host that renders from its `VSync`-paced loop is handed ready frames
/// there. A host with its own timer can mark them
/// [`starting_when_planned`](PresentHints::starting_when_planned) to start
/// frames nearer their deadline.
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
    .starting_at_tick()
}

/// Returns display timing for a Windows [`FrameTick`] and its target output.
///
/// `fallback_timing` describes the output the tick was made for. Pass
/// [`DisplayTiming::variable`] when the output runs with variable refresh
/// (adaptive sync or Windows dynamic refresh): DXGI does not report the
/// range, so it comes from the host, for example from the monitor's EDID.
/// Variable timing is kept as is, because on such an output the tick's
/// refresh interval is only the last observed gap. Fixed timing is refined
/// from the tick: [`FrameTick::refresh_interval`] first, then the
/// predicted-present delta, then `fallback_timing`'s interval.
#[must_use]
pub fn display_timing(tick: &FrameTick, fallback_timing: DisplayTiming) -> DisplayTiming {
    if fallback_timing.is_variable() {
        fallback_timing
    } else {
        DisplayTiming::from_tick(tick, fallback_timing.min_interval())
    }
}

/// Builds a [`FrameOpportunity`] from a Windows tick using
/// [`default_commit_lead`].
///
/// `fallback_timing` is in QPC ticks and describes the tick's output, as for
/// [`display_timing`]. Its fastest interval stands in for the refresh
/// interval until the tick reports one.
#[must_use]
pub fn frame_opportunity(tick: FrameTick, fallback_timing: DisplayTiming) -> FrameOpportunity {
    let refresh_interval = tick.refresh_interval_or(fallback_timing.min_interval());
    frame_opportunity_with_commit_lead(tick, fallback_timing, default_commit_lead(refresh_interval))
}

/// Builds a [`FrameOpportunity`] with an explicit commit lead.
#[must_use]
pub fn frame_opportunity_with_commit_lead(
    tick: FrameTick,
    fallback_timing: DisplayTiming,
    commit_lead: Duration,
) -> FrameOpportunity {
    let hints = present_hints_with_commit_lead(&tick, fallback_timing.min_interval(), commit_lead);
    let display_timing = display_timing(&tick, fallback_timing);
    FrameOpportunity::new(tick, hints, display_timing)
}

#[cfg(test)]
mod tests {
    use super::{
        default_display_timing, display_timing, frame_opportunity, present_hints,
        present_hints_with_commit_lead, scheduler_config, timebase,
    };
    use frameclock::timing::PresentationTiming;
    use frameclock::{DisplayTiming, Duration, FrameTick, HostTime, OutputId};
    use frameclock::{FrameBeginResult, FrameDemand, FrameDriver, FrameSubmission};

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
        let fallback = DisplayTiming::fixed(Duration(83_333));
        let opportunity = frame_opportunity(tick, fallback);

        assert_eq!(opportunity.tick, tick);
        assert_eq!(opportunity.hints, present_hints(&tick, Duration(83_333)));
        assert_eq!(opportunity.display_timing, DisplayTiming::fixed(REFRESH));
        assert_eq!(
            display_timing(&tick, fallback),
            DisplayTiming::fixed(REFRESH)
        );
    }

    #[test]
    fn vsync_ticks_are_handed_ready_frames() {
        let mut driver = FrameDriver::new(scheduler_config());
        for i in 0..4 {
            let now = HostTime(1_000_000 + i * REFRESH.ticks());
            let tick = FrameTick {
                now,
                predicted_present: Some(HostTime(now.ticks() + REFRESH.ticks())),
                refresh_interval: Some(REFRESH),
                output: OutputId(0),
                prev_actual_present: None,
            };
            driver.request(FrameDemand::ANIMATION);
            let opportunity = frame_opportunity(tick, default_display_timing());
            let FrameBeginResult::Ready(frame) = driver.begin_frame(opportunity).result else {
                panic!("tick {i} must be handed a ready frame");
            };
            let _ = driver.submit_frame(frame, FrameSubmission::new(now, None));
        }
    }

    #[test]
    fn default_display_timing_is_60_hz() {
        let interval = default_display_timing().min_interval();
        let nanos = timebase().ticks_to_nanos(interval.ticks());
        assert!(nanos.abs_diff(16_666_667) < 1_000, "{nanos} ns");
    }

    #[test]
    fn variable_fallback_timing_is_kept() {
        // A 48-144 Hz variable-refresh range in 10 MHz QPC ticks.
        let variable = DisplayTiming::variable(Duration(69_444), Duration(208_333), None);
        let tick = tick(Some(HostTime(1_100_000)));
        let opportunity = frame_opportunity(tick, variable);

        assert_eq!(display_timing(&tick, variable), variable);
        assert_eq!(opportunity.display_timing, variable);
        // The tick's own interval still sets the commit lead.
        assert_eq!(opportunity.hints, present_hints(&tick, Duration(69_444)));
    }
}
