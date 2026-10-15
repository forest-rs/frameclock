// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! X11 timing adapters for [`frameclock`], built on the Present extension.
//!
//! The crate assumes a server with the Present extension and has no
//! pre-Present fallbacks. It reads the Present unadjusted-system-time clock as
//! [`HostTime`], converts `PresentCompleteNotify` completions into
//! [`FrameTick`] values via [`PresentTicker`], and carries per-pixmap Present
//! feedback as [`PresentEvent`] values.
//!
//! It intentionally does not own X11 windows, connections, event queues, or
//! protocol dispatch. Protocol I/O belongs to hosts and backend crates; this
//! crate owns the timing bookkeeping those hosts feed and poll, and references
//! no `x11rb` type.
//!
//! # Core flow
//!
//! ```text
//! PresentNotifyMSC request  -> PresentTicker (claims the in-flight slot)
//! PresentCompleteNotify      -> PresentTicker -> FrameTick
//! PresentCompleteNotify(px)  -> PresentEvent  -> PresentEventQueue (backend path)
//! ```
//!
//! A host's frame loop has this shape:
//!
//! ```rust,ignore
//! use frameclock::OutputId;
//! use frameclock_x11::{Clock, PresentTicker, frame_opportunity};
//!
//! let mut ticker = PresentTicker::new();
//!
//! // Arm the next vblank notify before sending a PresentNotifyMSC request:
//! if ticker.mark_notify_requested() {
//!     // present_notify_msc(window, serial, 0, 1, 0)  -- notify at the next msc
//! }
//!
//! // When the matching PresentCompleteNotify (kind NotifyMSC) arrives:
//! ticker.on_complete_notify(Clock::Monotonic, event.ust, event.msc);
//!
//! // After dispatch, drain the queued ticks and build opportunities:
//! while let Some(tick) = ticker.poll_tick() {
//!     let opportunity = frame_opportunity(tick, fallback_timing);
//!     _ = opportunity;
//! }
//! ```
//!
//! # The `ust` clock domain
//!
//! Present reports `ust` (unadjusted system time) in microseconds. On Linux with
//! DRI3/Present that is `CLOCK_MONOTONIC`, which [`Clock::Monotonic`] reads in
//! nanoseconds, so `ust`-derived timestamps and clock reads share one domain —
//! the same domain a host should stamp input events in. All [`HostTime`] values
//! are nanosecond ticks.
//!
//! This requires a local Linux server whose UST clock shares the client's
//! `CLOCK_MONOTONIC` domain. Remote servers and servers with another UST domain
//! require clock synchronization outside this adapter.
//!
//! # Pacing while a swapchain presents
//!
//! `PresentNotifyMSC` reads the window's vblank clock without presenting
//! anything, so the tick stream stays vsync-aligned even when a swapchain
//! owner such as wgpu/Vulkan performs the actual scan-out. Per-submission
//! feedback — which vblank a frame landed on, flip vs. copy, buffer idleness —
//! only reaches a presenter that itself submits `PresentPixmap`; the
//! [`PresentEvent`] queue models that feedback for such a presenter.
//! `NotifyMSC` ticks leave [`FrameTick::prev_actual_present`] unset: a vblank
//! notification is not evidence that an application frame was shown. Use
//! [`frameclock::FrameSubmission::new`] with no actual timestamp when a swapchain
//! owns presentation. Hosts submitting pixmaps can use
//! [`frameclock::FrameSubmission::reported`] and correlate completion serials
//! with frame indices, checking [`PresentEvent::actual_present`] and handling
//! skipped frames before calling [`frameclock::FrameDriver::report_frame`].
//!
//! [`HostTime`]: frameclock::HostTime

#![no_std]

extern crate alloc;

mod presentation;
mod queue;
mod tick;
mod time;

pub use presentation::{PresentEvent, PresentEventQueue, SubmissionId};
pub use tick::PresentTicker;
pub use time::{Clock, now, timebase, ust_to_host_time};

use frameclock::timing::{PresentationTiming, default_commit_lead};
use frameclock::{
    DisplayTiming, Duration, FrameOpportunity, FrameTick, PresentHints, SchedulerConfig,
};

/// Returns the scheduler configuration for X11 Present ticks.
///
/// This is [`SchedulerConfig::estimated`] in this crate's nanosecond
/// [`timebase`]: once two completions establish a cadence,
/// [`frame_opportunity`] builds estimated hints from a client-side
/// extrapolation of the refresh grid.
#[must_use]
pub const fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig::estimated(timebase())
}

/// Returns fixed 60 Hz display timing in nanosecond ticks.
///
/// Pass it to [`frame_opportunity`] when nothing better is known about the
/// output: fixed timing is refined from each tick's refresh interval once
/// two completions establish the cadence. For a variable-refresh output, pass
/// [`DisplayTiming::variable`] with its range instead.
#[must_use]
pub const fn default_display_timing() -> DisplayTiming {
    DisplayTiming::fixed(Duration::from_nanos(16_666_667, timebase()))
}

/// Computes [`PresentHints`] from an X11 [`FrameTick`] using
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

/// Computes [`PresentHints`] from an X11 [`FrameTick`].
///
/// A predicted present time derived from Present `ust`/`msc` (see
/// [`PresentTicker`]) is a client-side extrapolation of the vblank grid, so it
/// is reported as estimated timing (`PresentationTiming::Estimated`) rather than
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

/// Returns display timing for an X11 [`FrameTick`] and its target output.
///
/// `fallback_timing` describes the output the tick was made for. Pass
/// [`DisplayTiming::variable`] when the output is variable-refresh capable (its
/// `vrr_capable` output property) and the window has opted in with
/// `_VARIABLE_REFRESH`. Present does not report the range, so it comes from the
/// host, for example from the `EDID` output property. Variable timing is kept
/// as is, because on such an output the tick's refresh interval is only the
/// last measured `msc` gap. Fixed timing is refined from the tick:
/// [`FrameTick::refresh_interval`] first, then the predicted-present delta,
/// then `fallback_timing`'s interval.
#[must_use]
pub fn display_timing(tick: &FrameTick, fallback_timing: DisplayTiming) -> DisplayTiming {
    if fallback_timing.is_variable() {
        fallback_timing
    } else {
        DisplayTiming::from_tick(tick, fallback_timing.min_interval())
    }
}

/// Builds a [`FrameOpportunity`] from an X11 Present tick.
///
/// `fallback_timing` describes the tick's output, as for [`display_timing`].
/// Its fastest interval stands in for the refresh interval until
/// a second completion has established a cadence.
#[must_use]
pub fn frame_opportunity(tick: FrameTick, fallback_timing: DisplayTiming) -> FrameOpportunity {
    let refresh_interval = tick.refresh_interval_or(fallback_timing.min_interval());
    frame_opportunity_with_commit_lead(tick, fallback_timing, default_commit_lead(refresh_interval))
}

/// Builds a [`FrameOpportunity`] with an explicit platform commit lead.
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
        present_hints_with_commit_lead,
    };
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
    }

    #[test]
    fn display_timing_prefers_reported_refresh_interval() {
        assert_eq!(
            display_timing(
                &tick(Some(HostTime(2_000_000))),
                DisplayTiming::fixed(Duration(8_333_333))
            ),
            DisplayTiming::fixed(Duration(16_666_667))
        );
    }

    #[test]
    fn frame_opportunity_pairs_tick_hints_and_display_timing() {
        let tick = tick(Some(HostTime(20_000_000)));
        let opportunity = frame_opportunity(tick, DisplayTiming::fixed(Duration(8_333_333)));

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

    #[test]
    fn default_display_timing_is_60_hz() {
        assert_eq!(
            default_display_timing(),
            DisplayTiming::fixed(Duration(16_666_667))
        );
    }

    #[test]
    fn variable_fallback_timing_is_kept() {
        // A 48-144 Hz variable-refresh range.
        let variable = DisplayTiming::variable(Duration(6_944_444), Duration(20_833_333), None);
        let tick = tick(Some(HostTime(20_000_000)));
        let opportunity = frame_opportunity(tick, variable);

        assert_eq!(display_timing(&tick, variable), variable);
        assert_eq!(opportunity.display_timing, variable);
        // The tick's own interval still sets the commit lead.
        assert_eq!(opportunity.hints, present_hints(&tick, Duration(6_944_444)));
    }
}
