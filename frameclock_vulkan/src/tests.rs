// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;

fn domain() -> TimeDomain {
    TimeDomain {
        swapchain: SwapchainId(4),
        clock: Clock::SwapchainLocal,
        id: 7,
    }
}

fn correlation() -> ClockCorrelation {
    ClockCorrelation::new(
        domain(),
        5_000,
        Timebase::NANOS,
        HostTime(1_000),
        Timebase::NANOS,
        5,
        HostTime(2_000),
    )
    .unwrap()
}

fn key(id: u64) -> PresentKey {
    PresentKey {
        swapchain: SwapchainId(4),
        id: PresentId::Khr(core::num::NonZeroU64::new(id).unwrap()),
    }
}

#[test]
fn different_epochs_map_in_both_directions_without_retyping_foreign_time() {
    let clock = correlation();
    assert_eq!(
        clock.to_host(domain(), 5_100, HostTime(1_500)).unwrap(),
        MappedTime {
            time: HostTime(1_100),
            uncertainty: Duration(6)
        }
    );
    assert_eq!(
        clock
            .to_host(domain(), 4_900, HostTime(1_500))
            .unwrap()
            .time,
        HostTime(900)
    );
    assert_eq!(
        clock.to_source(domain(), HostTime(1_100), HostTime(1_500), Duration(6)),
        Ok(5_100)
    );
}

#[test]
fn a_mach_host_uses_its_real_tick_scale() {
    let mach = Timebase::new(125, 3);
    let clock = ClockCorrelation::new(
        domain(),
        1_000_000,
        Timebase::NANOS,
        HostTime(24_000),
        mach,
        100,
        HostTime(240_000),
    )
    .unwrap();
    let mapped = clock
        .to_host(domain(), 2_000_000, HostTime(100_000))
        .unwrap();
    assert_eq!(mapped.time, HostTime(48_000));
    assert_eq!(mapped.uncertainty, Duration(4));
    assert_eq!(
        clock.to_source(domain(), mapped.time, HostTime(100_000), Duration(4)),
        Ok(2_000_000)
    );
}

#[test]
fn domains_stages_generations_and_extrapolation_windows_are_explicit() {
    let clock = correlation();
    for changed in [
        TimeDomain { id: 8, ..domain() },
        TimeDomain {
            swapchain: SwapchainId(5),
            ..domain()
        },
        TimeDomain {
            clock: Clock::StageLocal(PresentStage::FirstPixelVisible),
            ..domain()
        },
    ] {
        assert_eq!(
            clock.to_host(changed, 5_100, HostTime(1_500)),
            Err(ClockError::DomainMismatch)
        );
    }
    assert_eq!(
        clock.to_host(domain(), 5_100, HostTime(2_001)),
        Err(ClockError::Expired)
    );
    assert_eq!(
        clock.to_host(domain(), 7_000, HostTime(1_500)),
        Err(ClockError::Expired)
    );
}

#[test]
fn conversion_rejects_unrepresentable_readings_and_zero_absolute_targets() {
    let clock = ClockCorrelation::new(
        domain(),
        1_000,
        Timebase::NANOS,
        HostTime(10),
        Timebase::NANOS,
        0,
        HostTime(2_000),
    )
    .unwrap();
    assert_eq!(
        clock.to_host(domain(), 500, HostTime(20)),
        Err(ClockError::OutOfRange)
    );
    assert_eq!(
        clock.to_source(domain(), HostTime(1_010), HostTime(20), Duration::ZERO),
        Err(ClockError::TooUncertain)
    );
    let zero = ClockCorrelation::new(
        domain(),
        10,
        Timebase::NANOS,
        HostTime(20),
        Timebase::NANOS,
        0,
        HostTime(100),
    )
    .unwrap();
    assert_eq!(
        zero.to_source(domain(), HostTime(10), HostTime(30), Duration(1)),
        Err(ClockError::OutOfRange)
    );
}

#[test]
fn partial_reports_and_nonvisible_stages_do_not_prove_presentation() {
    let mut tracker = PresentTracker::new(4);
    tracker.register(key(1), 11).unwrap();
    let ready = [StageTime {
        stage: PresentStage::QueueOperationsEnd,
        time: 5_010,
    }];
    assert_eq!(
        tracker
            .resolve_ext(
                key(1),
                domain(),
                &ready,
                false,
                Some(correlation()),
                HostTime(1_500),
                Duration(10)
            )
            .unwrap(),
        None
    );
    assert_eq!(tracker.pending_count(), 1);
    let report = tracker
        .resolve_ext(
            key(1),
            domain(),
            &ready,
            true,
            Some(correlation()),
            HostTime(1_500),
            Duration(10),
        )
        .unwrap()
        .unwrap();
    assert_eq!(report.report.outcome, PresentOutcome::Unknown);
}

#[test]
fn out_of_order_feedback_resolves_only_its_own_frame() {
    let mut tracker = PresentTracker::new(4);
    tracker.register(key(1), 11).unwrap();
    tracker.register(key(2), 12).unwrap();
    for (id, frame) in [(2, 12), (1, 11)] {
        let stages = [StageTime {
            stage: PresentStage::FirstPixelVisible,
            time: 5_100,
        }];
        let result = tracker
            .resolve_ext(
                key(id),
                domain(),
                &stages,
                false,
                Some(correlation()),
                HostTime(1_500),
                Duration(6),
            )
            .unwrap()
            .unwrap();
        assert_eq!(result.frame_index, frame);
        assert_eq!(
            result.report.outcome,
            PresentOutcome::Presented(HostTime(1_100))
        );
    }
    assert_eq!(tracker.pending_count(), 0);
}

#[test]
fn fallback_domains_and_excess_uncertainty_degrade_to_unknown() {
    let mut tracker = PresentTracker::new(4);
    let stages = [StageTime {
        stage: PresentStage::FirstPixelVisible,
        time: 5_100,
    }];
    tracker.register(key(1), 1).unwrap();
    let result = tracker
        .resolve_ext(
            key(1),
            TimeDomain { id: 99, ..domain() },
            &stages,
            true,
            Some(correlation()),
            HostTime(1_500),
            Duration(10),
        )
        .unwrap()
        .unwrap();
    assert_eq!(result.clock_error, Some(ClockError::DomainMismatch));
    assert_eq!(result.report.outcome, PresentOutcome::Unknown);
    tracker.register(key(2), 2).unwrap();
    let result = tracker
        .resolve_ext(
            key(2),
            domain(),
            &stages,
            true,
            Some(correlation()),
            HostTime(1_500),
            Duration(5),
        )
        .unwrap()
        .unwrap();
    assert_eq!(result.clock_error, Some(ClockError::TooUncertain));
    assert_eq!(result.report.outcome, PresentOutcome::Unknown);
}

#[test]
fn capacity_duplicates_and_swapchain_recreation_never_alias_frames() {
    let mut tracker = PresentTracker::new(1);
    tracker.register(key(1), 1).unwrap();
    assert_eq!(
        tracker.register(key(1), 2),
        Err(CorrelationError::Duplicate)
    );
    assert_eq!(tracker.register(key(2), 2), Err(CorrelationError::Full));
    assert_eq!(tracker.abandon_swapchain(SwapchainId(4)), alloc::vec![1]);
    let new_key = PresentKey {
        swapchain: SwapchainId(5),
        ..key(1)
    };
    tracker.register(new_key, 2).unwrap();
    assert_eq!(
        tracker.resolve_ext(
            key(1),
            domain(),
            &[],
            true,
            None,
            HostTime(1_500),
            Duration(10)
        ),
        Err(CorrelationError::UnknownPresent)
    );
    assert_eq!(tracker.pending_count(), 1);
}

#[test]
fn google_zero_identifier_is_valid_but_unavailable_timestamp_is_not_a_drop() {
    let mut tracker = PresentTracker::new(1);
    let google = PresentKey {
        swapchain: SwapchainId(4),
        id: PresentId::Google(0),
    };
    tracker.register(google, 5).unwrap();
    let result = tracker
        .resolve_google(
            google,
            domain(),
            0,
            Some(correlation()),
            HostTime(1_500),
            Duration(10),
        )
        .unwrap();
    assert_eq!(result.frame_index, 5);
    assert_eq!(result.report.outcome, PresentOutcome::Unknown);
}
