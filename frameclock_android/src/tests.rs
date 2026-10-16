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
