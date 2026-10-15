// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `FrameTick` construction from QPC reads.

use frameclock::timing::next_refresh;
use frameclock::{Duration, FrameTick, HostTime, OutputId};

/// Builds a [`FrameTick`] for `output` from QPC. Call inside a `VSync`-paced
/// tick handler.
///
/// `refresh_interval` is the display's refresh interval in QPC ticks, like
/// every time the tick carries, or `None` when unknown; a zero interval counts
/// as unknown. `DWM_TIMING_INFO::qpcRefreshPeriod` is already in QPC ticks.
/// Convert a nanosecond interval with [`Duration::from_nanos`] and
/// [`timebase`](crate::timebase).
///
/// The predicted present is the first refresh at or after now on the grid of
/// `prev_present_time`, or one refresh after now when no previous present is
/// known.
#[must_use]
pub fn make_tick(
    output: OutputId,
    refresh_interval: Option<Duration>,
    prev_present_time: Option<HostTime>,
) -> FrameTick {
    tick_at(
        crate::time::now(),
        output,
        refresh_interval,
        prev_present_time,
    )
}

/// Builds the tick for `now`.
fn tick_at(
    now: HostTime,
    output: OutputId,
    refresh_interval: Option<Duration>,
    prev_present_time: Option<HostTime>,
) -> FrameTick {
    let refresh_interval = refresh_interval.filter(|interval| !interval.is_zero());
    let predicted_present =
        refresh_interval.map(|interval| predict_next_present(now, interval, prev_present_time));

    FrameTick {
        now,
        predicted_present,
        refresh_interval,
        output,
        prev_actual_present: prev_present_time,
    }
}

/// Predicts the next refresh from the previous present, or from `now`.
///
/// A previous present gives the next refresh at or after `now` on its grid,
/// as found by [`next_refresh`]. `interval` is not zero. Saturates at
/// `HostTime(u64::MAX)`.
fn predict_next_present(
    now: HostTime,
    interval: Duration,
    prev_present_time: Option<HostTime>,
) -> HostTime {
    match prev_present_time {
        Some(prev) => next_refresh(prev, interval, now).unwrap_or(prev),
        None => now.checked_add(interval).unwrap_or(HostTime(u64::MAX)),
    }
}

#[cfg(test)]
mod tests {
    use super::{make_tick, tick_at};
    use frameclock::{Duration, HostTime, OutputId};

    /// 16.67 ms in ticks of a 10 MHz QPC, the usual frequency.
    const REFRESH: Duration = Duration(166_666);

    #[test]
    fn tick_reports_the_interval_and_output() {
        let tick = tick_at(HostTime(5_000_000), OutputId(3), Some(REFRESH), None);
        assert_eq!(tick.output, OutputId(3));
        assert_eq!(tick.refresh_interval, Some(REFRESH));
        assert_eq!(tick.predicted_present, Some(HostTime(5_166_666)));
    }

    #[test]
    fn prediction_follows_the_previous_present() {
        let prev = HostTime(4_900_000);
        let tick = tick_at(HostTime(5_000_000), OutputId(0), Some(REFRESH), Some(prev));
        assert_eq!(tick.predicted_present, Some(HostTime(5_066_666)));
        assert_eq!(tick.prev_actual_present, Some(prev));
    }

    #[test]
    fn prediction_after_idle_lands_on_the_next_refresh() {
        // One second idle at 60 Hz: the next refresh on the previous
        // present's grid is the 61st, not one refresh after it.
        let prev = HostTime(4_000_000);
        let tick = tick_at(HostTime(14_000_000), OutputId(0), Some(REFRESH), Some(prev));
        assert_eq!(
            tick.predicted_present,
            Some(HostTime(4_000_000 + 61 * 166_666))
        );
        assert!(tick.predicted_present >= Some(tick.now));
    }

    #[test]
    fn prediction_saturates() {
        let now = HostTime(u64::MAX - 10);
        let prev = Some(HostTime(u64::MAX - 100_000));
        let tick = tick_at(now, OutputId(0), Some(REFRESH), prev);
        assert_eq!(tick.predicted_present, Some(HostTime(u64::MAX)));
        let tick = tick_at(now, OutputId(0), Some(REFRESH), None);
        assert_eq!(tick.predicted_present, Some(HostTime(u64::MAX)));
    }

    #[test]
    fn unknown_or_zero_interval_gives_no_prediction() {
        for refresh_interval in [None, Some(Duration::ZERO)] {
            let tick = tick_at(HostTime(5_000_000), OutputId(0), refresh_interval, None);
            assert_eq!(tick.predicted_present, None);
            assert_eq!(tick.refresh_interval, None);
        }
    }

    #[test]
    fn make_tick_with_refresh_and_prev() {
        let prev = HostTime(1_000_000);
        let tick = make_tick(OutputId(1), Some(REFRESH), Some(prev));
        assert_eq!(tick.output, OutputId(1));
        assert_eq!(tick.prev_actual_present, Some(prev));
        assert!(tick.predicted_present.is_some());
        assert_eq!(tick.refresh_interval, Some(REFRESH));
    }

    #[test]
    fn make_tick_first_frame_predicts_from_now() {
        let tick = make_tick(OutputId(0), Some(REFRESH), None);
        let predicted = tick.predicted_present.unwrap();
        assert!(predicted.ticks() > tick.now.ticks());
    }
}
