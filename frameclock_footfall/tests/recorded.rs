// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a `Recorder` emits, as a Footfall backend sees it.

use std::sync::{Mutex, MutexGuard, OnceLock};

use footfall_core::{
    Backend, BeginsAt, ClockReading, ContextHandle, CounterDesc, DropReason, FrameKind,
    FrameNumber, FrameOpportunity, FramePlan, FramePoint, FramePointKind, FrameStream, HoldReason,
    InstrumentationLevel, LabelValue, MarkerKind, MarkerPhase, TickPeriod, TimingBasis, Unit,
};
use frameclock::diagnostics::{
    FrameDropReason, FramePlanEvent, FrameTickEvent, FrameTimingBasis, FrameTimingSummary,
    FrameTimingSummaryBuilder, PresentFeedbackEvent, SchedulerStateEvent, SubmitEvent,
};
use frameclock::scheduler::SchedulerState;
use frameclock::time::Timebase;
use frameclock::timing::{FrameDemand, PresentationTiming};
use frameclock::{BufferWait, Duration, HostTime, OutputId};
use frameclock_footfall::{HOST_CLOCK, Recorder};

/// Mach absolute time on Apple silicon: 125 ns per 3 ticks.
const MACH: Timebase = Timebase::new(125, 3);

#[derive(Clone, Debug, PartialEq)]
enum Seen {
    Marker(MarkerPhase, FrameNumber),
    Sync(u64),
    Point(FrameNumber, FramePointKind, Option<u64>, TimingBasis),
    /// The args of the point just before.
    Args(Vec<(&'static str, LabelValue)>),
    Counter(&'static str, Unit, &'static str, u64),
    /// A frame opportunity: its stream, ticks, interval, and output arg.
    Opportunity(FrameStream, u64, Option<u64>, Option<LabelValue>),
    /// A frame's plan, in host ticks.
    Plan(Planned),
}

#[derive(Clone, Debug, PartialEq)]
struct Planned {
    number: FrameNumber,
    interval: Option<u64>,
    depth: Option<u8>,
    latency_allowance: Option<u64>,
    hold: Option<(u64, HoldReason)>,
    kind: FrameKind,
    args: Vec<(&'static str, LabelValue)>,
}

/// The plan of `summary(number)`: 300 Mach ticks of safety margin are
/// 12.5 µs.
fn planned(number: u64) -> Planned {
    Planned {
        number: number.into(),
        interval: Some(3_000),
        depth: Some(2),
        latency_allowance: Some(60),
        hold: None,
        kind: FrameKind::Paced,
        args: vec![("safety_margin_ns", LabelValue::Unsigned(12_500))],
    }
}

/// The plan recorded, of the only frame recorded.
fn plan_of(seen: Vec<Seen>) -> Planned {
    let mut plans = seen.into_iter().filter_map(|seen| match seen {
        Seen::Plan(plan) => Some(plan),
        _ => None,
    });
    let plan = plans.next().expect("the frame's plan is recorded");
    assert!(plans.next().is_none(), "once");
    plan
}

/// A point of frame `number` of the default stream, with no basis.
fn point(number: u64, kind: FramePointKind, ticks: Option<u64>) -> Seen {
    Seen::Point(number.into(), kind, ticks, TimingBasis::Unknown)
}

#[derive(Default)]
struct Recording(Mutex<Vec<Seen>>);

impl Recording {
    fn push(&self, seen: Seen) {
        self.0.lock().unwrap().push(seen);
    }
}

impl Backend for Recording {
    fn numbered_marker(
        &self,
        kind: MarkerKind,
        phase: MarkerPhase,
        number: FrameNumber,
        _context: ContextHandle,
    ) {
        assert_eq!(kind, MarkerKind::Frame, "only frame markers are emitted");
        self.push(Seen::Marker(phase, number));
    }

    fn clock_sync(&self, reading: ClockReading) {
        assert!(
            core::ptr::eq(reading.clock(), &HOST_CLOCK),
            "syncs are of the host clock"
        );
        assert_eq!(
            reading.period(),
            TickPeriod::new(125, 3),
            "the Mach timebase"
        );
        self.push(Seen::Sync(reading.ticks()));
    }

    fn frame_point(&self, point: &FramePoint) {
        if let Some(time) = point.time() {
            assert!(
                core::ptr::eq(time.clock(), &HOST_CLOCK),
                "points are in the host clock"
            );
            assert_eq!(time.period(), TickPeriod::new(125, 3), "the Mach timebase");
        }
        self.push(Seen::Point(
            point.number(),
            point.kind(),
            point.time().map(|time| time.ticks()),
            point.basis(),
        ));
        if !point.args().is_empty() {
            self.push(Seen::Args(
                point
                    .args()
                    .iter()
                    .map(|arg| (arg.key(), arg.value()))
                    .collect(),
            ));
        }
    }

    fn frame_opportunity(&self, opportunity: &FrameOpportunity) {
        let time = opportunity.time();
        assert!(
            core::ptr::eq(time.clock(), &HOST_CLOCK),
            "opportunities are in the host clock"
        );
        assert_eq!(time.period(), TickPeriod::new(125, 3), "the Mach timebase");
        self.push(Seen::Opportunity(
            opportunity.stream(),
            time.ticks(),
            opportunity.interval(),
            opportunity
                .args()
                .iter()
                .find(|arg| arg.key() == "output")
                .map(|arg| arg.value()),
        ));
    }

    fn frame_plan(&self, plan: &FramePlan) {
        assert_eq!(plan.period(), TickPeriod::new(125, 3), "the Mach timebase");
        self.push(Seen::Plan(Planned {
            number: plan.number(),
            interval: plan.interval(),
            depth: plan.depth(),
            latency_allowance: plan.latency_allowance(),
            hold: plan.hold(),
            kind: plan.kind(),
            args: plan
                .args()
                .iter()
                .map(|arg| (arg.key(), arg.value()))
                .collect(),
        }));
    }

    fn counter_add(&self, desc: &'static CounterDesc, _context: ContextHandle, value: u64) {
        self.push(Seen::Counter(desc.name(), desc.unit(), "add", value));
    }

    fn gauge_set(&self, desc: &'static CounterDesc, _context: ContextHandle, value: u64) {
        self.push(Seen::Counter(desc.name(), desc.unit(), "gauge", value));
    }

    fn histogram_record(&self, desc: &'static CounterDesc, _context: ContextHandle, value: u64) {
        self.push(Seen::Counter(desc.name(), desc.unit(), "histogram", value));
    }
}

/// Installs the recording backend once, and serializes the tests that read
/// it, since a Footfall backend is process-wide.
fn recording() -> (&'static Recording, MutexGuard<'static, ()>) {
    static BACKEND: OnceLock<&'static Recording> = OnceLock::new();
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let backend = *BACKEND.get_or_init(|| {
        let backend: &'static Recording = Box::leak(Box::default());
        footfall_core::install_backend(backend).expect("nothing else installs a backend");
        footfall_core::set_max_level(InstrumentationLevel::Trace);
        backend
    });
    backend.0.lock().unwrap().clear();
    (backend, guard)
}

fn taken(backend: &Recording) -> Vec<Seen> {
    core::mem::take(&mut *backend.0.lock().unwrap())
}

fn summary(frame_index: u64) -> FrameTimingSummary {
    summary_with_wait(frame_index, None)
}

/// [`summary`], with how long the frame waited for a buffer.
fn summary_with_wait(frame_index: u64, buffer_wait: Option<BufferWait>) -> FrameTimingSummary {
    let output = OutputId(0);
    let mut summary = FrameTimingSummaryBuilder::from_tick_and_plan(
        &FrameTickEvent {
            frame_index,
            output,
            now: HostTime(1_000),
            predicted_present: Some(HostTime(3_900)),
            refresh_interval: Some(Duration(3_000)),
        },
        &FramePlanEvent {
            frame_index,
            output,
            demand: FrameDemand::ANIMATION,
            frame_interval: Duration(3_000),
            frame_start: HostTime(1_200),
            starts_at_tick: false,
            sample_time: HostTime(4_000),
            target_present: Some(HostTime(4_000)),
            presentation_timing: PresentationTiming::Predictive,
            commit_deadline: HostTime(3_250),
            pipeline_depth: 2,
            present_latency: Duration(60),
            held_back: Duration::ZERO,
            synchronous: false,
            safety_margin_ticks: 300,
        },
    );
    summary
        .record_submit(&SubmitEvent {
            frame_index,
            submitted_at: HostTime(2_900),
            expected_present: Some(HostTime(4_100)),
            buffer_wait,
        })
        .record_present_feedback(&PresentFeedbackEvent {
            frame_index,
            work: Some(Duration(600)),
            actual_present: Some(HostTime(7_000)),
            missed_deadline: Some(true),
            pacing_overrun: None,
        })
        .record_scheduler_state(&SchedulerStateEvent {
            state: SchedulerState {
                pipeline_depth: 2,
                present_latency_ticks: 30,
                safety_margin_ticks: 300,
                recent_misses: 1,
                consecutive_hits: 0,
                wake_lateness_ticks: 0,
                queue_drains: 0,
            },
        });
    let summary = summary
        .finish()
        .expect("the tick and plan describe one frame");
    assert_eq!(
        summary.timing_basis,
        FrameTimingBasis::ActualPresent,
        "the frame was reported shown"
    );
    summary
}

#[test]
fn a_shown_frame_records_every_time_frameclock_knows_in_host_ticks() {
    let (backend, _serial) = recording();
    Recorder::new(MACH).record(&summary(1234));

    assert_eq!(
        taken(backend),
        [
            point(1234, FramePointKind::Scheduled(BeginsAt::Time), Some(1_200)),
            // What the scheduler decided for the frame.
            Seen::Plan(planned(1234)),
            point(1234, FramePointKind::Predicted, Some(3_900)),
            Seen::Args(vec![(
                "presentation_timing",
                LabelValue::StaticStr("predictive")
            )]),
            point(1234, FramePointKind::Deadline, Some(3_250)),
            point(1234, FramePointKind::Target, Some(4_000)),
            point(1234, FramePointKind::Sampled, Some(4_000)),
            Seen::Point(
                1234.into(),
                FramePointKind::Submitted,
                Some(2_900),
                TimingBasis::Observed
            ),
            Seen::Args(vec![("work_ns", LabelValue::Unsigned(25_000))]),
            point(1234, FramePointKind::Expected, Some(4_100)),
            Seen::Point(
                1234.into(),
                FramePointKind::Presented,
                Some(7_000),
                TimingBasis::Observed
            ),
            Seen::Counter(
                "frameclock_submission_work",
                Unit::Nanos,
                "histogram",
                25_000
            ),
        ]
    );
}

#[test]
fn a_wait_for_a_buffer_is_recorded_as_observed_points() {
    let (backend, _serial) = recording();
    let wait = BufferWait {
        requested: HostTime(1_300),
        acquired: HostTime(1_900),
    };
    Recorder::new(MACH).record(&summary_with_wait(77, Some(wait)));

    let buffer: Vec<_> = taken(backend)
        .into_iter()
        .filter(|seen| {
            matches!(
                seen,
                Seen::Point(
                    _,
                    FramePointKind::BufferRequested | FramePointKind::BufferAcquired,
                    ..
                )
            )
        })
        .collect();
    assert_eq!(
        buffer,
        [
            Seen::Point(
                77.into(),
                FramePointKind::BufferRequested,
                Some(1_300),
                TimingBasis::Observed
            ),
            Seen::Point(
                77.into(),
                FramePointKind::BufferAcquired,
                Some(1_900),
                TimingBasis::Observed
            ),
        ]
    );
}

#[test]
fn a_frame_that_starts_at_a_tick_is_scheduled_to_begin_at_an_opportunity() {
    let (backend, _serial) = recording();
    let mut held = summary(11);
    held.starts_at_tick = true;
    held.frame_start = HostTime(1_500);
    Recorder::new(MACH).record(&held);

    assert_eq!(
        taken(backend).first(),
        Some(&point(
            11,
            FramePointKind::Scheduled(BeginsAt::Opportunity),
            Some(1_500)
        ))
    );
}

#[test]
fn a_frame_the_display_never_showed_is_not_presented() {
    let (backend, _serial) = recording();
    let mut unshown = summary(7);
    unshown.actual_present = None;
    unshown.missed_deadline = None;
    unshown.work = None;
    unshown.scheduler_state = None;
    unshown.drop_reason = Some(FrameDropReason::NotPresented);
    Recorder::new(MACH).record(&unshown);

    let points: Vec<_> = taken(backend)
        .into_iter()
        .filter(|seen| !matches!(seen, Seen::Args(..) | Seen::Plan(..)))
        .collect();
    assert_eq!(
        points,
        [
            point(7, FramePointKind::Scheduled(BeginsAt::Time), Some(1_200)),
            point(7, FramePointKind::Predicted, Some(3_900)),
            point(7, FramePointKind::Deadline, Some(3_250)),
            point(7, FramePointKind::Target, Some(4_000)),
            point(7, FramePointKind::Sampled, Some(4_000)),
            Seen::Point(
                7.into(),
                FramePointKind::Submitted,
                Some(2_900),
                TimingBasis::Observed
            ),
            point(7, FramePointKind::Expected, Some(4_100)),
            point(7, FramePointKind::Dropped(DropReason::NotPresented), None),
        ],
        "it keeps its plan's times, and frameclock does not say a later frame replaced it"
    );
}

#[test]
fn the_plan_records_its_latency_without_a_scheduler_state_sample() {
    let (backend, _serial) = recording();
    let mut unsampled = summary(8);
    unsampled.scheduler_state = None;
    Recorder::new(MACH).record(&unsampled);

    assert_eq!(
        plan_of(taken(backend)),
        planned(8),
        "the latency comes from the plan, not the state sampled after feedback"
    );
}

#[test]
fn a_frame_held_back_to_drain_the_queue_says_so() {
    let (backend, _serial) = recording();
    let mut held = summary(8);
    held.held_back = Duration(3_000);
    Recorder::new(MACH).record(&held);

    assert_eq!(
        plan_of(taken(backend)),
        Planned {
            hold: Some((3_000, HoldReason::DrainQueue)),
            ..planned(8)
        }
    );
}

#[test]
fn a_synchronous_frame_records_nothing_to_judge_it_by() {
    let (backend, _serial) = recording();
    let mut resize = summary(8);
    resize.synchronous = true;
    Recorder::new(MACH).record(&resize);

    let seen = taken(backend);
    assert_eq!(plan_of(seen.clone()).kind, FrameKind::Synchronous);
    let kinds: Vec<_> = seen
        .into_iter()
        .filter_map(|seen| match seen {
            Seen::Point(_, kind, ..) => Some(kind),
            _ => None,
        })
        .collect();
    for judged in [
        FramePointKind::Predicted,
        FramePointKind::Deadline,
        FramePointKind::Target,
    ] {
        assert!(!kinds.contains(&judged), "{judged:?} in {kinds:?}");
    }
    assert!(kinds.contains(&FramePointKind::Sampled));
    assert!(kinds.contains(&FramePointKind::Presented));
}

#[test]
fn a_presentation_frameclock_did_not_observe_is_estimated() {
    let (backend, _serial) = recording();
    let mut estimated = summary(9);
    estimated.timing_basis = FrameTimingBasis::TargetPresent;
    Recorder::new(MACH).record(&estimated);

    assert!(taken(backend).contains(&Seen::Point(
        9.into(),
        FramePointKind::Presented,
        Some(7_000),
        TimingBasis::Estimated
    )));
}

#[test]
fn frames_are_numbered_in_the_recorders_stream() {
    let (backend, _serial) = recording();
    let window = FrameStream::new(3).with_generation(1);
    Recorder::new(MACH).with_stream(window).record(&summary(5));

    assert!(
        taken(backend)
            .iter()
            .filter_map(|seen| match seen {
                Seen::Point(number, ..) => Some(*number),
                _ => None,
            })
            .all(|number| number == window.frame(5)),
        "every point is frame 5 of the window's stream"
    );
}

#[test]
fn every_drop_reason_reaches_footfall() {
    let (backend, _serial) = recording();
    let recorder = Recorder::new(MACH);
    let reasons = [
        (FrameDropReason::Discarded, DropReason::Discarded),
        (FrameDropReason::Superseded, DropReason::Superseded),
        (FrameDropReason::MissedDeadline, DropReason::MissedDeadline),
        (
            FrameDropReason::SurfaceUnavailable,
            DropReason::SurfaceUnavailable,
        ),
        (
            FrameDropReason::OutputUnavailable,
            DropReason::OutputUnavailable,
        ),
        (FrameDropReason::NotPresented, DropReason::NotPresented),
    ];
    for (number, (frameclock_reason, _)) in (0_u64..).zip(reasons) {
        let mut dropped = summary(number);
        dropped.actual_present = None;
        dropped.drop_reason = Some(frameclock_reason);
        recorder.record(&dropped);
    }

    let drops: Vec<_> = taken(backend)
        .into_iter()
        .filter_map(|seen| match seen {
            Seen::Point(number, FramePointKind::Dropped(reason), None, _) => {
                Some((number.number(), reason))
            }
            _ => None,
        })
        .collect();
    let expected: Vec<_> = (0_u64..)
        .zip(reasons)
        .map(|(number, (_, reason))| (number, reason))
        .collect();
    assert_eq!(drops, expected, "one drop per frame, with its reason");
}

#[test]
fn a_pacing_only_frame_records_no_present_times() {
    let (backend, _serial) = recording();
    let mut paced = summary(9);
    paced.timing_basis = FrameTimingBasis::PacingOnly;
    paced.presentation_timing = PresentationTiming::PacingOnly;
    paced.target_present = None;
    paced.predicted_present = None;
    paced.expected_present = None;
    paced.actual_present = None;
    Recorder::new(MACH).record(&paced);

    let points: Vec<_> = taken(backend)
        .into_iter()
        .filter(|seen| matches!(seen, Seen::Point(..)))
        .collect();
    assert_eq!(
        points,
        [
            point(9, FramePointKind::Scheduled(BeginsAt::Time), Some(1_200)),
            point(9, FramePointKind::Deadline, Some(3_250)),
            point(9, FramePointKind::Sampled, Some(4_000)),
            Seen::Point(
                9.into(),
                FramePointKind::Submitted,
                Some(2_900),
                TimingBasis::Observed
            ),
        ]
    );
}

#[test]
fn syncs_and_frames_are_in_frameclock_terms() {
    let (backend, _serial) = recording();
    let recorder = Recorder::new(MACH);
    let mut driver = frameclock::FrameDriver::new(frameclock::SchedulerConfig::pacing_only(MACH));
    driver.request(FrameDemand::INPUT);

    recorder.sync(HostTime(24_000_000));
    let opportunity = frameclock::FrameOpportunity::pacing_only(
        HostTime(24_000_000),
        Duration(400_000),
        OutputId(3),
    );
    recorder.opportunity(&opportunity);
    let frameclock::FrameBeginResult::Ready(frame) = driver.begin_frame(opportunity).result else {
        panic!("input starts a frame right away");
    };
    let number = frame.plan().frame_index;
    recorder.begin_frame(&frame);
    recorder.end_frame(&frame);

    assert_eq!(
        taken(backend),
        [
            Seen::Sync(24_000_000),
            // At the tick's time, with its refresh interval, in host ticks.
            Seen::Opportunity(
                FrameStream::DEFAULT,
                24_000_000,
                Some(400_000),
                Some(LabelValue::Unsigned(3))
            ),
            Seen::Marker(MarkerPhase::Begin, number.into()),
            Seen::Marker(MarkerPhase::End, number.into()),
        ]
    );
}

#[test]
fn as_a_diagnostics_sink_it_records_summaries() {
    let (backend, _serial) = recording();
    let mut sink = Recorder::new(MACH);
    let mut diagnostics = frameclock::diagnostics::Diagnostics::new(&mut sink);
    diagnostics.frame_timing_summary(&summary(42));

    assert!(
        taken(backend).contains(&Seen::Point(
            42.into(),
            FramePointKind::Presented,
            Some(7_000),
            TimingBasis::Observed
        )),
        "the summary reached Footfall"
    );
}
