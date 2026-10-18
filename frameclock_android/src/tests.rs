// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use frameclock::{FrameBeginResult, FrameDemand, FrameDriver};

fn select(
    timelines: &[FrameTimeline],
    preferred: usize,
    budget: FrameBudget,
) -> Result<SelectedTimeline, TimelineError> {
    FrameTimelines {
        frame_time: HostTime(0),
        preferred_index: preferred,
        timelines,
    }
    .select(
        HostTime(1_000_000),
        budget,
        Some(Duration(8_000_000)),
        DisplayTiming::fixed(Duration(8_000_000)),
        OutputId(0),
    )
}

#[test]
fn readiness_budget_selects_a_later_native_timeline() {
    let timelines = [
        FrameTimeline::new(10, 8_000_000, 6_000_000).unwrap(),
        FrameTimeline::new(11, 16_000_000, 14_000_000).unwrap(),
    ];
    let selected = select(
        &timelines,
        0,
        FrameBudget {
            build: Duration(2_000_000),
            ready_lead: Duration(5_000_000),
        },
    )
    .unwrap();
    assert_eq!(selected.timeline().vsync_id(), 11);
    assert_eq!(
        selected.frame_opportunity().hints.latest_commit(),
        HostTime(9_000_000)
    );
    assert!(selected.frame_opportunity().hints.has_hard_deadline());
}

#[test]
fn ordinary_native_timeline_round_trips_through_the_driver() {
    let timelines = [FrameTimeline::new(20, 16_000_000, 12_000_000).unwrap()];
    let selected = select(
        &timelines,
        0,
        FrameBudget {
            build: Duration(1_000_000),
            ready_lead: Duration(2_000_000),
        },
    )
    .unwrap();
    let mut driver = FrameDriver::new(scheduler_config());
    driver.request(FrameDemand::INPUT);
    let FrameBeginResult::Ready(frame) = driver.begin_frame(selected.frame_opportunity()).result
    else {
        panic!("selected input frame must be ready")
    };
    let timing = selected.bind(&frame.plan()).unwrap();
    assert_eq!(timing.vsync_id, 20);
    assert_eq!(timing.expected_present, HostTime(16_000_000));
    assert_eq!(timing.commit_deadline, HostTime(10_000_000));
}

#[test]
fn invalid_and_unreachable_native_data_are_rejected() {
    assert_eq!(
        FrameTimeline::new(-1, 16, 12),
        Err(TimelineError::InvalidData)
    );
    assert_eq!(
        FrameTimeline::new(1, -1, 12),
        Err(TimelineError::InvalidData)
    );
    assert_eq!(
        FrameTimeline::new(1, 16, 20),
        Err(TimelineError::InvalidData)
    );
    let timelines = [FrameTimeline::new(1, 16_000_000, 12_000_000).unwrap()];
    let budget = FrameBudget {
        build: Duration(20_000_000),
        ready_lead: Duration::ZERO,
    };
    assert_eq!(
        select(&timelines, 0, budget).unwrap_err(),
        TimelineError::NoFeasibleTimeline
    );
    assert_eq!(
        select(&timelines, 1, budget).unwrap_err(),
        TimelineError::InvalidPreferredIndex
    );
}

#[test]
fn legacy_callbacks_provide_no_presentation_evidence() {
    let opportunity = legacy_opportunity(HostTime(1_000), Duration(100), OutputId(0));
    assert_eq!(opportunity.tick.prev_actual_present, None);
    assert_eq!(opportunity.hints.desired_present(), None);
}

#[test]
fn a_selected_native_target_survives_learned_presentation_latency() {
    let timelines = [FrameTimeline::new(20, 16_000_000, 12_000_000).unwrap()];
    let budget = FrameBudget {
        build: Duration(1_000_000),
        ready_lead: Duration(2_000_000),
    };
    let selected = select(&timelines, 0, budget).unwrap();
    let mut config = scheduler_config();
    config.present_latency_alpha = 1.0;
    let mut driver = FrameDriver::new(config);
    driver.request(FrameDemand::INPUT);
    let FrameBeginResult::Ready(frame) = driver.begin_frame(selected.frame_opportunity()).result
    else {
        panic!("first input frame")
    };
    let _ = driver.submit_frame(
        frame,
        frameclock::FrameSubmission::new(HostTime(2_000_000), Some(HostTime(24_000_000))),
    );
    assert_eq!(driver.scheduler().present_latency_ticks(), 8_000_000);

    let next = [FrameTimeline::new(21, 40_000_000, 36_000_000).unwrap()];
    let selected = FrameTimelines {
        frame_time: HostTime(24_000_000),
        preferred_index: 0,
        timelines: &next,
    }
    .select(
        HostTime(25_000_000),
        budget,
        Some(Duration(8_000_000)),
        DisplayTiming::fixed(Duration(8_000_000)),
        OutputId(0),
    )
    .unwrap();
    driver.request(FrameDemand::INPUT);
    let FrameBeginResult::Ready(frame) = driver.begin_frame(selected.frame_opportunity()).result
    else {
        panic!("next input frame")
    };
    assert!(
        selected.bind(&frame.plan()).is_ok(),
        "native vsync token must still name the plan's target: {:?}",
        frame.plan()
    );
}

#[test]
fn a_held_frame_binds_to_the_releasing_callbacks_selected_native_timeline() {
    let budget = FrameBudget {
        build: Duration::ZERO,
        ready_lead: Duration::ZERO,
    };
    let first = [FrameTimeline::new(30, 16_000_000, 12_000_000).unwrap()];
    let selected = select(&first, 0, budget).unwrap();
    let mut driver = FrameDriver::new(scheduler_config());
    driver.request(FrameDemand::BACKGROUND);
    assert!(matches!(
        driver.begin_frame(selected.frame_opportunity()).result,
        FrameBeginResult::Wait(_)
    ));
    // Native policy now prefers a farther timeline rather than a simple
    // extrapolation of the previous one.
    let next = [FrameTimeline::new(32, 32_000_000, 28_000_000).unwrap()];
    let selected = FrameTimelines {
        frame_time: HostTime(8_000_000),
        preferred_index: 0,
        timelines: &next,
    }
    .select(
        HostTime(9_000_000),
        budget,
        Some(Duration(8_000_000)),
        DisplayTiming::fixed(Duration(8_000_000)),
        OutputId(0),
    )
    .unwrap();
    let FrameBeginResult::Ready(frame) = driver.begin_frame(selected.frame_opportunity()).result
    else {
        panic!("later callback releases held frame")
    };
    assert!(
        selected.bind(&frame.plan()).is_ok(),
        "releasing callback's vsync token must name the ready frame: {:?}",
        frame.plan()
    );
}

const MS: u64 = 1_000_000;

/// Observes `frames` frames that each build for `build` ms and are ready
/// `lead(frame)` ms after submission.
fn learn(budget: &mut LearnedBudget, frames: u64, build: u64, lead: impl Fn(u64) -> u64) {
    for frame in 0..frames {
        let start = frame * 16 * MS;
        let submitted = start + build * MS;
        assert!(budget.observe(
            HostTime(start),
            HostTime(submitted),
            HostTime(submitted + lead(frame) * MS),
        ));
    }
}

#[test]
fn a_learned_budget_starts_from_its_guess_and_converges() {
    let initial = FrameBudget {
        build: Duration(3 * MS),
        ready_lead: Duration(6 * MS),
    };
    let mut budget = LearnedBudget::new(initial);
    assert_eq!(budget.budget(), initial);

    // The first frame sets the mean and a quarter of it as the deviation:
    // room to spare until frames show their spread.
    learn(&mut budget, 1, 2, |_| 4);
    assert_eq!(budget.budget().ready_lead, Duration(8 * MS));

    // Steady frames bring it to what they take, with half a millisecond
    // of headroom kept.
    learn(&mut budget, 60, 2, |_| 4);
    let learned = budget.budget();
    let near = |value: Duration, ms: u64| {
        value >= Duration(ms * MS + MS / 2) && value < Duration(ms * MS + MS / 2 + MS / 10)
    };
    assert!(near(learned.build, 2), "{learned:?}");
    assert!(near(learned.ready_lead, 4), "{learned:?}");
}

#[test]
fn a_learned_budget_covers_the_spread_frames_show() {
    let mut budget = LearnedBudget::new(FrameBudget {
        build: Duration(2 * MS),
        ready_lead: Duration(4 * MS),
    });
    // Ready 3 or 5 ms after submission, alternately: the budget covers the
    // slow ones with room.
    learn(
        &mut budget,
        60,
        2,
        |frame| if frame % 2 == 0 { 3 } else { 5 },
    );
    let lead = budget.budget().ready_lead;
    assert!(
        lead > Duration(5 * MS) && lead < Duration(9 * MS),
        "{lead:?}"
    );

    // A frame submitted in time missed its timeline: the lead widens, and
    // stays wider for a while.
    let mut unmissed = budget;
    budget.missed();
    assert!(budget.budget().ready_lead >= lead + Duration(MS));
    let spread = |frame| if frame % 2 == 0 { 3 } else { 5 };
    learn(&mut budget, 10, 2, spread);
    learn(&mut unmissed, 10, 2, spread);
    assert!(
        budget.budget().ready_lead >= unmissed.budget().ready_lead + Duration(MS * 8 / 10),
        "{:?} against {:?}",
        budget.budget(),
        unmissed.budget()
    );
    // Misses in a row widen it further.
    budget.missed();
    budget.missed();
    assert!(budget.budget().ready_lead >= unmissed.budget().ready_lead + Duration(3 * MS));
}

#[test]
fn a_learned_budget_ignores_times_that_cannot_be_a_frames() {
    let mut budget = LearnedBudget::new(FrameBudget {
        build: Duration(2 * MS),
        ready_lead: Duration(4 * MS),
    });
    learn(&mut budget, 30, 2, |_| 4);
    let learned = budget.budget();

    // Ready before submission, as from another clock or an unavailable
    // readiness time.
    assert!(!budget.observe(HostTime(10 * MS), HostTime(12 * MS), HostTime(11 * MS)));
    // Submitted before it began.
    assert!(!budget.observe(HostTime(10 * MS), HostTime(9 * MS), HostTime(11 * MS)));
    // A sentinel read as a time, far in the future.
    assert!(!budget.observe(HostTime(10 * MS), HostTime(12 * MS), HostTime(u64::MAX)));
    assert_eq!(budget.budget(), learned);
}

#[test]
fn a_learned_budget_selects_the_preferred_timeline_once_it_knows_the_frames() {
    let timelines = [
        FrameTimeline::new(30, 8_000_000, 7_000_000).unwrap(),
        FrameTimeline::new(31, 16_000_000, 15_000_000).unwrap(),
    ];
    // A cautious guess picks the later timeline.
    let mut budget = LearnedBudget::new(FrameBudget {
        build: Duration(3 * MS),
        ready_lead: Duration(6 * MS),
    });
    let selected = select(&timelines, 0, budget.budget()).unwrap();
    assert_eq!(selected.timeline().vsync_id(), 31);

    // Frames that build in 1 ms and are ready 2 ms later fit the preferred
    // one, which needs 6 ms from the callback at 1 ms.
    learn(&mut budget, 60, 1, |_| 2);
    let selected = select(&timelines, 0, budget.budget()).unwrap();
    assert_eq!(selected.timeline().vsync_id(), 30);
    assert_eq!(
        selected.frame_opportunity().hints.latest_commit(),
        HostTime(7_000_000) - budget.budget().ready_lead
    );
}
