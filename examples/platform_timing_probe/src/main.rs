// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Deterministic API integration probe; it creates no device or native window.
//!
//! The ash structures below are the real GOOGLE display-timing request and
//! feedback types. Android builds also type-check copying API 33 callback data
//! from the real NDK bindings. Values in this executable are synthetic API
//! facts, so success establishes integration invariants, not hardware pacing.

use ash::vk;
use frameclock::time::Timebase;
use frameclock::{
    DisplayTiming, Duration, FrameBeginResult, FrameDemand, FrameDriver, FrameSubmission, HostTime,
    OutputId,
};
use frameclock_android::{FrameBudget, FrameTimeline, FrameTimelines};
use frameclock_vulkan::{
    Clock, ClockCorrelation, PresentId, PresentKey, PresentTracker, SwapchainId, TimeDomain,
};

#[cfg(target_os = "android")]
mod android_bindings;

fn main() {
    #[cfg(target_os = "android")]
    let _decoder = android_bindings::copy_callback;

    let timelines = [FrameTimeline::new(101, 16_000_000, 12_000_000).unwrap()];
    let selected = FrameTimelines {
        frame_time: HostTime(0),
        preferred_index: 0,
        timelines: &timelines,
    }
    .select(
        HostTime(1_000_000),
        FrameBudget {
            build: Duration(1_000_000),
            ready_lead: Duration(2_000_000),
        },
        Some(Duration(8_000_000)),
        DisplayTiming::fixed(Duration(8_000_000)),
        OutputId(0),
    )
    .unwrap();
    let mut driver = FrameDriver::new(frameclock_android::scheduler_config());
    driver.request(FrameDemand::INPUT);
    let FrameBeginResult::Ready(frame) = driver.begin_frame(selected.frame_opportunity()).result
    else {
        panic!("input frame must be ready")
    };
    let native = selected.bind(&frame.plan()).unwrap();

    let domain = TimeDomain {
        swapchain: SwapchainId(1),
        clock: Clock::Monotonic,
        id: 0,
    };
    // Android and GOOGLE timing share CLOCK_MONOTONIC nanoseconds here.
    // Other host clocks still require an explicit calibrated correlation.
    let correlation = ClockCorrelation::new(
        domain,
        1_000_000,
        Timebase::NANOS,
        HostTime(1_000_000),
        Timebase::NANOS,
        100,
        HostTime(100_000_000),
    )
    .unwrap();
    let raw_target = correlation
        .to_source(
            domain,
            native.expected_present,
            HostTime(1_000_000),
            Duration(200),
        )
        .unwrap();
    let request = vk::PresentTimeGOOGLE::default()
        .present_id(7)
        .desired_present_time(raw_target);
    let requests = [request];
    let _present_chain = vk::PresentTimesInfoGOOGLE::default().times(&requests);
    let frame_index = frame.plan().frame_index;
    let _ = driver.submit_frame(frame, FrameSubmission::reported(HostTime(2_000_000)));
    let key = PresentKey {
        swapchain: domain.swapchain,
        id: PresentId::Google(request.present_id),
    };
    let mut tracker = PresentTracker::new(8);
    tracker.register(key, frame_index).unwrap();

    let feedback = vk::PastPresentationTimingGOOGLE::default()
        .present_id(request.present_id)
        .desired_present_time(request.desired_present_time)
        .actual_present_time(request.desired_present_time + 100_000);
    let resolved = tracker
        .resolve_google(
            key,
            domain,
            feedback.actual_present_time,
            Some(correlation),
            HostTime(20_000_000),
            Duration(200),
        )
        .unwrap();
    let summary = driver
        .report_frame(resolved.frame_index, resolved.report)
        .unwrap();
    assert_eq!(
        summary.actual_present,
        Some(HostTime(16_100_000)),
        "feedback must return to the host epoch"
    );
    assert_eq!(
        summary.expected_present,
        Some(native.expected_present),
        "planning must retain the chosen native target"
    );
    println!(
        "native vsync {}, frame {}, Vulkan present {}: expected {:?}, shown {:?}, CPU deadline {:?}",
        native.vsync_id,
        frame_index,
        feedback.present_id,
        summary.expected_present,
        summary.actual_present,
        native.commit_deadline
    );
}
