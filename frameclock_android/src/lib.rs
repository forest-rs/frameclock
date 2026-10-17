// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Experimental Android Choreographer frame-timeline adaptation.
//!
//! This crate owns timeline selection and conversion into frameclock inputs.
//! The host owns Choreographer registration, native callback lifetimes, looper
//! dispatch, surfaces, and submission through Vulkan or `SurfaceControl`.
//! Copy the API 33 callback's timeline facts while its native data is valid.
//! All times here are Android `CLOCK_MONOTONIC` nanoseconds. The callback's
//! frame timestamp is synchronization metadata, not a fresh CPU clock read.
//!
//! Android's deadline means the frame must be ready, including GPU work. The
//! host supplies [`FrameBudget::ready_lead`] for everything between CPU queue
//! submission and readiness: GPU queueing, execution, and propagation. The
//! adapter subtracts that lead before creating a hard CPU commit deadline.
//!
//! Report when each frame was shown: submit with
//! [`FrameSubmission::reported`](frameclock::FrameSubmission::reported) and
//! pass each frame's present time, such as its `SurfaceControl` present fence
//! or `VK_GOOGLE_display_timing`'s actual present time, to
//! [`FrameDriver::report_frame`](frameclock::FrameDriver::report_frame). The
//! driver drains a frame queued behind one that missed `SurfaceFlinger`'s
//! deadline only from those times.
//!
//! [`LearnedBudget`] learns the [`FrameBudget`] from each frame's begin,
//! submission, and readiness times, and widens it when a frame submitted in
//! time still misses its timeline.
//!
//! Keep the selected timeline ID with the frame. [`SelectedTimeline::bind`]
//! refuses a plan that silently moves its native target or deadline. This is
//! deliberately executable API-pressure detection during the pre-release
//! experiment; native presentation must never use a mismatched vsync token.
//!
//! The API 33 fields and deadline semantics come from the
//! [NDK Choreographer reference](https://developer.android.com/ndk/reference/group/choreographer).
//! This crate does not register callbacks, query fences, or submit buffers.
//! Older Android releases can use [`legacy_opportunity`] without pretending
//! that a pacing callback proves presentation. The separate
//! `platform_timing_probe` example checks the real NDK accessor signatures.

#![no_std]

use frameclock::time::Timebase;
use frameclock::timing::FramePlan;
use frameclock::{
    DisplayTiming, Duration, FrameOpportunity, FrameTick, HostTime, OutputId, PresentHints,
    SchedulerConfig,
};

/// Nanosecond timebase shared by Choreographer and Android monotonic reads.
pub const TIMEBASE: Timebase = Timebase::NANOS;

/// Timing and identifier of one API 33 Choreographer candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameTimeline {
    vsync_id: i64,
    expected_present: HostTime,
    ready_deadline: HostTime,
}

/// Why native data or a frame plan cannot represent a selected timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimelineError {
    /// A timestamp or vsync identifier was negative, or readiness follows presentation.
    InvalidData,
    /// The callback has no valid preferred timeline index.
    InvalidPreferredIndex,
    /// No supplied candidate can meet the host's readiness budget.
    NoFeasibleTimeline,
    /// The required readiness lead cannot be subtracted from the deadline.
    BudgetOutOfRange,
    /// The scheduler moved the target to a time not named by the chosen vsync ID.
    TargetMoved,
    /// The scheduler granted submission time beyond the selected resource's limit.
    DeadlineExtended,
    /// The plan describes a frame that must wait for a later opportunity.
    WaitingForLaterFrame,
}

impl FrameTimeline {
    /// Copies one candidate from the NDK's signed nanosecond fields.
    ///
    /// # Errors
    /// Returns [`TimelineError::InvalidData`] for invalid timestamps or identity.
    pub fn new(
        vsync_id: i64,
        expected_present_ns: i64,
        ready_deadline_ns: i64,
    ) -> Result<Self, TimelineError> {
        let expected_present =
            HostTime(u64::try_from(expected_present_ns).map_err(|_| TimelineError::InvalidData)?);
        let ready_deadline =
            HostTime(u64::try_from(ready_deadline_ns).map_err(|_| TimelineError::InvalidData)?);
        if vsync_id < 0 || ready_deadline > expected_present {
            return Err(TimelineError::InvalidData);
        }
        Ok(Self {
            vsync_id,
            expected_present,
            ready_deadline,
        })
    }

    /// Returns the native timeline token. It is distinct from a content-frame ID.
    #[must_use]
    pub const fn vsync_id(self) -> i64 {
        self.vsync_id
    }

    /// Returns the native predicted presentation time.
    #[must_use]
    pub const fn expected_present(self) -> HostTime {
        self.expected_present
    }

    /// Returns when CPU and GPU work must be ready for the selected timeline.
    #[must_use]
    pub const fn ready_deadline(self) -> HostTime {
        self.ready_deadline
    }
}

/// Learns a [`FrameBudget`] from when each frame began, was submitted, and
/// was ready.
///
/// Each part is estimated as round-trip times are for TCP's retransmission
/// timer (RFC 6298): a smoothed mean plus four smoothed deviations, and at
/// least half a millisecond more however steady the frames are. Too
/// short a budget picks a timeline the frame cannot make, and it is shown a
/// refresh late; too long a budget picks a later timeline than needed, which
/// costs latency. The deviation term keeps the budget just past the spread
/// the frames have shown.
///
/// Feed it every frame with [`observe`](Self::observe): when its CPU work
/// began, when it was submitted, and when it was ready. A frame is ready
/// when its GPU work finished and its buffer could be latched: its acquire
/// fence's signal time in `SurfaceControl` transaction stats, EGL's
/// `EGL_RENDERING_COMPLETE_TIME_ANDROID`, or a Vulkan fence and timestamp,
/// in `CLOCK_MONOTONIC` nanoseconds. Call [`missed`](Self::missed) for a
/// frame submitted by its CPU deadline that still missed its timeline: the
/// budget widens even when the times it saw looked in time.
#[derive(Clone, Copy, Debug)]
pub struct LearnedBudget {
    build: Estimate,
    ready_lead: Estimate,
}

/// A smoothed mean and mean deviation of one duration, in nanoseconds.
#[derive(Clone, Copy, Debug)]
struct Estimate {
    mean: u64,
    deviation: u64,
    /// How much missed frames have widened it, fading as frames go by.
    missed_margin: u64,
    seen: bool,
}

/// Headroom an estimate keeps however steady its frames, as RFC 6298 keeps
/// its clock granularity: frames that never vary still vary a little.
const MIN_HEADROOM_NS: u64 = 500_000;
/// How far a missed frame widens the ready lead at least.
const MISSED_MARGIN_NS: u64 = 1_000_000;
/// How far missed frames widen it at most.
const MAX_MISSED_MARGIN_NS: u64 = 16_000_000;
/// The longest duration believed. A longer one comes from clocks mixed up
/// or a sentinel taken for a time, not from a frame.
const MAX_SAMPLE_NS: u64 = 1_000_000_000;

impl Estimate {
    const fn new(initial: Duration) -> Self {
        Self {
            mean: initial.ticks(),
            deviation: 0,
            missed_margin: 0,
            seen: false,
        }
    }

    fn observe(&mut self, sample: u64) {
        if self.seen {
            // RFC 6298: deviation first, against the old mean, by 1/4; then
            // the mean, by 1/8.
            let off = self.mean.abs_diff(sample);
            self.deviation = self.deviation.saturating_mul(3).saturating_add(off) / 4;
            self.mean = self.mean.saturating_mul(7).saturating_add(sample) / 8;
        } else {
            // A quarter of the first frame as its deviation: room to spare
            // until the frames show their spread.
            self.mean = sample;
            self.deviation = sample / 4;
            self.seen = true;
        }
        // A miss's margin fades over about a second of frames at 60 Hz.
        self.missed_margin -= self.missed_margin.div_ceil(64);
    }

    fn widen(&mut self) {
        self.missed_margin = self
            .missed_margin
            .saturating_mul(2)
            .clamp(MISSED_MARGIN_NS, MAX_MISSED_MARGIN_NS);
    }

    fn value(self) -> Duration {
        let headroom = if self.seen {
            self.deviation.saturating_mul(4).max(MIN_HEADROOM_NS)
        } else {
            0
        };
        Duration(
            self.mean
                .saturating_add(headroom)
                .saturating_add(self.missed_margin),
        )
    }
}

impl LearnedBudget {
    /// Starts from `initial`, used as is until the first frame is observed.
    #[must_use]
    pub const fn new(initial: FrameBudget) -> Self {
        Self {
            build: Estimate::new(initial.build),
            ready_lead: Estimate::new(initial.ready_lead),
        }
    }

    /// Learns from one frame: when its CPU work began, when it was
    /// submitted, and when it was ready. Returns whether it learned.
    ///
    /// Times out of order, or a second or more apart, come from clocks
    /// mixed up or a readiness time that was not available, such as EGL's
    /// pending or invalid sentinels, so the frame teaches nothing.
    pub fn observe(
        &mut self,
        build_start: HostTime,
        submitted_at: HostTime,
        ready_at: HostTime,
    ) -> bool {
        if submitted_at < build_start || ready_at < submitted_at {
            return false;
        }
        let build = submitted_at.saturating_duration_since(build_start);
        let ready_lead = ready_at.saturating_duration_since(submitted_at);
        if build.ticks() >= MAX_SAMPLE_NS || ready_lead.ticks() >= MAX_SAMPLE_NS {
            return false;
        }
        self.build.observe(build.ticks());
        self.ready_lead.observe(ready_lead.ticks());
        true
    }

    /// Widens the ready lead after a frame submitted by its CPU deadline
    /// missed its timeline anyway.
    ///
    /// The lead widens by a millisecond, or twice as much as the last miss
    /// widened it, up to 16 ms, and the widening fades over about the next
    /// 64 frames: a miss the times did not explain may be one they keep not
    /// explaining.
    pub fn missed(&mut self) {
        self.ready_lead.widen();
    }

    /// Returns the budget to select the next timeline with.
    #[must_use]
    pub fn budget(&self) -> FrameBudget {
        FrameBudget {
            build: self.build.value(),
            ready_lead: self.ready_lead.value(),
        }
    }
}

/// Host estimates used to select a feasible Android timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameBudget {
    /// CPU work from callback entry until queue submission.
    pub build: Duration,
    /// Queueing, GPU execution, and propagation from submission until readiness.
    pub ready_lead: Duration,
}

/// One callback's candidate timelines, borrowed from host-owned copied data.
#[derive(Clone, Copy, Debug)]
pub struct FrameTimelines<'a> {
    /// Callback frame timestamp, retained as synchronization metadata.
    pub frame_time: HostTime,
    /// Platform-preferred index returned by Choreographer.
    pub preferred_index: usize,
    /// Copied native candidates. They need not be sorted by presentation time.
    pub timelines: &'a [FrameTimeline],
}

impl FrameTimelines<'_> {
    /// Selects the preferred candidate if feasible, otherwise the earliest feasible one.
    ///
    /// `now` is a fresh monotonic CPU reading in [`TIMEBASE`], independent of
    /// the callback's older `frame_time`. `display_timing` comes from output
    /// capabilities; spacing between future candidates is not a VRR range.
    ///
    /// # Errors
    /// Returns a preferred-index, budget, or feasibility error.
    pub fn select(
        self,
        now: HostTime,
        budget: FrameBudget,
        refresh_interval: Option<Duration>,
        display_timing: DisplayTiming,
        output: OutputId,
    ) -> Result<SelectedTimeline, TimelineError> {
        let preferred = self
            .timelines
            .get(self.preferred_index)
            .ok_or(TimelineError::InvalidPreferredIndex)?;
        let ready_at = now
            .checked_add(budget.build)
            .and_then(|time| time.checked_add(budget.ready_lead))
            .ok_or(TimelineError::BudgetOutOfRange)?;
        let feasible = |timeline: &&FrameTimeline| {
            timeline.ready_deadline >= ready_at && timeline.expected_present >= now
        };
        let timeline = if feasible(&preferred) {
            *preferred
        } else {
            *self
                .timelines
                .iter()
                .filter(feasible)
                .min_by_key(|timeline| timeline.expected_present)
                .ok_or(TimelineError::NoFeasibleTimeline)?
        };
        let commit_deadline = timeline
            .ready_deadline
            .checked_sub(budget.ready_lead)
            .ok_or(TimelineError::BudgetOutOfRange)?;
        let tick = FrameTick {
            now,
            predicted_present: Some(timeline.expected_present),
            refresh_interval,
            output,
            prev_actual_present: None,
        };
        let hints = PresentHints::selected_timeline(timeline.expected_present, commit_deadline);
        Ok(SelectedTimeline {
            timeline,
            opportunity: FrameOpportunity::new(tick, hints, display_timing),
        })
    }
}

/// A chosen native timeline together with its exact CPU submission constraints.
#[derive(Clone, Copy, Debug)]
pub struct SelectedTimeline {
    timeline: FrameTimeline,
    opportunity: FrameOpportunity,
}

impl SelectedTimeline {
    /// Returns the selected native timeline and its original readiness deadline.
    #[must_use]
    pub const fn timeline(self) -> FrameTimeline {
        self.timeline
    }

    /// Returns the opportunity to feed to [`frameclock::FrameDriver::begin_frame`].
    #[must_use]
    pub const fn frame_opportunity(self) -> FrameOpportunity {
        self.opportunity
    }

    /// Binds a ready plan to the current native timeline's presentation token.
    ///
    /// Call this on the candidate from the callback that actually releases the
    /// frame. A retained plan from an older callback must be rebound by core
    /// scheduling before a different native timeline can be submitted.
    ///
    /// # Errors
    /// Returns an error when target identity, commit limit, or readiness differs.
    pub fn bind(self, plan: &FramePlan) -> Result<SubmissionTiming, TimelineError> {
        if plan.frame_start > self.opportunity.tick.now {
            return Err(TimelineError::WaitingForLaterFrame);
        }
        if plan.target_present != Some(self.timeline.expected_present) {
            return Err(TimelineError::TargetMoved);
        }
        if plan.commit_deadline > self.opportunity.hints.latest_commit() {
            return Err(TimelineError::DeadlineExtended);
        }
        Ok(SubmissionTiming {
            vsync_id: self.timeline.vsync_id,
            expected_present: self.timeline.expected_present,
            commit_deadline: plan.commit_deadline,
        })
    }
}

/// Presentation metadata retained alongside a frame's renderer submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubmissionTiming {
    /// Native timeline token for a presentation path that accepts one.
    pub vsync_id: i64,
    /// Expected native present time; Vulkan requests use this host-domain time.
    pub expected_present: HostTime,
    /// Latest CPU submission after reserving readiness work.
    pub commit_deadline: HostTime,
}

/// Returns a scheduler configuration in Android monotonic nanoseconds.
#[must_use]
pub const fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig::predictive(TIMEBASE)
}

/// Adapts an older callback that supplies pacing without frame timelines.
///
/// No actual present or predictive target is inferred from its frame timestamp.
#[must_use]
pub fn legacy_opportunity(
    now: HostTime,
    refresh_interval: Duration,
    output: OutputId,
) -> FrameOpportunity {
    let mut opportunity = FrameOpportunity::pacing_only(now, refresh_interval, output);
    opportunity.hints = opportunity.hints.starting_at_tick();
    opportunity
}

#[cfg(test)]
mod tests;
