// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Frame plans recorded from a simulated `FrameDriver`, judged by Footfall's
//! pacing analysis.

use footfall_analysis::{Outcome, OwnedSnapshot, Trace};
use footfall_core::InstrumentationLevel;
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

fn opportunity(now: u64) -> FrameOpportunity {
    let tick = FrameTick {
        now: HostTime(now),
        predicted_present: Some(HostTime(now + REFRESH)),
        refresh_interval: Some(Duration(REFRESH)),
        output: OutputId(0),
        prev_actual_present: None,
    };
    FrameOpportunity::new(
        tick,
        PresentHints::predictive(HostTime(now + REFRESH), HostTime(now + REFRESH * 3 / 4)),
        DisplayTiming::fixed(Duration(REFRESH)),
    )
}

#[test]
fn a_resize_frame_is_judged_as_synchronous_and_paced_frames_by_their_plan() {
    let runtime = Runtime::builder()
        .level(InstrumentationLevel::Trace)
        .event_capacity(4096)
        .install_global()
        .expect("nothing else installs a backend");
    let recorder = Recorder::new(Timebase::NANOS);
    let mut driver = FrameDriver::new(SchedulerConfig::predictive(Timebase::NANOS));
    recorder.sync(HostTime(START));

    let mut resize = None;
    for frame in 0..8_u64 {
        let now = START + frame * REFRESH;
        driver.request(FrameDemand::INPUT);
        let FrameBeginResult::Ready(active) = driver.begin_frame(opportunity(now)).result else {
            panic!("input starts frame {frame} right away");
        };
        let target = active.plan().target_present.expect("predictive");
        recorder.begin_frame(&active);
        recorder.end_frame(&active);
        let submission = FrameSubmission::new(HostTime(now + 1_000_000), Some(target));
        let summary = driver.submit_frame(active, submission).summary.unwrap();
        recorder.record(&summary);

        if frame == 4 {
            // The window resized between ticks, and its frame was shown two
            // and a half refreshes later, by the platform's rules.
            let at = now + REFRESH / 2;
            let active = driver.begin_synchronous_frame(HostTime(at));
            resize = Some(active.plan().frame_index);
            recorder.begin_frame(&active);
            recorder.end_frame(&active);
            let shown = HostTime(at + REFRESH * 5 / 2);
            let submission = FrameSubmission::new(HostTime(at + 6_000_000), Some(shown));
            let summary = driver.submit_frame(active, submission).summary.unwrap();
            recorder.record(&summary);
        }
    }

    let trace = Trace::from_snapshot(&OwnedSnapshot::from(runtime.snapshot()));
    let pacing = trace.pacing(core::time::Duration::from_millis(1));
    assert_eq!(pacing.unplaced_points(), 0);
    assert_eq!(pacing.planned(), 9, "every frame has its plan");

    let resize = pacing.frame(resize.unwrap()).expect("the resize frame");
    assert!(resize.is_synchronous());
    assert!(
        matches!(resize.outcome(), Outcome::Synchronous),
        "not judged as late: {:?}",
        resize.outcome()
    );
    let counts = pacing.counts();
    assert_eq!(
        (counts.on_time, counts.late, counts.synchronous),
        (8, 0, 1),
        "{counts:?}"
    );

    let paced = pacing.frame(2).expect("a paced frame");
    let plan = paced.plan().expect("its plan");
    assert_eq!(
        plan.interval(),
        Some(core::time::Duration::from_nanos(REFRESH))
    );
    assert_eq!(plan.depth(), Some(1));
    assert!(!plan.is_synchronous());
}
