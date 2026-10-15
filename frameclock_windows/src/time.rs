// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! QPC-based timing functions for [`HostTime`] / [`Timebase`].

use std::sync::OnceLock;

use frameclock::HostTime;
use frameclock::time::Timebase;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Cached QPC frequency — constant for the lifetime of the system.
static QPC_FREQ: OnceLock<i64> = OnceLock::new();

fn cached_frequency() -> i64 {
    *QPC_FREQ.get_or_init(|| {
        let mut freq = 0_i64;
        unsafe { QueryPerformanceFrequency(&mut freq).unwrap() };
        freq
    })
}

/// Current monotonic time as a [`HostTime`] (raw QPC ticks).
#[must_use]
#[expect(clippy::cast_sign_loss, reason = "QPC values are always non-negative")]
pub fn now() -> HostTime {
    let mut count = 0_i64;
    unsafe { QueryPerformanceCounter(&mut count).unwrap() };
    HostTime(count as u64)
}

/// Conversion factor from QPC ticks to nanoseconds.
///
/// `nanos = ticks * numer / denom`
/// → `numer / denom = 1_000_000_000 / QPC frequency`, reduced to lowest terms
///
/// # Panics
///
/// Panics if the reported QPC frequency is not positive, or if the reduced
/// ratio does not fit in a [`Timebase`].
#[must_use]
pub fn timebase() -> Timebase {
    let freq = cached_frequency();
    timebase_for_frequency(freq)
        .unwrap_or_else(|| panic!("QPC frequency {freq} has no u32 timebase"))
}

/// Reduces `1_000_000_000 / freq` to a [`Timebase`], or `None` if `freq` is not
/// positive or the reduced terms do not fit in `u32`.
fn timebase_for_frequency(freq: i64) -> Option<Timebase> {
    const NANOS_PER_SECOND: u64 = 1_000_000_000;
    let freq = u64::try_from(freq).ok().filter(|freq| *freq > 0)?;
    let divisor = gcd(NANOS_PER_SECOND, freq);
    let numer = u32::try_from(NANOS_PER_SECOND / divisor).ok()?;
    let denom = u32::try_from(freq / divisor).ok()?;
    Timebase::try_new(numer, denom)
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::{cached_frequency, now, timebase, timebase_for_frequency};

    #[test]
    fn timebase_is_valid() {
        let freq = u64::try_from(cached_frequency()).expect("QPC frequency is positive");
        assert_eq!(
            timebase().ticks_to_nanos(freq),
            1_000_000_000,
            "one second of QPC ticks"
        );
    }

    #[test]
    fn frequencies_reduce_to_lowest_terms() {
        let tb = timebase_for_frequency(10_000_000).expect("10 MHz");
        assert_eq!((tb.numer(), tb.denom()), (100, 1));
        let tb = timebase_for_frequency(10_000_000_000).expect("10 GHz");
        assert_eq!((tb.numer(), tb.denom()), (1, 10));
        let tb = timebase_for_frequency(3_579_545).expect("ACPI PM timer");
        assert_eq!((tb.numer(), tb.denom()), (200_000_000, 715_909));
    }

    #[test]
    fn unusable_frequencies_have_no_timebase() {
        assert_eq!(timebase_for_frequency(0), None);
        assert_eq!(timebase_for_frequency(-1), None);
        // A prime frequency above `u32::MAX` cannot be reduced into range.
        assert_eq!(timebase_for_frequency(4_294_967_311), None);
    }

    #[test]
    fn now_is_monotonic() {
        let a = now();
        let b = now();
        assert!(b.ticks() >= a.ticks(), "QPC must be monotonic");
    }
}
