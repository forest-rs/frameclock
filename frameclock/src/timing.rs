// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Capability-graded timing model.
//!
//! This module defines the types that flow between backends and the scheduler:
//!
//! - [`PresentationTiming`] — whether presentation timestamps are available
//! - [`DisplayTiming`] — fixed/variable display timing constraints
//! - [`FrameTick`] — a frame opportunity delivered by the backend
//! - [`FrameOpportunity`] — tick, presentation hints, and display timing
//! - [`FramePlan`] — what the engine uses to evaluate the scene for a frame
//! - [`PresentHints`] — submission constraints from the backend
//! - [`PresentFeedback`] — post-submit observations fed back to the scheduler
//!
//! # Data flow
//!
//! Each frame follows a pipeline through these types:
//!
//! 1. The backend produces a [`FrameTick`] from a platform callback (e.g.
//!    `CADisplayLink`, `requestAnimationFrame`).
//! 2. The backend computes [`PresentHints`] from the tick and platform
//!    knowledge (deadlines, desired present time).
//! 3. The host combines those facts with [`DisplayTiming`] for the tick's
//!    current target output into a [`FrameOpportunity`].
//! 4. [`Scheduler::plan()`](crate::scheduler::Scheduler::plan) consumes the
//!    opportunity plus [`FrameDemand`] to produce a [`FramePlan`] with a frame
//!    start time, sampling time, target presentation time, and commit deadline.
//! 5. The application schedules frame work at
//!    [`frame_start`](FramePlan::frame_start), uses the plan's
//!    [`sample_time`](FramePlan::sample_time) to evaluate animation/simulation
//!    state, and builds/submits the frame.
//! 6. After submission, the backend constructs [`PresentFeedback`] from
//!    timing observations and feeds it back to
//!    [`Scheduler::observe()`](crate::scheduler::Scheduler::observe) to
//!    adapt pipeline depth and safety margins.

pub use crate::demand::{FrameDemand, FrameDemandClass};
use crate::output::OutputId;
use crate::time::{Duration, HostTime};

/// Display timing constraints supplied with a [`FrameOpportunity`].
///
/// Backends or host adapters construct this from platform display facts and
/// pass it to [`FrameOpportunity::new`]. The scheduler uses it to choose the
/// delivery interval for a frame.
///
/// Treat this as a per-output, per-opportunity input. If a window or surface is
/// dragged to another display, or if the platform reports a new refresh mode,
/// the host should build the next [`FrameOpportunity`] with timing for the new
/// target output. Do not cache one global `DisplayTiming` unless the backend
/// truly has only one fixed timing model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayTiming {
    min_interval: Duration,
    max_interval: Duration,
    granularity: Option<Duration>,
}

impl DisplayTiming {
    /// Creates fixed-rate display timing for the current target output.
    ///
    /// Use this when the backend knows only the current refresh interval, or
    /// when the platform does not expose a useful variable-refresh range.
    #[inline]
    #[must_use]
    pub const fn fixed(interval: Duration) -> Self {
        Self {
            min_interval: interval,
            max_interval: interval,
            granularity: Some(interval),
        }
    }

    /// Creates variable-refresh display timing for the current target output.
    ///
    /// `min_interval` is the fastest direct display interval, `max_interval`
    /// is the slowest direct display interval, and `granularity` describes a
    /// fixed direct-interval step when the platform exposes one.
    ///
    /// `None` means the backend knows a variable range exists, but does not
    /// know the direct interval granularity or cannot request arbitrary direct
    /// presentation durations. In that case the scheduler uses stable
    /// fixed-rate-like multiples of `min_interval` instead of inventing
    /// unsupported direct intervals inside the range.
    #[inline]
    #[must_use]
    pub const fn variable(
        min_interval: Duration,
        max_interval: Duration,
        granularity: Option<Duration>,
    ) -> Self {
        let max_interval = if max_interval.0 < min_interval.0 {
            min_interval
        } else {
            max_interval
        };
        Self {
            min_interval,
            max_interval,
            granularity,
        }
    }

    /// Creates fixed-rate display timing from a tick's timing facts.
    ///
    /// Prefer [`FrameTick::refresh_interval`] when it is present because it is
    /// the backend's explicit cadence fact for this tick's output. If no
    /// refresh interval is reported, this falls back to the delta from
    /// `tick.now` to [`FrameTick::predicted_present`] and finally to
    /// `fallback_interval`.
    #[inline]
    #[must_use]
    pub fn from_tick(tick: &FrameTick, fallback_interval: Duration) -> Self {
        let interval = tick
            .refresh_interval
            .filter(|interval| !interval.is_zero())
            .or_else(|| {
                tick.predicted_present
                    .map(|present| present.saturating_duration_since(tick.now))
                    .filter(|interval| !interval.is_zero())
            })
            .unwrap_or(fallback_interval);
        Self::fixed(interval)
    }

    /// Fastest direct display interval.
    #[inline]
    #[must_use]
    pub const fn min_interval(self) -> Duration {
        self.min_interval
    }

    /// Slowest direct display interval.
    #[inline]
    #[must_use]
    pub const fn max_interval(self) -> Duration {
        self.max_interval
    }

    /// Optional direct interval granularity for displays with known steps.
    ///
    /// `None` is conservative: the scheduler treats direct interval
    /// granularity as unknown and chooses stable multiples of
    /// [`Self::min_interval`].
    #[inline]
    #[must_use]
    pub const fn granularity(self) -> Option<Duration> {
        self.granularity
    }

    /// Returns true when the display exposes a range of direct intervals.
    #[inline]
    #[must_use]
    pub const fn is_variable(self) -> bool {
        self.min_interval.0 != self.max_interval.0
    }

    /// Chooses a stable delivery interval that can contain `needed`.
    ///
    /// Fixed-rate displays and variable displays without known direct
    /// granularity choose multiples of [`Self::min_interval`]. Variable
    /// displays with an explicit granularity choose the first supported direct
    /// interval step that can contain the needed work.
    #[must_use]
    pub fn choose_interval(self, needed: Duration) -> Duration {
        if self.min_interval.is_zero() {
            return needed;
        }

        if !self.is_variable() {
            return round_up_to_multiple(needed, self.min_interval);
        }

        if needed > self.max_interval {
            return round_up_to_multiple(needed, self.min_interval);
        }

        let clamped = clamp_duration(needed, self.min_interval, self.max_interval);
        match self.granularity {
            Some(step) if !step.is_zero() => clamp_duration(
                round_up_to_multiple(clamped, step),
                self.min_interval,
                self.max_interval,
            ),
            Some(_) => clamped,
            None => round_up_to_multiple(clamped, self.min_interval),
        }
    }
}

fn clamp_duration(value: Duration, min_value: Duration, max_value: Duration) -> Duration {
    value.max(min_value).min(max_value)
}

fn round_up_to_multiple(needed: Duration, interval: Duration) -> Duration {
    if interval.is_zero() {
        return needed;
    }
    let count = needed
        .ticks()
        .saturating_add(interval.ticks().saturating_sub(1))
        / interval.ticks();
    interval.saturating_mul(count.max(1))
}

/// How [`PresentHints`] should be interpreted for one planning request.
///
/// This is attached to [`PresentHints`], not [`FrameTick`], because it is part
/// of the backend's scheduling contract: it tells the scheduler whether
/// [`PresentHints::desired_present`] may be reported as presentation truth.
///
/// Adaptive scheduling policy is still selected by
/// [`SchedulerConfig`](crate::scheduler::SchedulerConfig). For example, hosts
/// should normally pair [`PresentationTiming::Estimated`] hints with
/// [`SchedulerConfig::estimated`](crate::scheduler::SchedulerConfig::estimated).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresentationTiming {
    /// Strong predicted present time available (e.g. macOS `CVDisplayLink`).
    Predictive,
    /// Vsync-ish timing but less strict (e.g. Android Choreographer).
    Estimated,
    /// No reliable present time; frame pacing only (e.g. Web `rAF`, X11 fallback).
    PacingOnly,
}

impl PresentationTiming {
    /// Returns whether this timing mode can report a target present time.
    #[inline]
    #[must_use]
    pub const fn has_target_present(self) -> bool {
        matches!(self, Self::Predictive | Self::Estimated)
    }
}

/// Platform timing facts used to create a [`FrameOpportunity`].
///
/// Backends produce a `FrameTick` each time a new frame can be submitted. Not
/// all fields are populated on every platform: [`Option`] fields reflect the
/// capability-graded timing model. Hosts pass this to
/// [`FrameOpportunity::new`] with [`PresentHints`] and [`DisplayTiming`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameTick {
    /// Current host time when the tick was generated.
    pub now: HostTime,
    /// Predicted time when pixels will be presented, if known.
    pub predicted_present: Option<HostTime>,
    /// Display refresh interval, if known.
    pub refresh_interval: Option<Duration>,
    /// Which output this tick is for.
    pub output: OutputId,
    /// Actual present time of the *previous* frame, if the backend can report
    /// it (e.g. from `CADisplayLink.timestamp`).
    pub prev_actual_present: Option<HostTime>,
}

impl FrameTick {
    /// Returns the tick's refresh interval, or `fallback` when the tick
    /// reports none or a zero interval.
    #[inline]
    #[must_use]
    pub fn refresh_interval_or(&self, fallback: Duration) -> Duration {
        self.refresh_interval
            .filter(|interval| !interval.is_zero())
            .unwrap_or(fallback)
    }
}

/// Returns the default platform commit lead for a refresh interval: a
/// quarter of it.
///
/// A predicted present time describes a refresh slot, not a promise that app
/// work can be committed at the last possible tick. Platform adapters commit
/// this far ahead of it so [`PresentHints::latest_commit`] remains a commit
/// boundary, while the scheduler still owns learned app build margins.
#[inline]
#[must_use]
pub const fn default_commit_lead(refresh_interval: Duration) -> Duration {
    refresh_interval.div_u64(4)
}

/// Returns the first refresh at or after `now` on the grid of `last_refresh`.
///
/// `last_refresh` is advanced by whole refresh intervals until it reaches
/// `now`, so a host that was idle for many refreshes still predicts a time on
/// the display's refresh grid rather than one in the past. A `last_refresh`
/// at or after `now` is returned as is. Returns `None` for a zero
/// `refresh_interval` and saturates at `HostTime(u64::MAX)`.
#[inline]
#[must_use]
pub const fn next_refresh(
    last_refresh: HostTime,
    refresh_interval: Duration,
    now: HostTime,
) -> Option<HostTime> {
    if refresh_interval.is_zero() {
        return None;
    }
    let elapsed = now.ticks().saturating_sub(last_refresh.ticks());
    let advance = elapsed
        .div_ceil(refresh_interval.ticks())
        .saturating_mul(refresh_interval.ticks());
    Some(HostTime(last_refresh.ticks().saturating_add(advance)))
}

/// Platform frame facts passed to the scheduler or retained driver.
///
/// Hosts construct this from the current display/frame callback. It packages
/// the platform tick, backend submission constraints, and output timing model
/// that the scheduler needs to plan a frame.
/// [`Self::display_timing`] should describe the same output or surface named by
/// [`FrameTick::output`].
///
/// Pass this to [`FrameDriver::begin_frame`](crate::FrameDriver::begin_frame)
/// when using the retained lifecycle API, or to
/// [`Scheduler::plan`](crate::scheduler::Scheduler::plan) when using the
/// lower-level scheduler directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameOpportunity {
    /// Platform frame opportunity.
    pub tick: FrameTick,
    /// Backend submission constraints for the opportunity.
    pub hints: PresentHints,
    /// Display timing constraints for the tick's current target output.
    ///
    /// Refresh this when the window/surface moves between displays or when the
    /// backend reports a changed display mode.
    pub display_timing: DisplayTiming,
}

impl FrameOpportunity {
    /// Creates a frame opportunity to pass to `FrameDriver` or `Scheduler`.
    #[inline]
    #[must_use]
    pub const fn new(tick: FrameTick, hints: PresentHints, display_timing: DisplayTiming) -> Self {
        Self {
            tick,
            hints,
            display_timing,
        }
    }

    /// Creates a pacing-only frame opportunity.
    ///
    /// Use this from hosts that have a monotonic clock and a nominal refresh
    /// interval but no reliable predicted present timestamp. The returned
    /// opportunity uses:
    ///
    /// - [`PresentationTiming::PacingOnly`],
    /// - no predicted or desired present time,
    /// - `latest_commit = now + refresh_interval`, saturating at `u64::MAX`,
    /// - [`DisplayTiming::fixed`] with `refresh_interval`.
    #[inline]
    #[must_use]
    pub fn pacing_only(now: HostTime, refresh_interval: Duration, output: OutputId) -> Self {
        let tick = FrameTick {
            now,
            predicted_present: None,
            refresh_interval: Some(refresh_interval),
            output,
            prev_actual_present: None,
        };
        let hints = PresentHints::pacing_only(
            now.checked_add(refresh_interval)
                .unwrap_or(HostTime(u64::MAX)),
        );
        Self::new(tick, hints, DisplayTiming::fixed(refresh_interval))
    }
}

/// Scheduler output that tells the host how to prepare one frame.
///
/// Produced by the [`Scheduler`](crate::scheduler::Scheduler) from a
/// [`FrameOpportunity`] and [`FrameDemand`]. With [`FrameDriver`](crate::FrameDriver),
/// hosts usually read this from [`ActiveFrame::plan`](crate::ActiveFrame::plan)
/// after [`FrameBeginResult::Ready`](crate::FrameBeginResult::Ready).
///
/// All engine evaluation and content selection should be driven by the times in
/// this plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FramePlan {
    /// Demand that selected this frame.
    pub demand: FrameDemand,
    /// Scheduler-selected delivery interval for this frame.
    pub frame_interval: Duration,
    /// Time applications should wake or start app-side frame work.
    ///
    /// This is derived from the commit deadline and scheduler safety margin.
    /// It is clamped to the originating tick time, so callers can request
    /// redraw immediately when this time is already due. Input frames start
    /// at the tick time. Frames whose hints
    /// [start at a tick](PresentHints::starting_at_tick) start at the tick
    /// time, or half a refresh before the later tick that the selected
    /// cadence waits for.
    pub frame_start: HostTime,
    /// Whether the frame starts at the first tick from
    /// [`frame_start`](Self::frame_start) on, rather than when the host
    /// wakes at it: whether its hints
    /// [start at a tick](PresentHints::starting_at_tick).
    pub starts_at_tick: bool,
    /// Time applications should sample animation and simulation state for.
    pub sample_time: HostTime,
    /// Intended display time, if known.
    pub target_present: Option<HostTime>,
    /// How [`target_present`](Self::target_present) should be interpreted.
    pub presentation_timing: PresentationTiming,
    /// Latest time by which the frame must be committed/submitted.
    pub commit_deadline: HostTime,
    /// Current scheduler pipeline depth.
    pub pipeline_depth: u8,
    /// How much later than the platform's predicted present this frame is
    /// planned for, because frames are being shown that much later
    /// ([`SchedulerConfig::present_latency_alpha`]).
    ///
    /// Feedback carries it back so the scheduler can correct the estimate
    /// from what this frame actually did.
    ///
    /// [`SchedulerConfig::present_latency_alpha`]: crate::scheduler::SchedulerConfig::present_latency_alpha
    pub present_latency: Duration,
    /// Which output this frame targets.
    pub output: OutputId,
    /// Monotonic content-frame counter used for diagnostics and summaries.
    ///
    /// Retained hosts get this from [`FrameDriver`](crate::FrameDriver), which
    /// advances its counter when a planned frame becomes ready or expires.
    /// Low-level scheduler integrations pass the value explicitly to
    /// [`Scheduler::plan`](crate::scheduler::Scheduler::plan).
    pub frame_index: u64,
}

/// Backend submission constraints carried by a [`FrameOpportunity`].
///
/// Backends compute these from the current [`FrameTick`] and their own
/// knowledge of the presentation pipeline. Hosts pass them to
/// [`FrameOpportunity::new`]; `FrameDriver` and `Scheduler` use them to compute
/// deadlines, target-present timing, and feedback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PresentHints {
    /// Whether [`desired_present`](Self::desired_present) can be used as
    /// presentation timing.
    presentation_timing: PresentationTiming,
    /// Target present time, if known.
    desired_present: Option<HostTime>,
    /// Latest time by which a commit must occur to hit the desired present.
    latest_commit: HostTime,
    /// Whether frame work must start at the tick that offered it.
    starts_at_tick: bool,
    /// Whether [`Self::latest_commit`] is a limit the scheduler must keep.
    hard_deadline: bool,
}

impl PresentHints {
    /// Creates hints with explicit presentation timing for a frame opportunity.
    ///
    /// If `presentation_timing` is [`PresentationTiming::PacingOnly`],
    /// `desired_present` is discarded. Pacing-only backends expose a commit
    /// boundary but not presentation truth.
    #[inline]
    #[must_use]
    pub const fn new(
        presentation_timing: PresentationTiming,
        desired_present: Option<HostTime>,
        latest_commit: HostTime,
    ) -> Self {
        let desired_present = if presentation_timing.has_target_present() {
            desired_present
        } else {
            None
        };
        Self {
            presentation_timing,
            desired_present,
            latest_commit,
            starts_at_tick: false,
            hard_deadline: false,
        }
    }

    /// Returns how [`Self::desired_present`] should be interpreted.
    #[inline]
    #[must_use]
    pub const fn presentation_timing(self) -> PresentationTiming {
        self.presentation_timing
    }

    /// Returns the target present time when this backend can provide one.
    #[inline]
    #[must_use]
    pub const fn desired_present(self) -> Option<HostTime> {
        self.desired_present
    }

    /// Returns the latest time by which the frame should be committed.
    #[inline]
    #[must_use]
    pub const fn latest_commit(self) -> HostTime {
        self.latest_commit
    }

    /// Returns these hints with frame work required to start at a tick.
    ///
    /// Use this when the host starts frame work only from its frame
    /// callback: a browser shows only what is drawn during
    /// `requestAnimationFrame`, and a host without its own timer has no
    /// other place to start. The scheduler then plans
    /// [`FramePlan::frame_start`] at the tick that offered the hints when
    /// the frame is due at the next refresh. A frame the selected cadence
    /// holds back, such as background work every other refresh, starts at
    /// the later tick before its refresh: its plan returns
    /// [`FrameWait::NextTick`](crate::FrameWait::NextTick) until a tick at or
    /// after its start, so the host calls
    /// [`FrameDriver::begin_frame`](crate::FrameDriver::begin_frame) from
    /// each callback.
    #[inline]
    #[must_use]
    pub const fn starting_at_tick(mut self) -> Self {
        self.starts_at_tick = true;
        self
    }

    /// Returns these hints with frame work starting when the scheduler
    /// plans it, which may fall between ticks.
    ///
    /// This is the default for [`PresentHints::new`]. A host that wakes
    /// itself at the time [`FrameWait::Until`](crate::FrameWait::Until) gives,
    /// and then calls
    /// [`FrameDriver::begin_pending_frame`](crate::FrameDriver::begin_pending_frame),
    /// can use it on hints a platform adapter marked
    /// [`starting_at_tick`](Self::starting_at_tick): frames then start as late
    /// as the commit deadline and its margins allow, so the input and state
    /// they sample are fresher.
    #[inline]
    #[must_use]
    pub const fn starting_when_planned(mut self) -> Self {
        self.starts_at_tick = false;
        self
    }

    /// Returns whether frame work must start at a tick. See
    /// [`starting_at_tick`](Self::starting_at_tick).
    #[inline]
    #[must_use]
    pub const fn starts_at_tick(self) -> bool {
        self.starts_at_tick
    }

    /// Returns these hints with [`latest_commit`](Self::latest_commit) as a
    /// hard limit.
    ///
    /// Use this when the opportunity hands the host a resource that must be
    /// committed by then, such as a Metal drawable that has to be presented
    /// before its update's deadline. The scheduler then never plans the
    /// frame's [`FramePlan::commit_deadline`] past it: it adds no
    /// pipeline-depth lookahead, which cannot give that resource more time,
    /// and a frame whose deadline has already passed when it is planned or
    /// released expires instead of starting. A frame the cadence holds back
    /// until a later tick is held to the deadline of the opportunity that
    /// releases it.
    #[inline]
    #[must_use]
    pub const fn with_hard_deadline(mut self) -> Self {
        self.hard_deadline = true;
        self
    }

    /// Returns whether [`latest_commit`](Self::latest_commit) is a hard
    /// limit. See [`with_hard_deadline`](Self::with_hard_deadline).
    #[inline]
    #[must_use]
    pub const fn has_hard_deadline(self) -> bool {
        self.hard_deadline
    }

    /// Creates predictive present hints.
    #[inline]
    #[must_use]
    pub const fn predictive(desired_present: HostTime, latest_commit: HostTime) -> Self {
        Self::new(
            PresentationTiming::Predictive,
            Some(desired_present),
            latest_commit,
        )
    }

    /// Creates estimated present hints.
    #[inline]
    #[must_use]
    pub const fn estimated(desired_present: HostTime, latest_commit: HostTime) -> Self {
        Self::new(
            PresentationTiming::Estimated,
            Some(desired_present),
            latest_commit,
        )
    }

    /// Creates pacing-only hints with no presentation timestamp.
    #[inline]
    #[must_use]
    pub const fn pacing_only(latest_commit: HostTime) -> Self {
        Self::new(PresentationTiming::PacingOnly, None, latest_commit)
    }

    /// Computes hints for a platform tick, committing `commit_lead` ahead of
    /// the frame's present.
    ///
    /// A [`FrameTick::predicted_present`] at or after `tick.now` becomes the
    /// desired present, interpreted as `presentation_timing`. A missing or
    /// stale prediction, or [`PresentationTiming::PacingOnly`], gives
    /// pacing-only hints aimed one refresh after `tick.now`, using
    /// `fallback_refresh_interval` when the tick has no refresh interval.
    /// The commit boundary is never before `tick.now`.
    ///
    /// Platform adapters use this with the [`PresentationTiming`] their
    /// predictions deserve, usually with [`default_commit_lead`].
    #[must_use]
    pub fn for_tick(
        tick: &FrameTick,
        presentation_timing: PresentationTiming,
        fallback_refresh_interval: Duration,
        commit_lead: Duration,
    ) -> Self {
        let commit_boundary = |present: HostTime| {
            present
                .checked_sub(commit_lead)
                .unwrap_or(tick.now)
                .max(tick.now)
        };
        if presentation_timing.has_target_present()
            && let Some(predicted_present) = tick
                .predicted_present
                .filter(|predicted_present| *predicted_present >= tick.now)
        {
            return Self::new(
                presentation_timing,
                Some(predicted_present),
                commit_boundary(predicted_present),
            );
        }

        let pacing_target = tick
            .now
            .checked_add(tick.refresh_interval_or(fallback_refresh_interval))
            .unwrap_or(HostTime(u64::MAX));
        Self::pacing_only(commit_boundary(pacing_target))
    }

    /// Creates feedback hints normalized to an executed [`FramePlan`].
    ///
    /// Use this after scheduling has produced a plan. The scheduler may shift
    /// target-present and commit timing for cadence throttling or pipeline
    /// depth, so feedback should be judged against the plan the host actually
    /// executed rather than the platform hints that entered planning.
    #[inline]
    #[must_use]
    pub const fn for_plan(plan: &FramePlan) -> Self {
        Self::new(
            plan.presentation_timing,
            plan.target_present,
            plan.commit_deadline,
        )
    }
}

/// Timing feedback passed to [`Scheduler::observe`](crate::scheduler::Scheduler::observe).
///
/// Low-level scheduler integrations construct this with [`PresentFeedback::new`]
/// and the executed [`FramePlan`] after submitting a frame, or by resolving a
/// [`PendingFeedback`]. Retained [`FrameDriver`](crate::FrameDriver) hosts
/// usually do not construct it directly;
/// [`FrameDriver::submit_frame`](crate::FrameDriver::submit_frame) derives it
/// from [`FrameSubmission`](crate::FrameSubmission).
///
/// This type intentionally separates two different claims:
///
/// - [`PresentFeedback::missed_deadline`] answers "do we know this frame
///   missed a real presentation deadline?"
/// - [`PresentFeedback::pacing_overrun`] answers "did frame building run past
///   the pacing boundary exposed by this backend?"
///
/// Pacing-only backends often know the second thing but not the first. Keeping
/// those signals separate avoids laundering weak timing evidence into false
/// deadline truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PresentFeedback {
    /// Host time when the frame was submitted/committed.
    pub submitted_at: HostTime,
    /// Latest time the frame could be committed to make its target present.
    ///
    /// A frame submitted after this was late because of its own build, so
    /// how late it was shown says nothing about presentation latency.
    pub commit_deadline: HostTime,
    /// When the frame began building (for build cost estimation).
    pub build_start: HostTime,
    /// The presentation latency the plan was made with, carried back so
    /// the scheduler can correct it.
    pub present_latency: Duration,
    /// The pipeline depth the frame was planned at, if known, carried back
    /// so that frames already in flight when the depth changed do not
    /// count for or against the new one.
    pub pipeline_depth: Option<u8>,
    /// What producing this frame cost beyond the host's own work, if the
    /// platform can say: a renderer that submits GPU work and returns
    /// reports how long that work took.
    ///
    /// Build cost is the longer of this and the host's span from
    /// [`build_start`](Self::build_start) to
    /// [`submitted_at`](Self::submitted_at), so a frame whose GPU work
    /// outlasts the display's refresh is one the scheduler knows cannot
    /// hold that cadence. It is the work's own duration, not the time it
    /// finished: a frame queued behind others waits, and waiting is not
    /// what the frame costs.
    pub work: Option<Duration>,
    /// Expected present time at submission, if known.
    pub expected_present: Option<HostTime>,
    /// Actual present time, if the platform reports it.
    pub actual_present: Option<HostTime>,
    /// Whether the commit deadline was missed, if determinable.
    pub missed_deadline: Option<bool>,
    /// Whether frame building overran the pacing tick budget.
    ///
    /// This is a weaker signal than [`missed_deadline`]: it says only that the
    /// frame was submitted after the backend's pacing boundary, not that the
    /// compositor actually presented it late.
    ///
    /// [`missed_deadline`]: Self::missed_deadline
    pub pacing_overrun: Option<bool>,
}

impl PresentFeedback {
    /// Constructs feedback to pass to [`Scheduler::observe`](crate::scheduler::Scheduler::observe).
    ///
    /// This derives both the strict deadline signal and the weaker pacing
    /// signal from the plan the host actually executed. Use this after calling
    /// [`Scheduler::plan`](crate::scheduler::Scheduler::plan); the scheduler
    /// may have shifted target-present and commit timing from the original
    /// platform hints.
    #[must_use]
    pub fn new(
        plan: &FramePlan,
        build_start: HostTime,
        submitted_at: HostTime,
        actual_present: Option<HostTime>,
    ) -> Self {
        let mut feedback = Self::from_hints(
            &PresentHints::for_plan(plan),
            build_start,
            submitted_at,
            actual_present,
        );
        feedback.present_latency = plan.present_latency;
        feedback.pipeline_depth = Some(plan.pipeline_depth);
        // A frame shown at the refresh it was planned for is on time, even
        // though the timestamp for that refresh lands a little after the
        // target: only the next refresh is a real miss. Platforms that
        // report presentation times report them with that much jitter.
        if let (Some(actual), Some(expected)) = (actual_present, feedback.expected_present) {
            let tolerance = plan.frame_interval.div_u64(2);
            feedback.missed_deadline = Some(actual.saturating_duration_since(expected) > tolerance);
        }
        feedback
    }

    /// Constructs feedback for a frame that cost `work` beyond the host's
    /// own span, such as the GPU work its submission queued.
    #[must_use]
    pub fn with_work(
        plan: &FramePlan,
        build_start: HostTime,
        submitted_at: HostTime,
        work: Option<Duration>,
        actual_present: Option<HostTime>,
    ) -> Self {
        let mut feedback = Self::new(plan, build_start, submitted_at, actual_present);
        feedback.work = work;
        feedback
    }

    /// What this frame cost to produce: the longer of the host's own span
    /// and the [`work`](Self::work) its submission queued.
    #[must_use]
    pub fn build_cost(&self) -> Duration {
        let host = self
            .submitted_at
            .saturating_duration_since(self.build_start);
        match self.work {
            Some(work) if work > host => work,
            _ => host,
        }
    }

    /// Constructs feedback from already-normalized presentation hints.
    ///
    /// Most integrations should prefer [`Self::new`] with the executed
    /// [`FramePlan`]. Use this only when the caller has intentionally built
    /// hints that already describe the exact target-present and commit timing
    /// the frame used.
    ///
    /// `missed_deadline` should only answer "the frame was late" when the
    /// backend has enough information to support that claim.
    /// `pacing_overrun` answers the weaker question "we ran long relative to
    /// the pacing tick budget" for backends that only expose pacing.
    ///
    /// The derivation rules are:
    ///
    /// - If both `actual_present` and [`PresentHints::desired_present`] are known, a
    ///   frame is missed when `actual_present > desired_present`.
    /// - Otherwise, the result is `None`: without actual presentation
    ///   feedback, commit timing is useful pacing evidence but not real
    ///   presentation truth.
    ///
    /// When the frame cannot be classified as a real hit or miss,
    /// [`PresentFeedback::pacing_overrun`] is populated from commit timing as
    /// the weaker pacing signal.
    ///
    /// Hints carry no frame interval or plan, so this differs from
    /// [`Self::new`] in two ways. Its miss test has no tolerance: a frame
    /// shown at its planned refresh but timestamped slightly after the target
    /// counts as missed. And [`present_latency`](Self::present_latency) is
    /// zero, so a scheduler using latency compensation would read the whole
    /// compensated lateness as new; set it from the plan before observing.
    #[must_use]
    pub fn from_hints(
        hints: &PresentHints,
        build_start: HostTime,
        submitted_at: HostTime,
        actual_present: Option<HostTime>,
    ) -> Self {
        let expected_present = if hints.presentation_timing().has_target_present() {
            hints.desired_present()
        } else {
            None
        };
        // Only report deadline truth when the backend can honestly support it.
        let missed_deadline = match (actual_present, expected_present) {
            (Some(actual), Some(expected)) => Some(actual > expected),
            (_, None) => None,
            (None, Some(_)) => None,
        };
        let pacing_overrun = if missed_deadline.is_none() {
            Some(submitted_at > hints.latest_commit())
        } else {
            None
        };

        Self {
            submitted_at,
            commit_deadline: hints.latest_commit(),
            build_start,
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            expected_present,
            actual_present,
            missed_deadline,
            pacing_overrun,
        }
    }
}

/// Stored submission facts for resolving [`PresentFeedback`] on a later tick.
///
/// Actual present time for frame N is typically only known at frame N+1
/// (e.g. from `CADisplayLink.timestamp`). This type captures what we know at
/// submission time so the feedback can be resolved one frame later.
///
/// Use this in low-level [`Scheduler`](crate::scheduler::Scheduler)
/// integrations that receive previous-frame present feedback. Retained
/// [`FrameDriver`](crate::FrameDriver) integrations can often pass immediate
/// submission facts through [`FrameSubmission`](crate::FrameSubmission)
/// instead.
///
/// # Usage
///
/// ```text
/// // Frame N: submit, then store pending.
/// let pending = PendingFeedback::new(plan, build_start, submitted_at);
///
/// // Frame N+1: resolve with actual_present from the new tick.
/// let feedback = pending.resolve(tick.prev_actual_present);
/// scheduler.observe(&feedback);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct PendingFeedback {
    /// The [`FramePlan`] that was submitted.
    pub plan: FramePlan,
    /// Host time when frame building began.
    pub build_start: HostTime,
    /// Host time when the frame was submitted/committed.
    pub submitted_at: HostTime,
}

impl PendingFeedback {
    /// Creates pending feedback for a submitted [`FramePlan`].
    #[inline]
    #[must_use]
    pub const fn new(plan: FramePlan, build_start: HostTime, submitted_at: HostTime) -> Self {
        Self {
            plan,
            build_start,
            submitted_at,
        }
    }

    /// Resolves this pending value into feedback for `Scheduler::observe`.
    ///
    /// Pass the actual present time reported by the next platform tick when it
    /// is available, or `None` when the backend cannot report it.
    #[must_use]
    pub fn resolve(self, actual_present: Option<HostTime>) -> PresentFeedback {
        PresentFeedback::new(
            &self.plan,
            self.build_start,
            self.submitted_at,
            actual_present,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick_at(now: u64, predicted_present: Option<u64>) -> FrameTick {
        FrameTick {
            now: HostTime(now),
            predicted_present: predicted_present.map(HostTime),
            refresh_interval: Some(Duration(16_666_667)),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    #[test]
    fn refresh_interval_or_skips_missing_and_zero_intervals() {
        let fallback = Duration(8_333_333);
        let tick = tick_at(0, None);
        assert_eq!(tick.refresh_interval_or(fallback), Duration(16_666_667));
        for refresh_interval in [None, Some(Duration::ZERO)] {
            let tick = FrameTick {
                refresh_interval,
                ..tick
            };
            assert_eq!(tick.refresh_interval_or(fallback), fallback);
        }
    }

    #[test]
    fn default_commit_lead_is_a_quarter_refresh() {
        assert_eq!(
            default_commit_lead(Duration(16_666_667)),
            Duration(4_166_666)
        );
    }

    #[test]
    fn next_refresh_rounds_up_to_the_grid() {
        // 1000 + 4 * 160 = 1640 is the first refresh at or after 1500.
        assert_eq!(
            next_refresh(HostTime(1000), Duration(160), HostTime(1500)),
            Some(HostTime(1640))
        );
        // 1000 + 3 * 160 lands exactly on `now`.
        assert_eq!(
            next_refresh(HostTime(1000), Duration(160), HostTime(1480)),
            Some(HostTime(1480))
        );
        // A refresh already at or after `now` is the next one.
        assert_eq!(
            next_refresh(HostTime(2000), Duration(160), HostTime(1500)),
            Some(HostTime(2000))
        );
    }

    #[test]
    fn next_refresh_needs_an_interval_and_saturates() {
        assert_eq!(
            next_refresh(HostTime(1000), Duration::ZERO, HostTime(1500)),
            None
        );
        assert_eq!(
            next_refresh(
                HostTime(u64::MAX - 100_000),
                Duration(166_666),
                HostTime(u64::MAX - 10)
            ),
            Some(HostTime(u64::MAX))
        );
    }

    #[test]
    fn hints_for_tick_commit_ahead_of_a_fresh_prediction() {
        let tick = tick_at(1_000_000, Some(20_000_000));
        let hints = PresentHints::for_tick(
            &tick,
            PresentationTiming::Estimated,
            Duration(16_666_667),
            Duration(2_000_000),
        );

        assert_eq!(hints.presentation_timing(), PresentationTiming::Estimated);
        assert_eq!(hints.desired_present(), Some(HostTime(20_000_000)));
        assert_eq!(hints.latest_commit(), HostTime(18_000_000));
    }

    #[test]
    fn hints_for_tick_pace_without_a_fresh_prediction() {
        let lead = Duration(4_166_666);
        for tick in [tick_at(1_000_000, None), tick_at(1_000_000, Some(900_000))] {
            let hints = PresentHints::for_tick(
                &tick,
                PresentationTiming::Predictive,
                Duration(8_333_333),
                lead,
            );

            assert_eq!(hints.presentation_timing(), PresentationTiming::PacingOnly);
            assert_eq!(hints.desired_present(), None);
            assert_eq!(hints.latest_commit(), HostTime(13_500_001));
        }
    }

    #[test]
    fn pacing_only_hints_for_tick_ignore_a_prediction() {
        let tick = tick_at(1_000_000, Some(20_000_000));
        let hints = PresentHints::for_tick(
            &tick,
            PresentationTiming::PacingOnly,
            Duration(16_666_667),
            Duration::ZERO,
        );

        assert_eq!(hints, PresentHints::pacing_only(HostTime(17_666_667)));
    }

    #[test]
    fn hints_for_tick_never_commit_before_now() {
        let tick = tick_at(1_000_000, Some(2_000_000));
        let hints = PresentHints::for_tick(
            &tick,
            PresentationTiming::Predictive,
            Duration(16_666_667),
            Duration(5_000_000),
        );

        assert_eq!(hints.latest_commit(), HostTime(1_000_000));
    }

    fn plan_with_hints(hints: PresentHints) -> FramePlan {
        FramePlan {
            present_latency: Duration::ZERO,
            demand: FrameDemand::ANIMATION,
            frame_interval: Duration(1_000_000),
            frame_start: HostTime(0),
            starts_at_tick: hints.starts_at_tick(),
            sample_time: hints.desired_present().unwrap_or(hints.latest_commit()),
            target_present: hints.desired_present(),
            presentation_timing: hints.presentation_timing(),
            commit_deadline: hints.latest_commit(),
            pipeline_depth: 1,
            output: OutputId(0),
            frame_index: 0,
        }
    }

    fn tick_with_timing(
        now: u64,
        predicted: Option<u64>,
        refresh_interval: Option<u64>,
    ) -> FrameTick {
        FrameTick {
            now: HostTime(now),
            predicted_present: predicted.map(HostTime),
            refresh_interval: refresh_interval.map(Duration),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    #[test]
    fn display_timing_from_tick_prefers_reported_refresh_interval() {
        let tick = tick_with_timing(10_000_000, Some(11_000_000), Some(16_666_667));

        let display = DisplayTiming::from_tick(&tick, Duration(8_333_333));

        assert_eq!(display, DisplayTiming::fixed(Duration(16_666_667)));
    }

    #[test]
    fn display_timing_from_tick_falls_back_to_predicted_delta() {
        let tick = tick_with_timing(10_000_000, Some(26_666_667), None);

        let display = DisplayTiming::from_tick(&tick, Duration(8_333_333));

        assert_eq!(display, DisplayTiming::fixed(Duration(16_666_667)));
    }

    #[test]
    fn display_timing_from_tick_ignores_stale_prediction() {
        let tick = tick_with_timing(20_000_000, Some(10_000_000), None);

        let display = DisplayTiming::from_tick(&tick, Duration(16_666_667));

        assert_eq!(display, DisplayTiming::fixed(Duration(16_666_667)));
    }

    #[test]
    fn variable_timing_rounds_to_its_granularity() {
        let needed = Duration(10_000_000);
        let stepped = DisplayTiming::variable(
            Duration(8_000_000),
            Duration(32_000_000),
            Some(Duration(4_000_000)),
        );
        assert_eq!(stepped.choose_interval(needed), Duration(12_000_000));

        // A step equal to the minimum interval is still a step.
        let stepped_by_min = DisplayTiming::variable(
            Duration(8_000_000),
            Duration(32_000_000),
            Some(Duration(8_000_000)),
        );
        assert_eq!(stepped_by_min.choose_interval(needed), Duration(16_000_000));
    }

    #[test]
    fn pacing_only_hints_discard_desired_present() {
        let hints = PresentHints::new(
            PresentationTiming::PacingOnly,
            Some(HostTime(2_000_000)),
            HostTime(1_000_000),
        );

        assert_eq!(hints.presentation_timing(), PresentationTiming::PacingOnly);
        assert_eq!(hints.desired_present(), None);
        assert_eq!(hints.latest_commit(), HostTime(1_000_000));
    }

    #[test]
    fn new_with_actual_present_compares_to_expected() {
        let hints = PresentHints::predictive(HostTime(2_000_000), HostTime(1_800_000));
        let fb = PresentFeedback::from_hints(
            &hints,
            HostTime(1_700_000),
            HostTime(1_750_000),
            Some(HostTime(2_100_000)),
        );
        assert_eq!(fb.missed_deadline, Some(true));
        assert_eq!(fb.expected_present, Some(HostTime(2_000_000)));
        assert_eq!(fb.actual_present, Some(HostTime(2_100_000)));
        assert_eq!(fb.pacing_overrun, None);

        // On time.
        let fb = PresentFeedback::from_hints(
            &hints,
            HostTime(1_700_000),
            HostTime(1_750_000),
            Some(HostTime(1_999_000)),
        );
        assert_eq!(fb.missed_deadline, Some(false));
        assert_eq!(fb.pacing_overrun, None);
    }

    #[test]
    fn new_without_actual_present_uses_commit_deadline_as_pacing_evidence() {
        let hints = PresentHints::predictive(HostTime(2_000_000), HostTime(1_800_000));
        // submitted_at > latest_commit is weak pacing evidence until the
        // backend reports actual present.
        let fb =
            PresentFeedback::from_hints(&hints, HostTime(1_700_000), HostTime(1_900_000), None);
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.pacing_overrun, Some(true));

        // submitted_at <= latest_commit is still not presentation truth.
        let fb =
            PresentFeedback::from_hints(&hints, HostTime(1_700_000), HostTime(1_750_000), None);
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.pacing_overrun, Some(false));
    }

    #[test]
    fn new_without_desired_present_is_unknown() {
        let hints = PresentHints::pacing_only(HostTime(1_000_000));
        let fb = PresentFeedback::from_hints(&hints, HostTime(900_000), HostTime(1_100_000), None);
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.expected_present, None);
        assert_eq!(fb.pacing_overrun, Some(true));
    }

    #[test]
    fn new_with_actual_present_but_no_expected_present_is_unknown() {
        let hints = PresentHints::pacing_only(HostTime(1_000_000));
        let fb = PresentFeedback::from_hints(
            &hints,
            HostTime(900_000),
            HostTime(1_100_000),
            Some(HostTime(1_200_000)),
        );
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.actual_present, Some(HostTime(1_200_000)));
        assert_eq!(fb.pacing_overrun, Some(true));
    }

    #[test]
    fn pending_feedback_resolve_with_actual_present() {
        let plan = plan_with_hints(PresentHints::predictive(
            HostTime(2_000_000),
            HostTime(1_800_000),
        ));
        let pending = PendingFeedback::new(plan, HostTime(1_700_000), HostTime(1_750_000));

        // A refresh late → missed.
        let late = HostTime(2_000_000 + plan.frame_interval.ticks());
        let fb = pending.resolve(Some(late));
        assert_eq!(fb.missed_deadline, Some(true));
        assert_eq!(fb.actual_present, Some(late));
        assert_eq!(fb.pacing_overrun, None);

        // Shown at the refresh it was planned for, timestamped a little
        // after it → not missed.
        let jittered = HostTime(2_000_000 + plan.frame_interval.ticks() / 4);
        let fb = pending.resolve(Some(jittered));
        assert_eq!(fb.missed_deadline, Some(false));
        assert_eq!(fb.actual_present, Some(jittered));
        assert_eq!(fb.pacing_overrun, None);

        // Actual present on time → not missed.
        let fb = pending.resolve(Some(HostTime(1_999_000)));
        assert_eq!(fb.missed_deadline, Some(false));
        assert_eq!(fb.actual_present, Some(HostTime(1_999_000)));
        assert_eq!(fb.pacing_overrun, None);
    }

    #[test]
    fn pending_feedback_resolve_without_actual_present() {
        let plan = plan_with_hints(PresentHints::predictive(
            HostTime(2_000_000),
            HostTime(1_800_000),
        ));
        let pending = PendingFeedback::new(plan, HostTime(1_700_000), HostTime(1_750_000));

        // No actual present → commit timing is pacing evidence, not deadline truth.
        let fb = pending.resolve(None);
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.actual_present, None);
        assert_eq!(fb.pacing_overrun, Some(false));
    }

    #[test]
    fn pending_feedback_without_expected_present_stays_unknown() {
        let plan = plan_with_hints(PresentHints::pacing_only(HostTime(1_800_000)));
        let pending = PendingFeedback::new(plan, HostTime(1_700_000), HostTime(1_950_000));

        let fb = pending.resolve(None);
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.actual_present, None);
        assert_eq!(fb.pacing_overrun, Some(true));
    }

    #[test]
    fn pacing_only_on_time_submission_reports_no_overrun() {
        let hints = PresentHints::pacing_only(HostTime(1_000_000));
        let fb = PresentFeedback::from_hints(&hints, HostTime(900_000), HostTime(950_000), None);
        assert_eq!(fb.missed_deadline, None);
        assert_eq!(fb.pacing_overrun, Some(false));
    }
}
