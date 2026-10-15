// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Browser timing adapters for [`frameclock`].
//!
//! This crate owns browser-specific timing adaptation. It converts
//! `requestAnimationFrame` callbacks into [`FrameTick`] values, exposes
//! `performance.now()` as [`HostTime`], and converts RAF ticks into
//! [`FrameOpportunity`] values for callers that own a
//! [`frameclock::FrameDriver`].
//!
//! It intentionally does not own DOM presentation, WebGL, WebGPU, application
//! state, or renderer submission.

#![no_std]

extern crate alloc;

mod raf;

pub use raf::RafLoop;

use frameclock::time::Timebase;
use frameclock::timing::{PresentationTiming, default_commit_lead};
use frameclock::{
    DisplayTiming, Duration, FrameOpportunity, FrameTick, HostTime, PresentHints, SchedulerConfig,
};

/// Browser host-time conversion: 1 tick = 1 microsecond = 1000 nanoseconds.
pub const TIMEBASE: Timebase = Timebase::new(1000, 1);

/// Fallback display interval for browser RAF ticks without an interval.
///
/// The value is a 60 Hz interval in microsecond ticks. Browsers do not expose a
/// portable refresh interval through `requestAnimationFrame`, so callers should
/// treat this only as a conservative pacing fallback.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration(16_667);

/// Returns the current host time from `performance.now()`.
///
/// The returned [`HostTime`] is in microsecond ticks.
#[must_use]
pub fn now() -> HostTime {
    let ms = raf::performance_now();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "performance.now() returns small positive f64 values; microseconds fit in u64"
    )]
    let us = (ms * 1000.0) as u64;
    HostTime(us)
}

/// Returns the browser [`Timebase`].
///
/// `Timebase::new(1000, 1)` means `nanoseconds = ticks * 1000`.
#[must_use]
pub const fn timebase() -> Timebase {
    TIMEBASE
}

/// Returns the scheduler configuration for browser ticks.
///
/// This is [`SchedulerConfig::pacing_only`] in microsecond ticks: RAF gives
/// no predicted present time, and every duration the scheduler works with
/// is in [`TIMEBASE`].
#[must_use]
pub const fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig::pacing_only(TIMEBASE)
}

/// Returns fixed display timing at [`DEFAULT_REFRESH_INTERVAL`], 60 Hz in
/// microsecond ticks.
///
/// Pass it to [`frame_opportunity`] unless the host knows more about the
/// output than the browser reports.
#[must_use]
pub const fn default_display_timing() -> DisplayTiming {
    DisplayTiming::fixed(DEFAULT_REFRESH_INTERVAL)
}

/// Computes pacing-only [`PresentHints`] from a browser [`FrameTick`] using
/// [`default_commit_lead`].
///
/// Use [`present_hints_with_commit_lead`] when a host has a better estimate of
/// how long the browser needs after the callback.
#[must_use]
pub fn present_hints(tick: &FrameTick, fallback_refresh_interval: Duration) -> PresentHints {
    present_hints_with_commit_lead(
        tick,
        fallback_refresh_interval,
        default_commit_lead(tick.refresh_interval_or(fallback_refresh_interval)),
    )
}

/// Computes pacing-only [`PresentHints`] from a browser [`FrameTick`].
///
/// Browsers do not expose a portable predicted present time or commit deadline
/// through `requestAnimationFrame`, so `desired_present` is `None` and
/// `latest_commit` is `commit_lead` before one refresh interval after the
/// tick's `now`. A missing or zero refresh interval on the tick uses
/// `fallback_refresh_interval`.
///
/// A browser shows only what is drawn during the callback, so the hints
/// [start each frame at the tick](PresentHints::starting_at_tick).
#[must_use]
pub fn present_hints_with_commit_lead(
    tick: &FrameTick,
    fallback_refresh_interval: Duration,
    commit_lead: Duration,
) -> PresentHints {
    PresentHints::for_tick(
        tick,
        PresentationTiming::PacingOnly,
        fallback_refresh_interval,
        commit_lead,
    )
    .starting_at_tick()
}

/// Returns display timing for a browser RAF tick and its target output.
///
/// Browsers report neither the display's refresh interval nor a
/// variable-refresh range, so `fallback_timing` is usually
/// [`default_display_timing`]. Variable timing from a host that knows the
/// output's range is kept as is. Fixed timing is refined from the tick when
/// it carries a refresh interval or predicted present, as described by
/// [`DisplayTiming::from_tick`].
#[must_use]
pub fn display_timing(tick: &FrameTick, fallback_timing: DisplayTiming) -> DisplayTiming {
    if fallback_timing.is_variable() {
        fallback_timing
    } else {
        DisplayTiming::from_tick(tick, fallback_timing.min_interval())
    }
}

/// Builds a [`FrameOpportunity`] from a browser RAF tick.
///
/// Browser RAF exposes no portable predicted present time or commit deadline,
/// so the opportunity is pacing-only. `fallback_timing` describes the output
/// as for [`display_timing`], and its fastest interval sets the commit
/// boundary, with [`default_commit_lead`].
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
    use super::*;
    use frameclock::{FrameBeginResult, FrameDemand, FrameDriver, OutputId};

    fn test_tick() -> FrameTick {
        FrameTick {
            now: HostTime(16_000),
            predicted_present: None,
            refresh_interval: None,
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    #[test]
    fn timebase_is_microsecond() {
        let tb = timebase();
        assert_eq!(tb.ticks_to_nanos(1), 1000);
        assert_eq!(tb.ticks_to_nanos(1_000_000), 1_000_000_000);
    }

    #[test]
    fn present_hints_are_pacing_only_and_start_at_the_tick() {
        let tick = test_tick();
        let hints = present_hints(&tick, DEFAULT_REFRESH_INTERVAL);

        assert_eq!(hints.desired_present(), None);
        assert!(hints.starts_at_tick());
        // A quarter refresh before the next one: 16_667 - 4_166 us.
        assert_eq!(hints.latest_commit(), HostTime(16_000 + 12_501));
    }

    #[test]
    fn present_hints_respect_explicit_commit_lead() {
        let hints =
            present_hints_with_commit_lead(&test_tick(), DEFAULT_REFRESH_INTERVAL, Duration(2_000));

        assert_eq!(hints.latest_commit(), HostTime(16_000 + 14_667));
    }

    #[test]
    fn present_hints_ignore_a_zero_refresh_interval() {
        let tick = FrameTick {
            refresh_interval: Some(Duration::ZERO),
            ..test_tick()
        };
        assert_eq!(
            present_hints(&tick, DEFAULT_REFRESH_INTERVAL),
            present_hints(&test_tick(), DEFAULT_REFRESH_INTERVAL)
        );
    }

    #[test]
    fn opportunity_uses_default_display_fallback() {
        let tick = test_tick();
        let opportunity = frame_opportunity(tick, default_display_timing());

        assert_eq!(opportunity.tick, tick);
        assert_eq!(
            opportunity.hints,
            present_hints(&tick, DEFAULT_REFRESH_INTERVAL)
        );
        assert_eq!(
            opportunity.display_timing,
            DisplayTiming::fixed(DEFAULT_REFRESH_INTERVAL)
        );
    }

    #[test]
    fn variable_fallback_timing_is_kept() {
        // A 48-144 Hz variable-refresh range in microseconds.
        let variable = DisplayTiming::variable(Duration(6_944), Duration(20_833), None);
        let tick = test_tick();
        let opportunity = frame_opportunity(tick, variable);

        assert_eq!(display_timing(&tick, variable), variable);
        assert_eq!(opportunity.display_timing, variable);
        // The fastest interval stands in for the refresh interval.
        assert_eq!(opportunity.hints, present_hints(&tick, Duration(6_944)));
    }

    #[test]
    fn driver_returns_ready_frames_during_the_callback() {
        for demand in [FrameDemand::INPUT, FrameDemand::ANIMATION] {
            let mut driver = FrameDriver::new(scheduler_config());
            driver.request(demand);
            let opportunity = frame_opportunity(test_tick(), default_display_timing());

            let FrameBeginResult::Ready(frame) = driver.begin_frame(opportunity).result else {
                panic!("{demand:?} must be ready in the callback");
            };
            assert_eq!(frame.plan().frame_start, test_tick().now);
        }
    }
}
