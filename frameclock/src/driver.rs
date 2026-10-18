// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Retained frame lifecycle state.
//!
//! This module owns pending frame demand, queued frame plans, feedback
//! observation, and frameclock timing summaries. It explicitly does not own
//! platform event loops, app timers, redraw requests, renderer submission, or
//! surface lifecycle.

use crate::demand::FrameDemand;
use crate::diagnostics::{
    FrameBuildEvent, FrameDropEvent, FrameDropReason, FramePlanEvent, FrameTickEvent,
    FrameTimingSummary, FrameTimingSummaryBuilder, PresentFeedbackEvent, SchedulerStateEvent,
    SubmitEvent,
};
use crate::scheduler::{Scheduler, SchedulerConfig};
use crate::time::{Duration, HostTime};
use crate::timing::{FrameOpportunity, FramePlan, FrameTick, PresentFeedback, PresentHints};

/// A queued scheduler plan paired with the platform facts used to make it.
///
/// `FrameDriver` stores this internally while waiting for
/// [`FramePlan::frame_start`]. Hosts can inspect the current value through
/// [`FrameDriver::pending_frame`], but normal frame preparation receives an
/// [`ActiveFrame`] from [`FrameBeginResult::Ready`] instead.
///
/// The tick and hints must stay with the plan because diagnostics and
/// presentation feedback should be resolved against the same platform
/// opportunity, desired-present time, and deadline facts that were active when
/// the scheduler made the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlannedFrame {
    /// Platform tick that produced [`Self::plan`].
    pub tick: FrameTick,
    /// Scheduler-selected frame timing.
    pub plan: FramePlan,
    /// Presentation constraints paired with [`Self::plan`].
    pub hints: PresentHints,
    /// Scheduler safety margin in host-time ticks when [`Self::plan`] was
    /// created.
    pub safety_margin_ticks: u64,
}

impl PlannedFrame {
    /// What releases this frame before its start.
    const fn wait(&self) -> FrameWait {
        if self.hints.starts_at_tick() {
            FrameWait::NextTick
        } else {
            FrameWait::Until(self.plan.frame_start)
        }
    }

    /// Creates a planned frame from a [`FrameTick`], [`FramePlan`], and
    /// matching [`PresentHints`].
    ///
    /// Most hosts do not call this directly; [`FrameDriver`] creates
    /// `PlannedFrame` values when it consumes pending demand.
    #[inline]
    #[must_use]
    pub const fn new(
        tick: FrameTick,
        plan: FramePlan,
        hints: PresentHints,
        safety_margin_ticks: u64,
    ) -> Self {
        Self {
            tick,
            plan,
            hints,
            safety_margin_ticks,
        }
    }
}

/// A planned frame returned to the host for preparation.
///
/// `FrameDriver::begin_frame` returns this inside
/// [`FrameBeginResult::Ready`]. It is the normal host handle for frame work:
/// use [`ActiveFrame::sample_time`] or [`ActiveFrame::plan`] to prepare app
/// state, then pass the same value to [`FrameDriver::submit_frame`] or
/// [`FrameDriver::discard_frame`].
///
/// The handle carries the scheduler plan plus the original planning facts
/// needed to resolve feedback and build a [`FrameTimingSummary`] when the host
/// submits the frame.
///
/// Treat an `ActiveFrame` as a single-use lifecycle token. A host should either
/// pass it to [`FrameDriver::submit_frame`] after renderer submission or to
/// [`FrameDriver::discard_frame`] / [`FrameDriver::discard_frame_with_reason`]
/// if no submission will happen. Dropping the Rust value without reporting it
/// loses diagnostics, but it does not update scheduler feedback.
#[derive(Debug)]
pub struct ActiveFrame {
    planned: PlannedFrame,
    build_start: HostTime,
}

impl ActiveFrame {
    const fn new(planned: PlannedFrame, build_start: HostTime) -> Self {
        Self {
            planned,
            build_start,
        }
    }

    /// Returns the platform tick that produced this frame's plan.
    #[must_use]
    pub const fn tick(&self) -> FrameTick {
        self.planned.tick
    }

    /// Returns the scheduler-selected plan.
    #[must_use]
    pub const fn plan(&self) -> FramePlan {
        self.planned.plan
    }

    /// Returns the presentation hints paired with this frame.
    #[must_use]
    pub const fn hints(&self) -> PresentHints {
        self.planned.hints
    }

    /// Returns the scheduler safety margin in host-time ticks used for this
    /// frame's plan.
    #[must_use]
    pub const fn safety_margin_ticks(&self) -> u64 {
        self.planned.safety_margin_ticks
    }

    /// Returns the host time at which the driver handed this frame to the host.
    #[must_use]
    pub const fn build_start(&self) -> HostTime {
        self.build_start
    }

    /// Returns the time application state should be sampled for.
    #[must_use]
    pub const fn sample_time(&self) -> HostTime {
        self.planned.plan.sample_time
    }
}

/// What presentation evidence is available for a submitted frame.
///
/// This tells [`FrameDriver::submit_frame`] whether it can return a complete
/// [`FrameTimingSummary`] immediately or should wait for a later
/// [`FrameTick::prev_actual_present`] value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresentationObservation {
    /// The platform will not report an actual-present timestamp for this
    /// frame.
    ///
    /// The driver returns a complete summary immediately using commit timing
    /// as pacing evidence.
    Unavailable,
    /// The platform already reported when this frame was actually presented.
    ///
    /// The driver returns a complete summary immediately using this timestamp
    /// as presentation feedback.
    Actual(HostTime),
    /// The platform reports actual presentation on a later frame tick.
    ///
    /// The driver stores the submission and returns the summary in
    /// [`FrameBegin::resolved_feedback`] from the first later
    /// [`FrameDriver::begin_frame`] call whose tick reports a present time.
    ///
    /// The tick's timestamp is the display's latest refresh, so this suits
    /// hosts whose submission reaches the screen on the next one (a layer
    /// tree committed to a compositor). A renderer with GPU work queued
    /// behind the submission should use [`Self::Reported`].
    ///
    /// A timestamp earlier than the frame's submission cannot be when it was
    /// shown, so the driver keeps waiting for a later tick's timestamp.
    Deferred,
    /// The host reports what became of this frame later, by frame index,
    /// with [`FrameDriver::report_frame`].
    ///
    /// For hosts that learn each frame's own fate asynchronously, possibly
    /// after later frames were submitted: GPU completion and presented
    /// handlers, `wp_presentation` feedback, swapchain statistics. The
    /// driver holds up to [`REPORTED_FRAME_CAPACITY`] such frames; a
    /// submission beyond that resolves the oldest as commit-only.
    Reported,
}

impl PresentationObservation {
    /// Returns the actual-present timestamp carried by this observation, if
    /// one is already known.
    #[must_use]
    pub const fn actual_present(self) -> Option<HostTime> {
        match self {
            Self::Actual(actual_present) => Some(actual_present),
            Self::Unavailable | Self::Deferred | Self::Reported => None,
        }
    }
}

/// Submission facts passed to [`FrameDriver::submit_frame`].
///
/// Construct this after the host has submitted, committed, or otherwise handed
/// the [`ActiveFrame`] to its renderer/platform presentation path. It contains
/// the submission timestamp and the kind of presentation evidence the platform
/// can provide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameSubmission {
    /// Host time when the frame was submitted or committed.
    ///
    /// Pass the timestamp closest to the point where the frame leaves app-side
    /// control and enters the renderer, compositor, or presentation backend.
    pub submitted_at: HostTime,
    /// Presentation evidence available for this submission.
    pub presentation: PresentationObservation,
}

impl FrameSubmission {
    /// Creates submission facts for [`FrameDriver::submit_frame`].
    ///
    /// Passing `Some(actual_present)` records an immediate actual-present
    /// timestamp. Passing `None` means the platform will not provide actual
    /// present feedback for this frame. Use [`Self::deferred`] when a later
    /// frame tick will carry the actual-present timestamp.
    #[inline]
    #[must_use]
    pub const fn new(submitted_at: HostTime, actual_present: Option<HostTime>) -> Self {
        Self {
            submitted_at,
            presentation: match actual_present {
                Some(actual_present) => PresentationObservation::Actual(actual_present),
                None => PresentationObservation::Unavailable,
            },
        }
    }

    /// Creates submission facts for platforms that report actual present on a
    /// later frame tick.
    ///
    /// The summary for this frame is returned in
    /// [`FrameBegin::resolved_feedback`] by the first later
    /// [`FrameDriver::begin_frame`] call whose tick reports a present time at
    /// or after the submission.
    #[inline]
    #[must_use]
    pub const fn deferred(submitted_at: HostTime) -> Self {
        Self {
            submitted_at,
            presentation: PresentationObservation::Deferred,
        }
    }

    /// Creates submission facts for a frame whose fate the host reports
    /// later with [`FrameDriver::report_frame`].
    #[inline]
    #[must_use]
    pub const fn reported(submitted_at: HostTime) -> Self {
        Self {
            submitted_at,
            presentation: PresentationObservation::Reported,
        }
    }

    /// Returns the immediate actual-present timestamp, if one is already
    /// available.
    #[inline]
    #[must_use]
    pub const fn actual_present(self) -> Option<HostTime> {
        self.presentation.actual_present()
    }
}

/// How many [`PresentationObservation::Reported`] frames a [`FrameDriver`]
/// holds while waiting for their reports.
pub const REPORTED_FRAME_CAPACITY: usize = 8;

/// What became of a submitted frame, reported to
/// [`FrameDriver::report_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameReport {
    /// What the frame's submission cost beyond the host's own work, if the
    /// platform can say: for a GPU renderer, how long its work took.
    ///
    /// Build cost is the longer of this and the host's span, so the
    /// scheduler sees what a frame really costs. Report the work's
    /// duration, not when it finished: a frame queued behind others waits,
    /// and waiting is not what the frame costs.
    pub work: Option<Duration>,
    /// What the display did with the frame.
    pub outcome: PresentOutcome,
}

/// What the display did with a submitted frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresentOutcome {
    /// Shown at this host time.
    Presented(HostTime),
    /// Never shown: a later frame replaced it first.
    NotPresented,
    /// The platform could not say.
    Unknown,
}

/// Result returned by [`FrameDriver::submit_frame`].
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSubmitResult {
    /// Complete timing summary, when feedback was resolved immediately.
    ///
    /// This is usually the summary for the frame just submitted. If a new
    /// deferred submission supersedes an older deferred submission that never
    /// received actual-present feedback, this is the older frame's summary
    /// resolved with commit-only evidence.
    ///
    /// This is `None` when [`FrameSubmission::deferred`] was used and no older
    /// deferred feedback was pending; in that case the summary is returned later
    /// through [`FrameBegin::resolved_feedback`].
    pub summary: Option<FrameTimingSummary>,
    /// Whether the driver is waiting for a later actual-present timestamp for
    /// this submitted frame.
    pub awaiting_actual_present: bool,
}

impl FrameSubmitResult {
    fn complete(summary: FrameTimingSummary) -> Self {
        Self {
            summary: Some(summary),
            awaiting_actual_present: false,
        }
    }
}

/// Result returned by [`FrameDriver::begin_frame`].
#[derive(Debug)]
pub enum FrameBeginResult {
    /// No demand or queued frame is waiting.
    Idle,
    /// A frame has been planned, but its start has not arrived.
    ///
    /// [`FrameWait`] says what releases it: a later tick, or a host wake at a
    /// given time.
    Wait(FrameWait),
    /// A planned frame is ready for application update and rendering.
    ///
    /// Prepare the frame, then pass this [`ActiveFrame`] to
    /// [`FrameDriver::submit_frame`] or [`FrameDriver::discard_frame`].
    Ready(ActiveFrame),
    /// A queued frame missed its commit deadline before the host released it.
    ///
    /// The driver has dropped the planned frame without feeding build or
    /// presentation feedback; only a late wake teaches the scheduler, through
    /// [`Scheduler::observe_wake`]. Hosts should record the summary and
    /// request fresh demand if the work is still needed.
    Expired(FrameTimingSummary),
}

/// What releases a planned frame whose start has not arrived.
///
/// Returned in [`FrameBeginResult::Wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameWait {
    /// A later tick starts the frame.
    ///
    /// Its hints [start frames at a tick](crate::PresentHints::starting_at_tick)
    /// and the cadence holds it back. Call [`FrameDriver::begin_frame`] from
    /// the next frame callback as usual, and keep callbacks coming while
    /// [`FrameDriver::needs_tick`] is true. There is no time to wake at.
    NextTick,
    /// The host starts the frame by waking at this time.
    ///
    /// Its hints start frames [when planned](crate::PresentHints::starting_when_planned).
    /// Mirror the time into the host's timer queue and call
    /// [`FrameDriver::begin_pending_frame`] when it fires.
    Until(HostTime),
}

/// Result returned by [`FrameDriver::begin_frame`].
///
/// [`Self::result`] is the lifecycle action for the current frame opportunity.
/// [`Self::resolved_feedback`] carries the timing summary for a previous
/// deferred submission when the current tick supplied a
/// [`FrameTick::prev_actual_present`] at or after that submission.
#[derive(Debug)]
pub struct FrameBegin {
    /// Summary resolved from a previous [`FrameSubmission::deferred`] call.
    pub resolved_feedback: Option<FrameTimingSummary>,
    /// Current begin-frame lifecycle result.
    pub result: FrameBeginResult,
}

enum DriverBeginResult {
    Idle,
    Wait(FrameWait),
    Ready(PlannedFrame),
    Expired(PlannedFrame),
}

/// Driver for pending demand, queued frame plans, and frame feedback.
///
/// `FrameDriver` is a small retained layer above [`Scheduler`]. Hosts call
/// [`request`](Self::request) when app-visible frame demand arrives, then call
/// [`begin_frame`](Self::begin_frame) from a platform redraw/tick
/// opportunity. If the scheduler chooses a future [`FramePlan::frame_start`],
/// the driver keeps that plan queued and returns [`FrameBeginResult::Wait`].
///
/// Submit completed frames with [`submit_frame`](Self::submit_frame) so
/// `frameclock` can observe feedback and produce the frame timing summary.
/// Discard frames that will not be submitted with
/// [`discard_frame`](Self::discard_frame).
///
/// # Lifecycle
///
/// 1. Call [`request`](Self::request) with non-empty [`FrameDemand`] when input,
///    animation, layout, or other app-visible work needs a frame.
/// 2. Call [`begin_frame`](Self::begin_frame) from a redraw/tick
///    opportunity. Record [`FrameBegin::resolved_feedback`] if it is
///    present; this is the summary for a previous deferred submission.
/// 3. Inspect [`FrameBegin::result`]. If it is [`FrameBeginResult::Wait`],
///    the [`FrameWait`] says whether a later tick or a host wake starts the
///    frame. If it is [`FrameBeginResult::Expired`], record the
///    returned drop summary and request fresh demand if the work is still
///    needed. If it is [`FrameBeginResult::Idle`], wait for more demand.
/// 4. If it is [`FrameBeginResult::Ready`], sample app state at
///    [`ActiveFrame::sample_time`], then either submit it with
///    [`submit_frame`](Self::submit_frame) or drop it with
///    [`discard_frame`](Self::discard_frame).
/// 5. A frame submitted with [`FrameSubmission::reported`] is resolved later:
///    call [`report_frame`](Self::report_frame) with its index and
///    [`FrameReport`] when the host learns what became of it.
/// 6. After submit or discard, call
///    [`has_pending_demand`](Self::has_pending_demand). If it is true, request
///    another redraw so weaker retained demand can be planned on a fresh turn.
///
/// This keeps demand preemption, feedback observation, and summary construction
/// inside `frameclock`. The driver also owns `FramePlan::frame_index`, and it
/// advances the counter only when a planned frame becomes ready or expires.
/// Queued plans that are preempted or explicitly cleared do not consume an id.
/// Timer queues, event-loop wake mechanics, renderer submission, and native
/// surface lifecycle stay in the host.
///
/// # Queued plans
///
/// Queued plans are replayed as decided when they are released inside their
/// valid work window. If a plan is created at tick `T` with a future frame
/// start, a later on-time wake releases the same [`FramePlan`], [`FrameTick`],
/// and [`PresentHints`]. The returned [`ActiveFrame`] records the later wake
/// time as [`ActiveFrame::build_start`], so diagnostics and budget
/// calculations can still see wake latency.
///
/// If the host does not release a queued plan until after
/// [`FramePlan::commit_deadline`], the driver drops the plan and returns
/// [`FrameBeginResult::Expired`] with a [`FrameDropReason::MissedDeadline`]
/// summary instead of handing out a renderable frame.
#[derive(Debug)]
pub struct FrameDriver {
    scheduler: Scheduler,
    pending_demand: FrameDemand,
    pending_frame: Option<PlannedFrame>,
    pending_feedback: Option<DeferredFrameFeedback>,
    /// [`PresentationObservation::Reported`] frames awaiting their reports,
    /// in submission order.
    reported: [Option<DeferredFrameFeedback>; REPORTED_FRAME_CAPACITY],
    next_frame_index: u64,
}

#[derive(Clone, Copy, Debug)]
struct DeferredFrameFeedback {
    planned: PlannedFrame,
    build_start: HostTime,
    submitted_at: HostTime,
}

impl DeferredFrameFeedback {
    const fn new(frame: ActiveFrame, submitted_at: HostTime) -> Self {
        Self {
            planned: frame.planned,
            build_start: frame.build_start,
            submitted_at,
        }
    }
}

impl FrameDriver {
    /// Creates a driver with a new [`Scheduler`] using `config`.
    #[must_use]
    pub fn new(config: SchedulerConfig) -> Self {
        Self::from_scheduler(Scheduler::new(config))
    }

    /// Creates a driver around an existing [`Scheduler`].
    #[must_use]
    pub const fn from_scheduler(scheduler: Scheduler) -> Self {
        Self {
            scheduler,
            pending_demand: FrameDemand::NONE,
            pending_frame: None,
            pending_feedback: None,
            reported: [None; REPORTED_FRAME_CAPACITY],
            next_frame_index: 0,
        }
    }

    /// Returns how many [`PresentationObservation::Reported`] frames are
    /// still waiting for [`report_frame`](Self::report_frame).
    #[must_use]
    pub fn awaiting_reports(&self) -> usize {
        self.reported.iter().flatten().count()
    }

    /// Returns the underlying scheduler.
    #[must_use]
    pub const fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Returns demand that has not yet been planned into a frame.
    #[must_use]
    pub const fn pending_demand(&self) -> FrameDemand {
        self.pending_demand
    }

    /// Returns whether demand is waiting for another planning turn.
    ///
    /// Hosts usually request another redraw after consuming a ready
    /// [`PlannedFrame`] when this returns `true`.
    #[must_use]
    pub const fn has_pending_demand(&self) -> bool {
        !self.pending_demand.is_empty()
    }

    /// Returns whether the host should keep frame callbacks coming.
    ///
    /// This is true while demand waits to be planned, a queued frame waits
    /// for a later tick, or a [deferred](FrameSubmission::deferred)
    /// submission waits for the tick whose
    /// [`FrameTick::prev_actual_present`] resolves it. A host that requests
    /// each callback, such as a Wayland or X11 host, requests the next one
    /// while this holds; one whose callbacks run continuously, such as a
    /// display link, can pause them when it does not.
    #[must_use]
    pub const fn needs_tick(&self) -> bool {
        self.has_pending_demand() || self.pending_frame.is_some() || self.pending_feedback.is_some()
    }

    /// Returns the currently queued frame, if any.
    ///
    /// The queued plan carries the next candidate frame index. The driver
    /// consumes that id only if the queued plan later becomes ready or expires.
    #[must_use]
    pub const fn pending_frame(&self) -> Option<PlannedFrame> {
        self.pending_frame
    }

    /// Returns the frame-start time for the queued plan, if any.
    #[must_use]
    pub const fn next_frame_start(&self) -> Option<HostTime> {
        match self.pending_frame {
            Some(frame) => Some(frame.plan.frame_start),
            None => None,
        }
    }

    /// Adds app-visible frame demand for a future [`begin_frame`](Self::begin_frame) call.
    ///
    /// Call this when input, animation, layout, resize, timers, or background
    /// visual work make a frame necessary. The driver keeps the demand until it
    /// can plan a frame for a later [`FrameOpportunity`].
    ///
    /// If a frame is already queued, stronger demand invalidates that queued
    /// plan so the next [`begin_frame`](Self::begin_frame) call replans with the
    /// combined old and new demand. Weaker or equal demand never mutates the
    /// queued plan because the plan's timing was selected for its original
    /// demand. Uncovered weaker demand is retained for a later planning turn.
    pub fn request(&mut self, demand: FrameDemand) {
        if demand.is_empty() {
            return;
        }

        if let Some(frame) = self.pending_frame {
            if demand.preempts(frame.plan.demand) {
                self.pending_demand.insert(frame.plan.demand | demand);
                self.pending_frame = None;
            } else if !frame.plan.demand.contains(demand) {
                let uncovered =
                    FrameDemand::from_bits_truncate(demand.bits() & !frame.plan.demand.bits());
                self.pending_demand.insert(uncovered);
            }
            return;
        }

        self.pending_demand.insert(demand);
    }

    /// Begins a frame, waits for a queued frame start, or reports idleness.
    ///
    /// This is the normal host entry point for converting a platform frame
    /// opportunity into frame work. It is the preferred public method that
    /// consumes pending demand into a ready frame. If the selected frame start
    /// is still in the future, this stores the planned frame internally and
    /// returns [`FrameBeginResult::Wait`] with what will release it.
    ///
    /// When a queued frame becomes ready before its commit deadline, the
    /// returned [`ActiveFrame`] carries the original planning tick and plan. Its
    /// [`ActiveFrame::build_start`] is the current opportunity time that
    /// released the queued frame. If the queued frame is released after its
    /// commit deadline, the driver returns [`FrameBeginResult::Expired`]
    /// instead and records a dropped-frame summary.
    #[must_use]
    pub fn begin_frame(&mut self, opportunity: FrameOpportunity) -> FrameBegin {
        let resolved_feedback =
            self.resolve_deferred_feedback(opportunity.tick.prev_actual_present);
        let result = self.take_next(opportunity);
        let result = self.begin_result(result, opportunity.tick.now);
        FrameBegin {
            resolved_feedback,
            result,
        }
    }

    /// Begins the queued frame from the host's own wake, without a new tick.
    ///
    /// A host whose hints start frames
    /// [when planned](crate::PresentHints::starting_when_planned) wakes
    /// itself at the time [`FrameWait::Until`] gave and calls this with the
    /// current host time. The queued frame is then
    /// [`Ready`](FrameBeginResult::Ready), or
    /// [`Expired`](FrameBeginResult::Expired) if the wake came after its
    /// commit deadline. This returns [`Wait`](FrameBeginResult::Wait) again
    /// if `now` is still early, and [`Idle`](FrameBeginResult::Idle) when no
    /// frame is queued. It plans no new frame: pending demand waits
    /// for the next [`begin_frame`](Self::begin_frame).
    #[must_use]
    pub fn begin_pending_frame(&mut self, now: HostTime) -> FrameBeginResult {
        let result = self.take_pending(now).unwrap_or(DriverBeginResult::Idle);
        self.begin_result(result, now)
    }

    fn begin_result(&mut self, result: DriverBeginResult, now: HostTime) -> FrameBeginResult {
        match result {
            DriverBeginResult::Idle => FrameBeginResult::Idle,
            DriverBeginResult::Wait(wait) => FrameBeginResult::Wait(wait),
            DriverBeginResult::Ready(frame) => {
                FrameBeginResult::Ready(ActiveFrame::new(frame, now))
            }
            DriverBeginResult::Expired(frame) => FrameBeginResult::Expired(self.dropped_summary(
                frame,
                None,
                FrameDropReason::MissedDeadline,
            )),
        }
    }

    /// Reports that an active frame was submitted and returns its timing
    /// result.
    ///
    /// For [`PresentationObservation::Unavailable`] and
    /// [`PresentationObservation::Actual`], this constructs presentation
    /// feedback, feeds it to the scheduler, and returns a complete summary in
    /// [`FrameSubmitResult::summary`].
    ///
    /// For [`PresentationObservation::Deferred`], this stores the submission
    /// until a later [`begin_frame`](Self::begin_frame) call whose tick carries
    /// a [`FrameTick::prev_actual_present`] at or after the submission, then
    /// resolves it with that timestamp. This keeps Apple-
    /// style previous-frame present feedback inside `frameclock` while keeping
    /// one public submit method.
    ///
    /// If an older deferred submission is still unresolved when a new deferred
    /// frame is submitted, the older frame is resolved as commit-only feedback
    /// and returned in [`FrameSubmitResult::summary`]. This avoids debug-only
    /// assertions and release-mode feedback loss on platforms that occasionally
    /// omit actual-present timestamps.
    #[must_use]
    pub fn submit_frame(
        &mut self,
        frame: ActiveFrame,
        submission: FrameSubmission,
    ) -> FrameSubmitResult {
        match submission.presentation {
            PresentationObservation::Deferred => {
                let summary = self.resolve_pending_feedback_as_unavailable();
                self.pending_feedback =
                    Some(DeferredFrameFeedback::new(frame, submission.submitted_at));
                FrameSubmitResult {
                    summary,
                    awaiting_actual_present: true,
                }
            }
            PresentationObservation::Reported => {
                let summary = self
                    .hold_for_report(DeferredFrameFeedback::new(frame, submission.submitted_at));
                FrameSubmitResult {
                    summary,
                    awaiting_actual_present: true,
                }
            }
            PresentationObservation::Unavailable | PresentationObservation::Actual(_) => {
                let feedback = PresentFeedback::new(
                    &frame.plan(),
                    frame.build_start(),
                    submission.submitted_at,
                    submission.actual_present(),
                );
                FrameSubmitResult::complete(self.finish_submitted_frame(
                    frame.planned,
                    frame.build_start,
                    submission.submitted_at,
                    &feedback,
                ))
            }
        }
    }

    /// Resolves a [`PresentationObservation::Reported`] frame from what
    /// the host learned about it.
    ///
    /// Feeds the scheduler and returns the frame's summary. Returns `None`
    /// when no frame with `frame_index` is waiting: it was never submitted
    /// as reported, was already reported, or was resolved commit-only to
    /// make room.
    ///
    /// Reports may arrive in any order. A frame reported
    /// [`PresentOutcome::NotPresented`] feeds build cost but no deadline
    /// signal, and its summary carries
    /// [`FrameDropReason::NotPresented`].
    pub fn report_frame(
        &mut self,
        frame_index: u64,
        report: FrameReport,
    ) -> Option<FrameTimingSummary> {
        let slot = self
            .reported
            .iter_mut()
            .find(|slot| slot.is_some_and(|held| held.planned.plan.frame_index == frame_index))?;
        let held = slot.take()?;
        let actual_present = match report.outcome {
            PresentOutcome::Presented(at) => Some(at),
            PresentOutcome::NotPresented | PresentOutcome::Unknown => None,
        };
        let feedback = PresentFeedback::with_work(
            &held.planned.plan,
            held.build_start,
            held.submitted_at,
            report.work,
            actual_present,
        );
        let mut summary = self.finish_submitted_frame(
            held.planned,
            held.build_start,
            held.submitted_at,
            &feedback,
        );
        if report.outcome == PresentOutcome::NotPresented {
            summary.drop_reason = Some(FrameDropReason::NotPresented);
        }
        Some(summary)
    }

    /// Holds `held` for a report, resolving the oldest held frame
    /// commit-only when every slot is taken.
    fn hold_for_report(&mut self, held: DeferredFrameFeedback) -> Option<FrameTimingSummary> {
        if let Some(free) = self.reported.iter_mut().find(|slot| slot.is_none()) {
            *free = Some(held);
            return None;
        }
        let oldest = self
            .reported
            .iter_mut()
            .min_by_key(|slot| slot.map_or(u64::MAX, |held| held.planned.plan.frame_index))
            .expect("the capacity is not zero");
        let evicted = oldest.replace(held).expect("every slot is taken");
        let feedback = PresentFeedback::new(
            &evicted.planned.plan,
            evicted.build_start,
            evicted.submitted_at,
            None,
        );
        Some(self.finish_submitted_frame(
            evicted.planned,
            evicted.build_start,
            evicted.submitted_at,
            &feedback,
        ))
    }

    fn resolve_deferred_feedback(
        &mut self,
        actual_present: Option<HostTime>,
    ) -> Option<FrameTimingSummary> {
        let actual_present = actual_present?;
        // A refresh that happened before the frame was submitted cannot have
        // shown it. Keep waiting rather than call a late frame on time.
        let pending = self
            .pending_feedback
            .take_if(|pending| actual_present >= pending.submitted_at)?;
        let feedback = PresentFeedback::new(
            &pending.planned.plan,
            pending.build_start,
            pending.submitted_at,
            Some(actual_present),
        );
        Some(self.finish_submitted_frame(
            pending.planned,
            pending.build_start,
            pending.submitted_at,
            &feedback,
        ))
    }

    fn resolve_pending_feedback_as_unavailable(&mut self) -> Option<FrameTimingSummary> {
        let pending = self.pending_feedback.take()?;
        let feedback = PresentFeedback::new(
            &pending.planned.plan,
            pending.build_start,
            pending.submitted_at,
            None,
        );
        Some(self.finish_submitted_frame(
            pending.planned,
            pending.build_start,
            pending.submitted_at,
            &feedback,
        ))
    }

    fn finish_submitted_frame(
        &mut self,
        planned: PlannedFrame,
        build_start: HostTime,
        submitted_at: HostTime,
        feedback: &PresentFeedback,
    ) -> FrameTimingSummary {
        let plan = planned.plan;
        let tick_event = FrameTickEvent::new(plan.frame_index, &planned.tick);
        let plan_event = FramePlanEvent::new(&plan, planned.safety_margin_ticks);
        let submit_event = SubmitEvent {
            frame_index: plan.frame_index,
            submitted_at,
            expected_present: feedback.expected_present,
        };
        debug_assert_eq!(
            feedback.build_start, build_start,
            "submitted frame summary must use feedback for the same build start"
        );
        let feedback_event = PresentFeedbackEvent::new(plan.frame_index, feedback);

        self.scheduler.observe(feedback);

        let state_event = SchedulerStateEvent {
            state: self.scheduler.state(),
        };
        let build_event = FrameBuildEvent {
            frame_index: plan.frame_index,
            build_start,
        };
        let mut summary = FrameTimingSummaryBuilder::from_tick_and_plan(&tick_event, &plan_event);
        summary
            .record_frame_build(&build_event)
            .record_submit(&submit_event)
            .record_present_feedback(&feedback_event)
            .record_scheduler_state(&state_event);
        summary
            .finish()
            .expect("driver-created tick and plan describe the same frame")
    }

    /// Drops an active frame without feeding scheduler feedback.
    ///
    /// This returns a [`FrameTimingSummary`] with
    /// [`FrameTimingSummary::drop_reason`] set to
    /// [`FrameDropReason::Discarded`]. Scheduler adaptation state is sampled
    /// for diagnostics, but [`Scheduler::observe`] is not called.
    #[must_use]
    pub fn discard_frame(&mut self, frame: ActiveFrame) -> FrameTimingSummary {
        self.discard_frame_with_reason(frame, FrameDropReason::Discarded)
    }

    /// Drops an active frame with an explicit reason and without feeding
    /// scheduler feedback.
    ///
    /// Dropping a frame is a host lifecycle fact, not presentation feedback.
    /// The returned summary is useful for traces and devtools, but this method
    /// deliberately does not call [`Scheduler::observe`].
    #[must_use]
    pub fn discard_frame_with_reason(
        &mut self,
        frame: ActiveFrame,
        reason: FrameDropReason,
    ) -> FrameTimingSummary {
        self.dropped_summary(frame.planned, Some(frame.build_start), reason)
    }

    /// The summary of a frame dropped for `reason`, after its work started
    /// at `build_start`, or before it started.
    fn dropped_summary(
        &mut self,
        planned: PlannedFrame,
        build_start: Option<HostTime>,
        reason: FrameDropReason,
    ) -> FrameTimingSummary {
        let plan = planned.plan;
        let tick_event = FrameTickEvent::new(plan.frame_index, &planned.tick);
        let plan_event = FramePlanEvent::new(&plan, planned.safety_margin_ticks);
        let drop_event = FrameDropEvent::new(&plan, reason);
        let state_event = SchedulerStateEvent {
            state: self.scheduler.state(),
        };

        let mut summary = FrameTimingSummaryBuilder::from_tick_and_plan(&tick_event, &plan_event);
        if let Some(build_start) = build_start {
            summary.record_frame_build(&FrameBuildEvent {
                frame_index: plan.frame_index,
                build_start,
            });
        }
        summary
            .record_frame_drop(&drop_event)
            .record_scheduler_state(&state_event);
        summary
            .finish()
            .expect("driver-created tick and plan describe the same frame")
    }

    /// Returns the next driver lifecycle result for a frame opportunity.
    ///
    /// This plans pending demand when needed, retains future-start plans, and
    /// reports an expired result instead of returning a frame whose commit
    /// deadline has already passed.
    fn take_pending(&mut self, now: HostTime) -> Option<DriverBeginResult> {
        let frame = self.pending_frame?;
        if now < frame.plan.frame_start {
            return Some(DriverBeginResult::Wait(frame.wait()));
        }

        self.pending_frame = None;
        if !frame.hints.starts_at_tick() {
            // The host woke for this frame; a tick-started one waits for a
            // tick by design.
            self.scheduler
                .observe_wake(now.saturating_duration_since(frame.plan.frame_start));
        }
        let frame = self.consume_frame_index(frame);
        if now > frame.plan.commit_deadline {
            return Some(DriverBeginResult::Expired(frame));
        }
        Some(DriverBeginResult::Ready(frame))
    }

    fn take_next(&mut self, opportunity: FrameOpportunity) -> DriverBeginResult {
        let tick = opportunity.tick;
        if opportunity.hints.has_hard_deadline()
            && let Some(frame) = self.pending_frame.as_mut()
            && tick.now >= frame.plan.frame_start
        {
            // The releasing opportunity's resource is the one the frame will
            // use, so its deadline is the one that binds, earlier or later
            // than planned, as when the tick the plan expected never came. A
            // tick the frame still waits through offers a resource it will
            // not use.
            frame.plan.commit_deadline = opportunity.hints.latest_commit();
        }
        if let Some(result) = self.take_pending(tick.now) {
            return result;
        }

        if self.pending_demand.is_empty() {
            return DriverBeginResult::Idle;
        }

        let demand = self.pending_demand;
        self.pending_demand = FrameDemand::NONE;
        let plan = self
            .scheduler
            .plan(opportunity, demand, self.next_frame_index);
        let frame = PlannedFrame::new(
            tick,
            plan,
            opportunity.hints,
            self.scheduler.safety_margin_ticks(),
        );
        if tick.now < frame.plan.frame_start {
            self.pending_frame = Some(frame);
            DriverBeginResult::Wait(frame.wait())
        } else if tick.now > frame.plan.commit_deadline {
            let frame = self.consume_frame_index(frame);
            DriverBeginResult::Expired(frame)
        } else {
            let frame = self.consume_frame_index(frame);
            DriverBeginResult::Ready(frame)
        }
    }

    fn consume_frame_index(&mut self, mut frame: PlannedFrame) -> PlannedFrame {
        frame.plan.frame_index = self.next_frame_index;
        self.next_frame_index = self.next_frame_index.saturating_add(1);
        frame
    }

    /// Drops the queued frame and returns whether one existed.
    ///
    /// This does not clear [`pending_demand`](Self::pending_demand). Demand
    /// retained behind the queued frame remains available for the next
    /// planning turn.
    pub fn clear_pending_frame(&mut self) -> bool {
        self.pending_frame.take().is_some()
    }

    /// Feeds presentation feedback to the underlying [`Scheduler`].
    pub fn observe(&mut self, feedback: &PresentFeedback) {
        self.scheduler.observe(feedback);
    }
}

#[cfg(test)]
mod tests {
    use crate::diagnostics::{
        FrameBuildEvent, FramePlanEvent, FrameTickEvent, FrameTimingBasis,
        FrameTimingSummaryBuilder, PresentFeedbackEvent, SchedulerStateEvent, SubmitEvent,
    };
    use crate::output::OutputId;
    use crate::scheduler::SchedulerState;
    use crate::time::{Duration, Timebase};
    use crate::timing::{DisplayTiming, PresentFeedback, PresentationTiming};

    use super::*;

    const REFRESH_INTERVAL: Duration = Duration(100);
    const START_MARGIN: Duration = Duration(10);

    fn driver() -> FrameDriver {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 1;
        config.minimum_frame_start_margin = START_MARGIN;
        FrameDriver::new(config)
    }

    fn tick(now: u64, _frame_index: u64) -> FrameTick {
        FrameTick {
            now: HostTime(now),
            predicted_present: None,
            refresh_interval: Some(REFRESH_INTERVAL),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    fn hints(deadline: u64) -> PresentHints {
        PresentHints::pacing_only(HostTime(deadline))
    }

    fn opportunity(now: u64, frame_index: u64) -> FrameOpportunity {
        FrameOpportunity::new(
            tick(now, frame_index),
            hints(now + REFRESH_INTERVAL.ticks()),
            DisplayTiming::fixed(REFRESH_INTERVAL),
        )
    }

    fn tick_started_opportunity(now: u64) -> FrameOpportunity {
        let mut opportunity = opportunity(now, 0);
        opportunity.hints = opportunity.hints.starting_at_tick();
        opportunity
    }

    fn predictive_opportunity(
        now: u64,
        _frame_index: u64,
        desired_present: u64,
        latest_commit: u64,
    ) -> FrameOpportunity {
        let tick = FrameTick {
            now: HostTime(now),
            predicted_present: Some(HostTime(desired_present)),
            refresh_interval: Some(REFRESH_INTERVAL),
            output: OutputId(0),
            prev_actual_present: None,
        };
        let hints = PresentHints::predictive(HostTime(desired_present), HostTime(latest_commit));
        FrameOpportunity::new(tick, hints, DisplayTiming::fixed(REFRESH_INTERVAL))
    }

    fn predictive_opportunity_with_prev_actual(
        now: u64,
        frame_index: u64,
        desired_present: u64,
        latest_commit: u64,
        prev_actual_present: Option<HostTime>,
    ) -> FrameOpportunity {
        let mut opportunity =
            predictive_opportunity(now, frame_index, desired_present, latest_commit);
        opportunity.tick.prev_actual_present = prev_actual_present;
        opportunity
    }

    fn begin_at(driver: &mut FrameDriver, now: u64) -> FrameBeginResult {
        driver.begin_frame(opportunity(now, 0)).result
    }

    fn ready_at(driver: &mut FrameDriver, now: u64) -> ActiveFrame {
        let FrameBeginResult::Ready(frame) = begin_at(driver, now) else {
            panic!("expected ready frame");
        };
        frame
    }

    #[test]
    fn pacing_only_opportunity_fills_common_host_defaults() {
        let opportunity =
            FrameOpportunity::pacing_only(HostTime(12), REFRESH_INTERVAL, OutputId(9));

        assert_eq!(opportunity.tick.now, HostTime(12));
        assert_eq!(opportunity.tick.predicted_present, None);
        assert_eq!(opportunity.tick.refresh_interval, Some(REFRESH_INTERVAL));
        assert_eq!(opportunity.tick.output, OutputId(9));
        assert_eq!(
            opportunity.hints.presentation_timing(),
            PresentationTiming::PacingOnly
        );
        assert_eq!(opportunity.hints.desired_present(), None);
        assert_eq!(
            opportunity.hints.latest_commit(),
            HostTime(12 + REFRESH_INTERVAL.ticks())
        );
        assert_eq!(
            opportunity.display_timing,
            DisplayTiming::fixed(REFRESH_INTERVAL)
        );
    }

    #[test]
    fn no_demand_does_not_plan_frame() {
        let mut driver = driver();

        assert!(matches!(begin_at(&mut driver, 0), FrameBeginResult::Idle));
        assert_eq!(driver.next_frame_start(), None);
    }

    #[test]
    fn demand_queues_future_frame_start_until_ready() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);

        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));
        assert_eq!(driver.next_frame_start(), Some(HostTime(90)));
        assert!(matches!(
            begin_at(&mut driver, 89),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        let frame = ready_at(&mut driver, 90);
        assert_eq!(frame.tick().now, HostTime(0));
        assert_eq!(frame.build_start(), HostTime(90));
        assert_eq!(frame.plan().demand, FrameDemand::ANIMATION);
        assert_eq!(frame.plan().frame_start, HostTime(90));
        assert_eq!(frame.plan().sample_time, HostTime(100));
        assert_eq!(frame.plan().commit_deadline, HostTime(100));
        assert_eq!(driver.next_frame_start(), None);
    }

    #[test]
    fn begin_frame_returns_ready_frame() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        let frame = ready_at(&mut driver, 90);
        assert_eq!(frame.tick().now, HostTime(0));
        assert_eq!(frame.build_start(), HostTime(90));
        assert_eq!(frame.plan().sample_time, HostTime(100));
    }

    #[test]
    fn begin_frame_assigns_sequential_frame_indices() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);

        let first = ready_at(&mut driver, 10);
        assert_eq!(first.plan().frame_index, 0);
        let _ = driver.discard_frame(first);

        driver.request(FrameDemand::INPUT);
        let second = ready_at(&mut driver, 20);
        assert_eq!(second.plan().frame_index, 1);
    }

    #[test]
    fn expired_queued_frame_returns_drop_summary() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));
        let state_before = driver.scheduler().state();

        let result = begin_at(&mut driver, 101);
        let FrameBeginResult::Expired(summary) = result else {
            panic!("expected expired queued frame");
        };

        assert_eq!(summary.frame_index, 0);
        assert_eq!(summary.output, OutputId(0));
        assert_eq!(summary.demand, FrameDemand::ANIMATION);
        assert_eq!(summary.frame_start, HostTime(90));
        assert_eq!(summary.commit_deadline, HostTime(100));
        assert_eq!(summary.submitted_at, None);
        assert_eq!(summary.drop_reason, Some(FrameDropReason::MissedDeadline));
        // The frame's work never ran, so only the late wake, 11 ticks after
        // its start, teaches the scheduler: 1.5 times that.
        let after = SchedulerState {
            wake_lateness_ticks: 16,
            ..state_before
        };
        assert_eq!(summary.scheduler_state, Some(after));
        assert_eq!(driver.scheduler().state(), after);
        assert_eq!(driver.next_frame_start(), None);
    }

    #[test]
    fn begin_frame_does_not_return_expired_queued_frame_as_ready() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        assert!(matches!(
            begin_at(&mut driver, 101),
            FrameBeginResult::Expired(_)
        ));
        assert_eq!(driver.next_frame_start(), None);
    }

    #[test]
    fn stronger_demand_replans_with_combined_demand() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));
        assert_eq!(driver.next_frame_start(), Some(HostTime(90)));

        driver.request(FrameDemand::INPUT);
        assert_eq!(driver.next_frame_start(), None);

        let frame = ready_at(&mut driver, 1);
        assert_eq!(frame.tick().now, HostTime(1));
        assert!(frame.plan().demand.contains(FrameDemand::INPUT));
        assert!(frame.plan().demand.contains(FrameDemand::ANIMATION));
        assert_eq!(frame.plan().frame_start, HostTime(1));
    }

    #[test]
    fn preempted_queued_plan_does_not_consume_frame_index() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        driver.request(FrameDemand::INPUT);

        let frame = ready_at(&mut driver, 1);
        assert_eq!(frame.plan().frame_index, 0);
        assert!(frame.plan().demand.contains(FrameDemand::INPUT));
        assert!(frame.plan().demand.contains(FrameDemand::ANIMATION));
    }

    #[test]
    fn weaker_demand_waits_behind_queued_plan() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        driver.request(FrameDemand::BACKGROUND);
        assert_eq!(driver.next_frame_start(), Some(HostTime(90)));
        assert!(driver.has_pending_demand());
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);

        let frame = ready_at(&mut driver, 90);
        assert_eq!(frame.plan().demand, FrameDemand::ANIMATION);
        assert!(driver.has_pending_demand());
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);

        assert!(matches!(
            begin_at(&mut driver, 90),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(280)))
        ));
        assert_eq!(driver.next_frame_start(), Some(HostTime(280)));
    }

    #[test]
    fn covered_demand_does_not_create_extra_pending_work() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        driver.request(FrameDemand::ANIMATION);

        assert_eq!(driver.pending_demand(), FrameDemand::NONE);
        let frame = ready_at(&mut driver, 90);
        assert_eq!(frame.plan().demand, FrameDemand::ANIMATION);
    }

    #[test]
    fn clear_pending_frame_drops_plan_without_clearing_demand() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));
        driver.request(FrameDemand::BACKGROUND);

        assert!(driver.clear_pending_frame());
        assert_eq!(driver.next_frame_start(), None);
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);
        assert!(!driver.clear_pending_frame());
    }

    #[test]
    fn cleared_queued_plan_does_not_consume_frame_index() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        assert!(driver.clear_pending_frame());
        driver.request(FrameDemand::INPUT);

        let frame = ready_at(&mut driver, 10);
        assert_eq!(frame.plan().frame_index, 0);
        assert_eq!(frame.plan().demand, FrameDemand::INPUT);
    }

    #[test]
    fn weaker_retained_demand_survives_stronger_frame_submission() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        driver.request(FrameDemand::BACKGROUND);
        let frame = ready_at(&mut driver, 90);
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);

        let summary = driver
            .submit_frame(frame, FrameSubmission::new(HostTime(91), None))
            .summary
            .expect("unavailable present feedback should resolve immediately");

        assert_eq!(summary.demand, FrameDemand::ANIMATION);
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);
    }

    #[test]
    fn submit_frame_returns_summary_from_driver_lifecycle_facts() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let frame = ready_at(&mut driver, 10);
        let submission = FrameSubmission::new(HostTime(20), None);
        let expected_feedback = PresentFeedback::new(
            &frame.plan(),
            frame.build_start(),
            submission.submitted_at,
            submission.actual_present(),
        );
        let plan = frame.plan();
        let tick_event = FrameTickEvent::new(plan.frame_index, &frame.tick());
        let plan_event = FramePlanEvent::new(&plan, frame.safety_margin_ticks());
        let submit_event = SubmitEvent {
            frame_index: plan.frame_index,
            submitted_at: submission.submitted_at,
            expected_present: expected_feedback.expected_present,
        };
        let feedback_event = PresentFeedbackEvent::new(plan.frame_index, &expected_feedback);
        let build_event = FrameBuildEvent {
            frame_index: plan.frame_index,
            build_start: frame.build_start(),
        };

        let summary = driver
            .submit_frame(frame, submission)
            .summary
            .expect("unavailable present feedback should resolve immediately");

        let state_event = SchedulerStateEvent {
            state: driver.scheduler().state(),
        };
        let mut expected = FrameTimingSummaryBuilder::from_tick_and_plan(&tick_event, &plan_event);
        expected
            .record_frame_build(&build_event)
            .record_submit(&submit_event)
            .record_present_feedback(&feedback_event)
            .record_scheduler_state(&state_event);

        assert_eq!(summary, expected.finish().expect("summary should finish"));
    }

    #[test]
    fn summaries_say_when_a_frame_was_to_start_and_when_it_did() {
        let mut held = driver();
        held.request(FrameDemand::BACKGROUND);
        let FrameBeginResult::Wait(FrameWait::NextTick) =
            held.begin_frame(tick_started_opportunity(1_000)).result
        else {
            panic!("the cadence holds the frame for a later tick");
        };
        let FrameBeginResult::Ready(frame) =
            held.begin_frame(tick_started_opportunity(1_100)).result
        else {
            panic!("the next tick releases the frame");
        };
        let summary = held.discard_frame(frame);
        assert!(summary.starts_at_tick);
        assert_eq!(summary.tick_time, HostTime(1_000));
        assert_eq!(summary.build_start, Some(HostTime(1_100)));

        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            driver.begin_frame(opportunity(1_000, 0)).result,
            FrameBeginResult::Wait(FrameWait::Until(_))
        ));
        let FrameBeginResult::Expired(expired) = driver.begin_pending_frame(HostTime(5_000)) else {
            panic!("the host woke after the deadline");
        };
        assert!(!expired.starts_at_tick);
        assert_eq!(expired.build_start, None, "it never started");
    }

    #[test]
    fn discard_frame_returns_drop_summary_without_observe() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let frame = ready_at(&mut driver, 10);
        let plan = frame.plan();
        let state_before = driver.scheduler().state();

        let summary = driver.discard_frame_with_reason(frame, FrameDropReason::SurfaceUnavailable);

        assert_eq!(driver.scheduler().state(), state_before);
        assert_eq!(summary.frame_index, plan.frame_index);
        assert_eq!(summary.output, plan.output);
        assert_eq!(summary.submitted_at, None);
        assert_eq!(summary.actual_present, None);
        assert_eq!(
            summary.drop_reason,
            Some(FrameDropReason::SurfaceUnavailable)
        );
        assert_eq!(summary.scheduler_state, Some(state_before));
    }

    #[test]
    fn retained_demand_survives_discarded_active_frame() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(90)))
        ));

        driver.request(FrameDemand::BACKGROUND);
        let frame = ready_at(&mut driver, 90);
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);

        let summary = driver.discard_frame(frame);

        assert_eq!(summary.drop_reason, Some(FrameDropReason::Discarded));
        assert_eq!(driver.pending_demand(), FrameDemand::BACKGROUND);
    }

    #[test]
    fn pacing_only_submission_summary_has_no_actual_present() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let frame = ready_at(&mut driver, 10);

        let summary = driver
            .submit_frame(frame, FrameSubmission::new(HostTime(20), None))
            .summary
            .expect("unavailable present feedback should resolve immediately");

        assert_eq!(summary.timing_basis, FrameTimingBasis::PacingOnly);
        assert_eq!(summary.actual_present, None);
        assert_eq!(summary.expected_present, None);
        assert_eq!(summary.pacing_overrun, Some(false));
    }

    #[test]
    fn actual_present_submission_summary_records_deadline_facts() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(10, 3, 100, 90))
            .result
        else {
            panic!("input should start immediately");
        };

        // A refresh late, not a tick late: the timestamp for the refresh a
        // frame was planned for lands a little after the target.
        let summary = driver
            .submit_frame(
                frame,
                FrameSubmission::new(HostTime(20), Some(HostTime(100 + REFRESH_INTERVAL.ticks()))),
            )
            .summary
            .expect("actual-present feedback should resolve immediately");

        assert_eq!(summary.timing_basis, FrameTimingBasis::ActualPresent);
        assert_eq!(summary.expected_present, Some(HostTime(100)));
        assert_eq!(
            summary.actual_present,
            Some(HostTime(100 + REFRESH_INTERVAL.ticks()))
        );
        assert_eq!(summary.missed_deadline, Some(true));
        assert_eq!(summary.pacing_overrun, None);
    }

    /// Starts an input frame at `now`, to be shown at `now + 90` and
    /// committed by `now + 80`.
    fn input_frame(driver: &mut FrameDriver, now: u64) -> ActiveFrame {
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(now, 0, now + 90, now + 80))
            .result
        else {
            panic!("input should start immediately");
        };
        frame
    }

    fn presented(at: u64) -> FrameReport {
        FrameReport {
            work: None,
            outcome: PresentOutcome::Presented(HostTime(at)),
        }
    }

    #[test]
    fn reported_frames_resolve_from_their_own_reports_in_any_order() {
        let mut driver = driver();
        let mut indices = [0; 3];
        for (slot, now) in indices.iter_mut().zip([10, 110, 210]) {
            let frame = input_frame(&mut driver, now);
            *slot = frame.plan().frame_index;
            let submit = driver.submit_frame(frame, FrameSubmission::reported(HostTime(now + 10)));
            assert_eq!(submit.summary, None);
            assert!(submit.awaiting_actual_present);
        }
        assert_eq!(driver.awaiting_reports(), 3);

        // Shown two refreshes late: a miss, though a later tick's timestamp
        // would have looked on time.
        let late = driver
            .report_frame(indices[1], presented(290))
            .expect("the second frame is held");
        assert_eq!(late.frame_index, indices[1]);
        assert_eq!(late.actual_present, Some(HostTime(290)));
        assert_eq!(late.missed_deadline, Some(true));

        let on_time = driver
            .report_frame(indices[0], presented(100))
            .expect("the first frame is held");
        assert_eq!(on_time.missed_deadline, Some(false));
        assert_eq!(on_time.timing_basis, FrameTimingBasis::ActualPresent);

        let replaced = driver
            .report_frame(
                indices[2],
                FrameReport {
                    work: Some(Duration(40)),
                    outcome: PresentOutcome::NotPresented,
                },
            )
            .expect("the third frame is held");
        assert_eq!(replaced.drop_reason, Some(FrameDropReason::NotPresented));
        assert_eq!(replaced.missed_deadline, None);
        assert_eq!(replaced.work, Some(Duration(40)));

        assert_eq!(driver.awaiting_reports(), 0);
        assert_eq!(driver.report_frame(indices[0], presented(100)), None);
    }

    #[test]
    fn reported_work_counts_toward_build_cost() {
        let margin_after = |work: Option<u64>| {
            let mut driver = driver();
            let frame = input_frame(&mut driver, 10);
            let index = frame.plan().frame_index;
            let _ = driver.submit_frame(frame, FrameSubmission::reported(HostTime(20)));
            let summary = driver
                .report_frame(
                    index,
                    FrameReport {
                        work: work.map(Duration),
                        outcome: PresentOutcome::Unknown,
                    },
                )
                .expect("the frame is held");
            (driver.scheduler().safety_margin_ticks(), summary)
        };

        // The host's own span is 10 ticks, from build start to submission.
        let (host_only, host_summary) = margin_after(None);
        let (with_work, work_summary) = margin_after(Some(85));
        assert!(
            with_work > host_only,
            "85 ticks of queued work cost more than the host's 10: {with_work} <= {host_only}"
        );
        assert_eq!(host_summary.work, None);
        assert_eq!(work_summary.work, Some(Duration(85)));
        // Work the host did not wait for does not make it late to commit.
        assert_eq!(work_summary.pacing_overrun, Some(false));
    }

    #[test]
    fn reported_frames_past_capacity_resolve_the_oldest_commit_only() {
        let mut driver = driver();
        let mut first = None;
        for step in 0..REPORTED_FRAME_CAPACITY as u64 {
            let now = step * 100 + 10;
            let frame = input_frame(&mut driver, now);
            first.get_or_insert(frame.plan().frame_index);
            let submit = driver.submit_frame(frame, FrameSubmission::reported(HostTime(now + 10)));
            assert_eq!(submit.summary, None);
        }
        let first = first.expect("frames were submitted");

        let now = REPORTED_FRAME_CAPACITY as u64 * 100 + 10;
        let frame = input_frame(&mut driver, now);
        let submit = driver.submit_frame(frame, FrameSubmission::reported(HostTime(now + 10)));
        let evicted = submit.summary.expect("the oldest frame makes room");

        assert_eq!(evicted.frame_index, first);
        assert_eq!(evicted.actual_present, None);
        assert_eq!(driver.awaiting_reports(), REPORTED_FRAME_CAPACITY);
        assert_eq!(driver.report_frame(first, presented(100)), None);
    }

    #[test]
    fn deferred_submission_resolves_feedback_on_next_begin_frame() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(10, 3, 100, 90))
            .result
        else {
            panic!("input should start immediately");
        };
        let state_before_submit = driver.scheduler().state();

        let submit = driver.submit_frame(frame, FrameSubmission::deferred(HostTime(20)));

        assert_eq!(submit.summary, None);
        assert!(submit.awaiting_actual_present);
        assert_eq!(driver.scheduler().state(), state_before_submit);

        let begin = driver.begin_frame(predictive_opportunity_with_prev_actual(
            110,
            4,
            200,
            190,
            Some(HostTime(99)),
        ));
        let summary = begin
            .resolved_feedback
            .expect("next tick should resolve deferred feedback");

        assert!(matches!(begin.result, FrameBeginResult::Idle));
        assert_eq!(summary.timing_basis, FrameTimingBasis::ActualPresent);
        assert_eq!(summary.submitted_at, Some(HostTime(20)));
        assert_eq!(summary.expected_present, Some(HostTime(100)));
        assert_eq!(summary.actual_present, Some(HostTime(99)));
        assert_eq!(summary.missed_deadline, Some(false));
        assert!(driver.scheduler().safety_margin_ticks() > 0);
    }

    #[test]
    fn deferred_submission_waits_until_actual_present_arrives() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(10, 3, 100, 90))
            .result
        else {
            panic!("input should start immediately");
        };
        let state_before_submit = driver.scheduler().state();

        let submit = driver.submit_frame(frame, FrameSubmission::deferred(HostTime(20)));

        assert_eq!(submit.summary, None);
        assert!(submit.awaiting_actual_present);
        assert_eq!(driver.scheduler().state(), state_before_submit);

        let begin_without_present = driver.begin_frame(predictive_opportunity_with_prev_actual(
            110, 4, 200, 190, None,
        ));

        assert_eq!(begin_without_present.resolved_feedback, None);
        assert!(matches!(
            begin_without_present.result,
            FrameBeginResult::Idle
        ));
        assert_eq!(driver.scheduler().state(), state_before_submit);

        let begin_with_present = driver.begin_frame(predictive_opportunity_with_prev_actual(
            210,
            5,
            300,
            290,
            Some(HostTime(99)),
        ));
        let summary = begin_with_present
            .resolved_feedback
            .expect("deferred feedback should resolve once actual present arrives");

        assert!(matches!(begin_with_present.result, FrameBeginResult::Idle));
        assert_eq!(summary.timing_basis, FrameTimingBasis::ActualPresent);
        assert_eq!(summary.actual_present, Some(HostTime(99)));
        assert_eq!(summary.missed_deadline, Some(false));
        assert!(driver.scheduler().safety_margin_ticks() > 0);
    }

    #[test]
    fn summary_reports_the_present_latency_the_frame_was_planned_with() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(10, 0, 100, 90))
            .result
        else {
            panic!("input should start immediately");
        };

        // Committed on time and shown two refreshes late.
        let submit = driver.submit_frame(
            frame,
            FrameSubmission::new(HostTime(20), Some(HostTime(300))),
        );
        let summary = submit
            .summary
            .expect("an actual present resolves the frame");

        assert_eq!(summary.present_latency, Duration::ZERO);
        let state = summary
            .scheduler_state
            .expect("submitted frames sample state");
        assert!(state.present_latency_ticks > 0, "the frame taught latency");
    }

    #[test]
    fn deferred_submission_ignores_a_present_before_it_was_submitted() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(10, 3, 100, 90))
            .result
        else {
            panic!("input should start immediately");
        };

        // The build overran: the frame was submitted after the refresh it
        // was planned for.
        let submit = driver.submit_frame(frame, FrameSubmission::deferred(HostTime(120)));
        assert!(submit.awaiting_actual_present);

        // The next tick reports that refresh, which came before submission.
        let begin = driver.begin_frame(predictive_opportunity_with_prev_actual(
            130,
            4,
            200,
            190,
            Some(HostTime(100)),
        ));
        assert_eq!(begin.resolved_feedback, None);

        // The tick after reports the refresh that really showed it.
        let begin = driver.begin_frame(predictive_opportunity_with_prev_actual(
            210,
            5,
            300,
            290,
            Some(HostTime(200)),
        ));
        let summary = begin
            .resolved_feedback
            .expect("a present after submission resolves the frame");
        assert_eq!(summary.actual_present, Some(HostTime(200)));
        assert_eq!(summary.missed_deadline, Some(true));
    }

    #[test]
    fn second_deferred_submission_resolves_unresolved_prior_feedback_as_commit_only() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(first) = driver
            .begin_frame(predictive_opportunity(10, 3, 100, 90))
            .result
        else {
            panic!("input should start immediately");
        };
        let first_plan = first.plan();

        let first_submit = driver.submit_frame(first, FrameSubmission::deferred(HostTime(20)));

        assert_eq!(first_submit.summary, None);
        assert!(first_submit.awaiting_actual_present);

        driver.request(FrameDemand::INPUT);
        let begin_second = driver.begin_frame(predictive_opportunity_with_prev_actual(
            110, 4, 200, 190, None,
        ));
        assert_eq!(begin_second.resolved_feedback, None);
        let FrameBeginResult::Ready(second) = begin_second.result else {
            panic!("second input frame should start immediately");
        };

        let second_submit = driver.submit_frame(second, FrameSubmission::deferred(HostTime(120)));
        let first_summary = second_submit
            .summary
            .expect("superseded deferred feedback should resolve as commit-only");

        assert!(second_submit.awaiting_actual_present);
        assert_eq!(first_summary.frame_index, first_plan.frame_index);
        assert_eq!(first_summary.submitted_at, Some(HostTime(20)));
        assert_eq!(first_summary.expected_present, Some(HostTime(100)));
        assert_eq!(first_summary.actual_present, None);
        assert_eq!(first_summary.missed_deadline, None);
        assert_eq!(first_summary.pacing_overrun, Some(false));

        let begin_with_present = driver.begin_frame(predictive_opportunity_with_prev_actual(
            210,
            5,
            300,
            290,
            Some(HostTime(199)),
        ));
        let second_summary = begin_with_present
            .resolved_feedback
            .expect("new deferred feedback should remain pending");

        assert_eq!(second_summary.submitted_at, Some(HostTime(120)));
        assert_eq!(second_summary.actual_present, Some(HostTime(199)));
        assert_eq!(second_summary.missed_deadline, Some(false));
    }

    #[test]
    fn submit_frame_judges_feedback_against_shifted_plan() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 2;
        config.minimum_frame_start_margin = Duration::ZERO;
        let mut driver = FrameDriver::new(config);

        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            driver
                .begin_frame(predictive_opportunity(0, 4, 100, 90))
                .result,
            FrameBeginResult::Wait(FrameWait::Until(HostTime(190)))
        ));

        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(predictive_opportunity(190, 4, 290, 280))
            .result
        else {
            panic!("shifted frame should be ready at its planned start");
        };
        let plan = frame.plan();
        assert_eq!(plan.target_present, Some(HostTime(200)));
        assert_eq!(plan.commit_deadline, HostTime(190));

        let summary = driver
            .submit_frame(
                frame,
                FrameSubmission::new(HostTime(190), Some(HostTime(200))),
            )
            .summary
            .expect("actual-present feedback should resolve immediately");

        assert_eq!(summary.expected_present, Some(HostTime(200)));
        assert_eq!(summary.missed_deadline, Some(false));
        assert_eq!(summary.pacing_overrun, None);
    }

    #[test]
    fn observe_updates_underlying_scheduler() {
        let mut driver = driver();
        driver.observe(&PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(20),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(10),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: None,
            pacing_overrun: Some(false),
        });

        assert!(driver.scheduler().safety_margin_ticks() > 0);
    }

    #[test]
    fn tick_started_frames_keep_the_cadence_across_callbacks() {
        let mut driver = driver();
        // Bit `i` is set when callback `i` started a frame.
        let mut started = 0_u8;
        for i in 0..8_u64 {
            // Callbacks arrive with a little jitter.
            let now = 1_000 + i * REFRESH_INTERVAL.ticks() + (i % 3);
            driver.request(FrameDemand::BACKGROUND);
            match driver.begin_frame(tick_started_opportunity(now)).result {
                FrameBeginResult::Ready(frame) => {
                    started |= 1 << i;
                    let _ =
                        driver.submit_frame(frame, FrameSubmission::new(HostTime(now + 5), None));
                }
                FrameBeginResult::Wait(FrameWait::NextTick) => {}
                other => panic!("callback {i}: {other:?}"),
            }
        }

        // Background work runs every other refresh.
        assert_eq!(started, 0b1010_1010, "{started:08b}");
    }

    #[test]
    fn input_preempts_a_frame_waiting_for_a_later_tick() {
        let mut driver = driver();
        driver.request(FrameDemand::BACKGROUND);
        assert!(matches!(
            driver.begin_frame(tick_started_opportunity(1_000)).result,
            FrameBeginResult::Wait(FrameWait::NextTick)
        ));

        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(tick_started_opportunity(1_000 + REFRESH_INTERVAL.ticks()))
            .result
        else {
            panic!("input must start at the next tick");
        };
        assert!(frame.plan().demand.contains(FrameDemand::INPUT));
        assert_eq!(frame.plan().frame_start, HostTime(1_100));
    }

    #[test]
    fn a_host_wake_begins_the_queued_frame() {
        let mut driver = driver();
        assert!(matches!(
            driver.begin_pending_frame(HostTime(1_000)),
            FrameBeginResult::Idle
        ));
        assert!(!driver.needs_tick());

        driver.request(FrameDemand::ANIMATION);
        let FrameBeginResult::Wait(FrameWait::Until(frame_start)) =
            driver.begin_frame(opportunity(1_000, 0)).result
        else {
            panic!("an animation frame starts as late as its deadline allows");
        };
        assert!(driver.needs_tick());
        assert!(matches!(
            driver.begin_pending_frame(HostTime(frame_start.ticks() - 1)),
            FrameBeginResult::Wait(FrameWait::Until(start)) if start == frame_start
        ));

        let FrameBeginResult::Ready(frame) = driver.begin_pending_frame(frame_start) else {
            panic!("the wake at its start must release the frame");
        };
        assert_eq!(frame.build_start(), frame_start);
        assert!(!driver.needs_tick());
        assert!(matches!(
            driver.begin_pending_frame(frame_start),
            FrameBeginResult::Idle
        ));
    }

    #[test]
    fn a_deferred_submission_needs_the_tick_that_resolves_it() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let frame = ready_at(&mut driver, 0);
        let submit = driver.submit_frame(frame, FrameSubmission::deferred(HostTime(50)));
        assert!(submit.awaiting_actual_present);
        assert!(!driver.has_pending_demand());
        assert!(
            driver.needs_tick(),
            "pausing now would strand the frame's summary"
        );

        // A tick without a present time leaves it waiting.
        let _ = driver.begin_frame(opportunity(100, 1));
        assert!(driver.needs_tick());

        let begin = driver.begin_frame(predictive_opportunity_with_prev_actual(
            200,
            1,
            300,
            290,
            Some(HostTime(120)),
        ));
        assert!(begin.resolved_feedback.is_some());
        assert!(!driver.needs_tick());
    }

    /// A drawable-style opportunity: start at the tick, commit by `deadline`.
    fn hard_deadline_opportunity(now: u64, deadline: u64) -> FrameOpportunity {
        FrameOpportunity::new(
            tick(now, 0),
            PresentHints::pacing_only(HostTime(deadline))
                .starting_at_tick()
                .with_hard_deadline(),
            DisplayTiming::fixed(REFRESH_INTERVAL),
        )
    }

    #[test]
    fn pipeline_depth_does_not_extend_a_hard_deadline() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 2;
        let mut driver = FrameDriver::new(config);
        driver.request(FrameDemand::ANIMATION);
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(hard_deadline_opportunity(1_000, 1_090))
            .result
        else {
            panic!("the frame starts at the tick");
        };
        assert_eq!(frame.plan().pipeline_depth, 2);
        assert_eq!(frame.plan().commit_deadline, HostTime(1_090));
    }

    #[test]
    fn an_opportunity_past_its_hard_deadline_expires() {
        let mut driver = driver();
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Expired(summary) = driver
            .begin_frame(hard_deadline_opportunity(1_100, 1_090))
            .result
        else {
            panic!("a frame whose resource is already due must not start");
        };
        assert_eq!(summary.drop_reason, Some(FrameDropReason::MissedDeadline));
    }

    #[test]
    fn a_held_frame_takes_the_hard_deadline_of_the_tick_that_releases_it() {
        let mut driver = driver();
        driver.request(FrameDemand::BACKGROUND);
        assert!(matches!(
            driver
                .begin_frame(hard_deadline_opportunity(1_000, 1_090))
                .result,
            FrameBeginResult::Wait(FrameWait::NextTick)
        ));
        // Planned for 1_190, but the next update's resource is due at 1_180.
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(hard_deadline_opportunity(1_100, 1_180))
            .result
        else {
            panic!("the next tick releases the frame");
        };
        assert_eq!(frame.plan().commit_deadline, HostTime(1_180));

        driver.request(FrameDemand::BACKGROUND);
        let _ = driver.discard_frame(frame);
        let _ = driver.begin_frame(hard_deadline_opportunity(1_200, 1_290));
        // The releasing update arrives after its own deadline.
        assert!(matches!(
            driver
                .begin_frame(hard_deadline_opportunity(1_300, 1_280))
                .result,
            FrameBeginResult::Expired(_)
        ));
    }

    #[test]
    fn a_tick_the_held_frame_waits_through_leaves_its_deadline_alone() {
        let mut driver = driver();
        // Slow frames stretch the background cadence to three refreshes.
        for step in 0..8 {
            let now = step * 100;
            let frame = input_frame(&mut driver, now);
            let _ = driver.submit_frame(frame, FrameSubmission::new(HostTime(now + 190), None));
        }
        driver.request(FrameDemand::BACKGROUND);
        assert!(matches!(
            driver
                .begin_frame(hard_deadline_opportunity(1_000, 1_090))
                .result,
            FrameBeginResult::Wait(FrameWait::NextTick)
        ));
        assert!(matches!(
            driver
                .begin_frame(hard_deadline_opportunity(1_100, 1_190))
                .result,
            FrameBeginResult::Wait(FrameWait::NextTick)
        ));
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(hard_deadline_opportunity(1_200, 1_290))
            .result
        else {
            panic!("the third tick releases the frame");
        };
        assert_eq!(frame.plan().frame_interval, Duration(300));
        assert_eq!(frame.plan().commit_deadline, HostTime(1_290));
    }

    #[test]
    fn a_held_frame_released_after_a_skipped_tick_takes_its_releasing_deadline() {
        let mut driver = driver();
        driver.request(FrameDemand::BACKGROUND);
        assert!(matches!(
            driver
                .begin_frame(hard_deadline_opportunity(1_000, 1_090))
                .result,
            FrameBeginResult::Wait(FrameWait::NextTick)
        ));
        // The tick at 1_100 never comes; the plan's 1_190 has passed by the
        // next one, but that tick's resource is due at 1_290.
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(hard_deadline_opportunity(1_200, 1_290))
            .result
        else {
            panic!("the next tick's resource still has time");
        };
        assert_eq!(frame.plan().commit_deadline, HostTime(1_290));
    }

    #[test]
    fn a_late_host_wake_expires_the_queued_frame() {
        let mut driver = driver();
        driver.request(FrameDemand::ANIMATION);
        let begin = driver.begin_frame(opportunity(1_000, 0));
        assert!(matches!(
            begin.result,
            FrameBeginResult::Wait(FrameWait::Until(_))
        ));

        let FrameBeginResult::Expired(summary) = driver.begin_pending_frame(HostTime(1_200)) else {
            panic!("a wake after the deadline must expire the frame");
        };
        assert_eq!(summary.drop_reason, Some(FrameDropReason::MissedDeadline));
    }

    #[test]
    fn hints_starting_when_planned_undo_starting_at_tick() {
        let hints = hints(1_100).starting_at_tick().starting_when_planned();
        assert!(!hints.starts_at_tick());
        assert_eq!(hints, self::hints(1_100));
    }

    #[test]
    fn late_wakes_move_later_frame_starts_earlier() {
        let mut driver = driver();
        for _ in 0..2 {
            driver.request(FrameDemand::ANIMATION);
            let FrameBeginResult::Wait(FrameWait::Until(start)) = begin_at(&mut driver, 0) else {
                panic!("an animation frame starts as late as its margins allow");
            };
            // The host's timer fires 4 ticks late.
            let FrameBeginResult::Ready(frame) =
                driver.begin_pending_frame(HostTime(start.ticks() + 4))
            else {
                panic!("a wake before the deadline releases the frame");
            };
            let _ = driver.discard_frame(frame);
        }
        // 1.5 times the 4-tick lateness.
        assert_eq!(driver.scheduler().wake_lateness_ticks(), 6);

        driver.request(FrameDemand::ANIMATION);
        assert!(matches!(
            begin_at(&mut driver, 0),
            FrameBeginResult::Wait(FrameWait::Until(HostTime(84)))
        ));
    }

    #[test]
    fn frames_started_by_ticks_teach_no_wake_lateness() {
        let mut driver = driver();
        driver.request(FrameDemand::BACKGROUND);
        let _ = driver.begin_frame(tick_started_opportunity(1_000));
        let FrameBeginResult::Ready(frame) = driver
            .begin_frame(tick_started_opportunity(1_000 + REFRESH_INTERVAL.ticks()))
            .result
        else {
            panic!("the next tick releases the frame");
        };
        let _ = driver.discard_frame(frame);
        assert_eq!(driver.scheduler().wake_lateness_ticks(), 0);
    }
}
