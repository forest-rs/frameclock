// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A `FrameDriver` whose platform skips a tick, recorded into a real
//! Footfall runtime: Footfall's analysis says the frame waiting for that
//! tick was dropped because the tick never came.

use footfall_analysis::{OwnedSnapshot, Trace};
use footfall_core::{BeginsAt, DropReason, FramePointKind, InstrumentationLevel};
use footfall_runtime::Runtime;
use frameclock::time::Timebase;
use frameclock::timing::{DisplayTiming, FrameTick};
use frameclock::{
    Duration, FrameBeginResult, FrameDemand, FrameDriver, FrameOpportunity, FrameSubmission,
    HostTime, OutputId, PresentHints, SchedulerConfig,
};
use frameclock_footfall::Recorder;

/// A 60 Hz display, in nanosecond host ticks.
const REFRESH: u64 = 16_666_667;
const START: u64 = 1_000_000_000;

/// A display-link tick at `now`, whose frames start at the tick.
fn tick(now: u64) -> FrameOpportunity {
    FrameOpportunity::new(
        FrameTick {
            now: HostTime(now),
            predicted_present: Some(HostTime(now + REFRESH)),
            refresh_interval: Some(Duration(REFRESH)),
            output: OutputId(0),
            prev_actual_present: None,
        },
        PresentHints::predictive(HostTime(now + REFRESH), HostTime(now + REFRESH * 3 / 4))
            .starting_at_tick(),
        DisplayTiming::fixed(Duration(REFRESH)),
    )
}

#[test]
fn a_frame_whose_tick_never_came_was_dropped_by_the_platforms_skip() {
    let runtime = Runtime::builder()
        .level(InstrumentationLevel::Trace)
        .event_capacity(4096)
        .install_global()
        .expect("nothing else installs a backend");
    let recorder = Recorder::new(Timebase::NANOS);
    let mut driver = FrameDriver::new(SchedulerConfig::predictive(Timebase::NANOS));
    // The simulated host clock runs ahead of the runtime's, so it is synced
    // once and placed at the declared period from there.
    recorder.sync(HostTime(START));

    // Background work runs every other refresh, so the first frame waits
    // for the next tick. The platform skips it, and the tick after comes
    // past the frame's deadline.
    let mut expired = Vec::new();
    for refresh in [0, 2, 3, 4] {
        let now = START + refresh * REFRESH;
        driver.request(FrameDemand::BACKGROUND);
        let opportunity = tick(now);
        recorder.opportunity(&opportunity);
        let begin = driver.begin_frame(opportunity);
        if let Some(summary) = &begin.resolved_feedback {
            recorder.record(summary);
        }
        match begin.result {
            FrameBeginResult::Ready(frame) => {
                recorder.begin_frame(&frame);
                recorder.end_frame(&frame);
                let submit =
                    driver.submit_frame(frame, FrameSubmission::new(HostTime(now + 1_000), None));
                if let Some(summary) = &submit.summary {
                    recorder.record(summary);
                }
            }
            FrameBeginResult::Expired(summary) => {
                expired.push(summary.frame_index);
                recorder.record(&summary);
            }
            FrameBeginResult::Wait(_) | FrameBeginResult::Idle => {}
        }
    }
    assert_eq!(expired.len(), 1, "the frame waiting for the skipped tick");

    let trace = Trace::from_snapshot(&OwnedSnapshot::from(runtime.snapshot()));
    let pacing = trace.pacing(core::time::Duration::from_millis(1));
    let opportunities = pacing.opportunities();
    assert_eq!(
        (opportunities.recorded, opportunities.skipped),
        (4, 1),
        "four ticks recorded, and the one between the first two skipped"
    );
    let frame = pacing.frame(expired[0]).expect("the expired frame");
    assert_eq!(
        frame.dropped(),
        Some(DropReason::MissedDeadline),
        "frameclock dropped it"
    );
    assert_eq!(frame.begins_at(), Some(BeginsAt::Opportunity));
    assert_eq!(
        frame.opportunity_late(),
        Some(core::time::Duration::from_nanos(REFRESH)),
        "the tick it waited for came a refresh late: the platform skipped it"
    );
    assert!(
        frame.time_of(FramePointKind::Scheduled(BeginsAt::Opportunity)) < frame.deadline(),
        "it was to start half a refresh before the tick it waited for"
    );
}
