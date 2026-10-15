// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `FrameTick` construction from QPC reads.

use frameclock::time::Timebase;
use frameclock::{Duration, FrameTick, HostTime, OutputId};

/// Build a [`FrameTick`] from QPC. Call inside a `VSync`-paced tick handler.
///
/// `refresh_interval_ns` is the display's refresh interval in nanoseconds,
/// or `0` when unknown. The tick reports it, like every time it carries, in
/// QPC ticks.
///
/// The predicted present is the first refresh at or after now on the grid of
/// `prev_present_time`, or one refresh after now when no previous present is
/// known.
#[must_use]
pub fn make_tick(refresh_interval_ns: u64, prev_present_time: Option<HostTime>) -> FrameTick {
    tick_at(
        crate::time::now(),
        crate::time::timebase(),
        refresh_interval_ns,
        prev_present_time,
    )
}

/// Builds the tick for `now`, converting the refresh interval from
/// nanoseconds to ticks of `timebase`.
fn tick_at(
    now: HostTime,
    timebase: Timebase,
    refresh_interval_ns: u64,
    prev_present_time: Option<HostTime>,
) -> FrameTick {
    let interval_ticks = timebase.nanos_to_ticks(refresh_interval_ns);
    let predicted_present =
        (interval_ticks > 0).then(|| predict_next_present(now, interval_ticks, prev_present_time));

    FrameTick {
        now,
        predicted_present,
        refresh_interval: (interval_ticks > 0).then_some(Duration(interval_ticks)),
        output: OutputId(0),
        prev_actual_present: prev_present_time,
    }
}

/// Predicts the next refresh from the previous present, or from `now`.
///
/// A previous present is advanced by whole refresh intervals until it reaches
/// `now`, so a host that was idle for many refreshes still predicts a time on
/// the display's refresh grid rather than one in the past. Saturates at
/// `HostTime(u64::MAX)`.
fn predict_next_present(
    now: HostTime,
    interval_ticks: u64,
    prev_present_time: Option<HostTime>,
) -> HostTime {
    let Some(prev) = prev_present_time else {
        return HostTime(now.ticks().saturating_add(interval_ticks));
    };
    let elapsed = now.ticks().saturating_sub(prev.ticks());
    let advance = elapsed
        .div_ceil(interval_ticks)
        .saturating_mul(interval_ticks);
    HostTime(prev.ticks().saturating_add(advance))
}

#[cfg(test)]
mod tests {
    use super::{make_tick, tick_at};
    use frameclock::time::Timebase;
    use frameclock::{Duration, HostTime};

    /// A 10 MHz QPC, the usual frequency: 100 ns per tick.
    const QPC_10_MHZ: Timebase = Timebase::new(1_000_000_000, 10_000_000);

    #[test]
    fn refresh_interval_is_in_qpc_ticks() {
        let tick = tick_at(HostTime(5_000_000), QPC_10_MHZ, 16_666_667, None);
        // 16.67 ms is 166,666 ticks of 100 ns, not 16,666,667 ticks (1.67 s).
        assert_eq!(tick.refresh_interval, Some(Duration(166_666)));
        assert_eq!(tick.predicted_present, Some(HostTime(5_166_666)));
    }

    #[test]
    fn prediction_follows_the_previous_present() {
        let prev = HostTime(4_900_000);
        let tick = tick_at(HostTime(5_000_000), QPC_10_MHZ, 16_666_667, Some(prev));
        assert_eq!(tick.predicted_present, Some(HostTime(5_066_666)));
        assert_eq!(tick.prev_actual_present, Some(prev));
    }

    #[test]
    fn prediction_after_idle_lands_on_the_next_refresh() {
        // One second idle at 60 Hz: the next refresh on the previous
        // present's grid is the 61st, not one refresh after it.
        let prev = HostTime(4_000_000);
        let tick = tick_at(HostTime(14_000_000), QPC_10_MHZ, 16_666_667, Some(prev));
        assert_eq!(
            tick.predicted_present,
            Some(HostTime(4_000_000 + 61 * 166_666))
        );
        assert!(tick.predicted_present >= Some(tick.now));
    }

    #[test]
    fn prediction_saturates() {
        let tick = tick_at(
            HostTime(u64::MAX - 10),
            QPC_10_MHZ,
            16_666_667,
            Some(HostTime(u64::MAX - 100_000)),
        );
        assert_eq!(tick.predicted_present, Some(HostTime(u64::MAX)));
        let tick = tick_at(HostTime(u64::MAX - 10), QPC_10_MHZ, 16_666_667, None);
        assert_eq!(tick.predicted_present, Some(HostTime(u64::MAX)));
    }

    #[test]
    fn make_tick_with_refresh_and_prev() {
        let prev = HostTime(1_000_000);
        let tick = make_tick(16_666_667, Some(prev));
        assert_eq!(tick.prev_actual_present, Some(prev));
        assert!(tick.predicted_present.is_some());
        assert!(tick.refresh_interval.is_some());
    }

    #[test]
    fn make_tick_zero_refresh() {
        let tick = make_tick(0, None);
        assert_eq!(tick.predicted_present, None);
        assert_eq!(tick.refresh_interval, None);
        assert_eq!(tick.prev_actual_present, None);
    }

    #[test]
    fn make_tick_first_frame_predicts_from_now() {
        let tick = make_tick(16_666_667, None);
        // First frame with no prev: predicted_present = now + interval
        let predicted = tick.predicted_present.unwrap();
        assert!(predicted.ticks() > tick.now.ticks());
    }
}
