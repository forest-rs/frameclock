// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Apple display-link timing adapters for [`frameclock`].
//!
//! This crate owns Apple-specific timing adaptation. It converts
//! `CADisplayLink` and `CVDisplayLink` callbacks into [`FrameTick`] values,
//! exposes Mach absolute time as [`HostTime`], and converts display-link ticks
//! into [`FrameOpportunity`] values for callers that own a
//! [`frameclock::FrameDriver`].
//!
//! The `ca-display-link` feature (on by default) provides `CaDisplayLink`
//! and the `cv-display-link` feature provides `CvDisplayLink` on macOS. The
//! features are independent: enable either or both.
//!
//! It intentionally does not own `CALayer` trees, `CAMetalLayer` presentation,
//! renderers, windows, or app event-loop policy.
//!
//! This crate keeps its own implementation `no_std`, but the selected
//! Objective-C framework bindings currently require `std`. It is intended to be
//! validated on supported Apple targets, not on generic no-std targets such as
//! `x86_64-unknown-none`.

#![no_std]
#![expect(
    unsafe_code,
    reason = "Apple display-link adapters require Objective-C/CoreVideo FFI"
)]

extern crate alloc;

mod mach_time;

#[cfg(feature = "ca-display-link")]
mod ca_display_link;
#[cfg(all(feature = "cv-display-link", target_os = "macos"))]
mod cv_display_link;

#[cfg(feature = "ca-display-link")]
pub use ca_display_link::CaDisplayLink;
#[cfg(all(feature = "cv-display-link", target_os = "macos"))]
pub use cv_display_link::{CvDisplayLink, CvDisplayLinkError};

use frameclock::time::Timebase;
use frameclock::timing::{PresentationTiming, default_commit_lead};
use frameclock::{
    ActiveFrame, DisplayTiming, Duration, FrameOpportunity, FrameSubmission, FrameTick, HostTime,
    PresentHints,
};

/// Returns the current host time using Mach absolute time.
#[must_use]
pub fn now() -> HostTime {
    mach_time::now()
}

/// Returns the Mach absolute time [`Timebase`].
#[must_use]
pub fn timebase() -> Timebase {
    mach_time::timebase()
}

/// Computes [`PresentHints`] from an Apple display-link tick using
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

/// Computes [`PresentHints`] from an Apple display-link tick.
///
/// Fresh `CADisplayLink.targetTimestamp` / `CVDisplayLink` output times are
/// treated as predictive present targets. If the prediction is missing or
/// stale, the hint falls back to pacing-only timing aimed one refresh after
/// the tick, as described by [`PresentHints::for_tick`]. The scheduler applies
/// its own learned build margin later when it turns these platform facts into
/// a [`frameclock::timing::FramePlan`].
#[must_use]
pub fn present_hints_with_commit_lead(
    tick: &FrameTick,
    fallback_refresh_interval: Duration,
    commit_lead: Duration,
) -> PresentHints {
    PresentHints::for_tick(
        tick,
        PresentationTiming::Predictive,
        fallback_refresh_interval,
        commit_lead,
    )
}

/// Converts a Core Animation media time to [`HostTime`].
///
/// Core Animation and Metal report times in media seconds
/// (`CACurrentMediaTime`'s clock): a drawable's `presentedTime`, a command
/// buffer's `GPUEndTime`, a display link's timestamps. Use this to turn
/// them into [`frameclock::FrameReport`] facts. The conversion is relative
/// to a pair of clock samples taken now, so it assumes nothing about the
/// two clocks' epochs; call it soon after the time was reported. `None`
/// when `media_seconds` is not finite.
#[must_use]
pub fn media_time_to_host_time(media_seconds: f64) -> Option<HostTime> {
    let host_now = mach_time::now();
    let media_now = objc2_quartz_core::CACurrentMediaTime();
    mach_time::media_time_to_host_time(media_seconds, host_now, media_now, mach_time::timebase())
}

/// Returns display timing for an Apple display-link tick and target output.
///
/// Pass a variable [`DisplayTiming`] when the current output is known to be a
/// ProMotion/VRR display. The tick's current interval remains available as
/// [`FrameTick::refresh_interval`], but the scheduler needs the broader
/// per-output range to choose cadence. Fixed fallback timing is refined from
/// the tick when the display link reports an explicit refresh interval.
#[must_use]
pub fn display_timing(tick: &FrameTick, fallback_timing: DisplayTiming) -> DisplayTiming {
    if fallback_timing.is_variable() {
        fallback_timing
    } else {
        DisplayTiming::from_tick(tick, fallback_timing.min_interval())
    }
}

/// Builds a [`FrameOpportunity`] from an Apple display-link tick.
///
/// `fallback_timing` describes the current target output. Fixed fallback
/// timing is refined from the tick when possible; variable timing is preserved
/// so the scheduler can choose a cadence within the output's supported range.
#[must_use]
pub fn frame_opportunity(tick: FrameTick, fallback_timing: DisplayTiming) -> FrameOpportunity {
    let refresh_interval = tick.refresh_interval_or(fallback_timing.min_interval());
    frame_opportunity_with_commit_lead(tick, fallback_timing, default_commit_lead(refresh_interval))
}

/// Builds a [`FrameOpportunity`] with an explicit platform commit lead.
///
/// Use this when the host has a platform-specific estimate for how far before
/// the display-link target-present time work must be committed.
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

/// What presentation feedback an Apple display-link source can provide.
///
/// These are the display link's own facts. A host that presents through
/// Metal knows more, frame by frame: submit with
/// [`FrameSubmission::reported`] and report each drawable's
/// `presentedTime` (and the command buffer's `GPUEndTime`) through
/// [`frameclock::FrameDriver::report_frame`], converted with
/// [`media_time_to_host_time`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AppleFeedbackMode {
    /// Actual-present feedback arrives on a later display-link tick.
    ///
    /// This is the normal `CADisplayLink` path: the next callback's timestamp
    /// resolves the submitted frame through
    /// [`FrameTick::prev_actual_present`].
    ///
    /// That timestamp is when the display last refreshed, which is when a
    /// frame submitted on the previous tick was shown only if it reached
    /// the screen at the next refresh, as a Core Animation transaction
    /// does. A Metal renderer's frame is shown after its GPU work, often
    /// several refreshes later; for it this feedback is wrong.
    DeferredActualPresent,
    /// The display-link source does not provide actual-present feedback.
    ///
    /// Submitted frames complete immediately using commit timing as weaker
    /// pacing evidence.
    CommitOnly,
}

impl AppleFeedbackMode {
    /// Builds [`FrameSubmission`] facts for this feedback mode.
    #[must_use]
    pub const fn submission(self, submitted_at: HostTime) -> FrameSubmission {
        match self {
            Self::DeferredActualPresent => FrameSubmission::deferred(submitted_at),
            Self::CommitOnly => FrameSubmission::new(submitted_at, None),
        }
    }
}

/// Preferred Core Animation frame-rate range.
///
/// This is a platform-neutral mirror of `CAFrameRateRange` so code can compute
/// and test cadence requests without depending on Objective-C bindings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PreferredFrameRateRange {
    /// Minimum acceptable frames per second.
    pub minimum: f32,
    /// Maximum acceptable frames per second.
    pub maximum: f32,
    /// Preferred frames per second.
    pub preferred: f32,
}

/// Computes a Core Animation-style frame-rate range for a planned interval.
///
/// `frame_interval` is usually
/// [`FramePlan::frame_interval`](frameclock::timing::FramePlan::frame_interval). The
/// display timing should describe the current target output. For variable
/// displays with unknown direct granularity, the preferred rate may be a stable
/// divisor below the display's slowest direct interval; in that case the
/// returned minimum is widened down to the preferred rate so Core Animation can
/// accept the request.
#[must_use]
pub fn preferred_frame_rate_range(
    frame_interval: Duration,
    display_timing: DisplayTiming,
    timebase: Timebase,
) -> Option<PreferredFrameRateRange> {
    let preferred = fps_for_interval(frame_interval, timebase)?;
    let fastest = fps_for_interval(display_timing.min_interval(), timebase)?;
    let slowest = fps_for_interval(display_timing.max_interval(), timebase)?;
    let maximum = fastest.max(preferred);
    let minimum = preferred.min(slowest).min(maximum);
    Some(PreferredFrameRateRange {
        minimum,
        maximum,
        preferred: preferred.clamp(minimum, maximum),
    })
}

/// Computes the Core Animation preferred frame-rate range for a ready frame.
///
/// `fallback_timing` describes the current target output. Fixed fallback timing
/// is refined from the frame's originating tick; variable timing is preserved.
#[must_use]
pub fn preferred_frame_rate_range_for_frame(
    frame: &ActiveFrame,
    fallback_timing: DisplayTiming,
) -> Option<PreferredFrameRateRange> {
    preferred_frame_rate_range(
        frame.plan().frame_interval,
        display_timing(&frame.tick(), fallback_timing),
        timebase(),
    )
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "valid display rates are finite positive f32-sized values"
)]
fn fps_for_interval(interval: Duration, timebase: Timebase) -> Option<f32> {
    let nanos = timebase.ticks_to_nanos(interval.ticks());
    if nanos == 0 {
        return None;
    }
    let fps = 1_000_000_000.0 / nanos as f64;
    if !fps.is_finite() || fps <= 0.0 || fps > f64::from(f32::MAX) {
        return None;
    }
    Some(fps as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frameclock::OutputId;

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
    fn present_hints_with_prediction() {
        let hints = present_hints(&tick(Some(HostTime(20_000_000))), Duration(16_666_667));

        assert_eq!(hints.presentation_timing(), PresentationTiming::Predictive);
        assert_eq!(hints.desired_present(), Some(HostTime(20_000_000)));
        assert_eq!(hints.latest_commit(), HostTime(15_833_334));
    }

    #[test]
    fn present_hints_with_prediction_respects_explicit_commit_lead() {
        let hints = present_hints_with_commit_lead(
            &tick(Some(HostTime(20_000_000))),
            Duration(16_666_667),
            Duration(2_000_000),
        );

        assert_eq!(hints.presentation_timing(), PresentationTiming::Predictive);
        assert_eq!(hints.desired_present(), Some(HostTime(20_000_000)));
        assert_eq!(hints.latest_commit(), HostTime(18_000_000));
    }

    #[test]
    fn present_hints_without_prediction() {
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
    fn display_timing_keeps_variable_output_range() {
        let output_timing =
            DisplayTiming::variable(Duration(8_333_333), Duration(16_666_667), None);

        assert_eq!(
            display_timing(&tick(Some(HostTime(2_000_000))), output_timing),
            output_timing
        );
    }

    #[test]
    fn display_timing_refines_fixed_fallback_from_tick() {
        assert_eq!(
            display_timing(
                &tick(Some(HostTime(2_000_000))),
                DisplayTiming::fixed(Duration(8_333_333)),
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
            PresentationTiming::Predictive
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
    fn apple_feedback_mode_selects_submission_observation() {
        assert_eq!(
            AppleFeedbackMode::DeferredActualPresent.submission(HostTime(1)),
            FrameSubmission::deferred(HostTime(1))
        );
        assert_eq!(
            AppleFeedbackMode::CommitOnly.submission(HostTime(1)),
            FrameSubmission::new(HostTime(1), None)
        );
    }

    #[test]
    fn preferred_frame_rate_range_uses_display_bounds() {
        let range = preferred_frame_rate_range(
            Duration(16_666_667),
            DisplayTiming::variable(Duration(8_333_333), Duration(16_666_667), None),
            Timebase::NANOS,
        )
        .expect("range should be representable");

        assert!((range.minimum - 60.0).abs() < 0.01);
        assert!((range.maximum - 120.0).abs() < 0.01);
        assert!((range.preferred - 60.0).abs() < 0.01);
    }

    #[test]
    fn preferred_frame_rate_range_can_request_stable_divisor_below_direct_range() {
        let range = preferred_frame_rate_range(
            Duration(33_333_333),
            DisplayTiming::variable(Duration(8_333_333), Duration(16_666_667), None),
            Timebase::NANOS,
        )
        .expect("range should be representable");

        assert!((range.minimum - 30.0).abs() < 0.01);
        assert!((range.maximum - 120.0).abs() < 0.01);
        assert!((range.preferred - 30.0).abs() < 0.01);
    }
}
