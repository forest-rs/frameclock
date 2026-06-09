// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Affine clock for A/V synchronization.
//!
//! [`AffineClock`] maintains a smoothed affine mapping from host time to media
//! time:
//!
//! ```text
//! media_time = rate * host_time + offset
//! ```
//!
//! Rate and offset are updated smoothly (via EMA) each time a new observation
//! is fed in, avoiding jitter while tracking drift.

/// A smoothed affine mapping from host time (ticks) to media time (seconds).
///
/// Feed observations via [`update`](Self::update) and query via
/// [`media_time_at`](Self::media_time_at).
#[derive(Clone, Debug)]
pub struct AffineClock {
    /// Current estimated rate (media seconds per host tick).
    rate: f64,
    /// Initial rate supplied at construction, restored by [`reset`](Self::reset).
    initial_rate: f64,
    /// Current estimated offset (media seconds).
    offset: f64,
    /// EMA smoothing factor for rate correction (0.0–1.0).
    rate_alpha: f64,
    /// EMA smoothing factor for offset correction (0.0–1.0).
    offset_alpha: f64,
    /// Whether at least one observation has been fed.
    initialized: bool,
    /// Last host time observation (for rate estimation).
    last_host: u64,
    /// Last media time observation.
    last_media: f64,
}

impl AffineClock {
    /// Creates a new clock with the given initial rate and EMA smoothing
    /// factors.
    ///
    /// `initial_rate` is in media-seconds per host-tick (e.g. for nanosecond
    /// ticks, this would be `1e-9`). It is also the rate that
    /// [`reset`](Self::reset) restores, so the value chosen here sets the
    /// post-reset baseline.
    #[must_use]
    pub fn new(initial_rate: f64, rate_alpha: f64, offset_alpha: f64) -> Self {
        Self {
            rate: initial_rate,
            initial_rate,
            offset: 0.0,
            rate_alpha,
            offset_alpha,
            initialized: false,
            last_host: 0,
            last_media: 0.0,
        }
    }

    /// Queries the estimated media time at the given host time.
    ///
    /// Returns `None` if no observations have been fed yet.
    #[must_use]
    pub fn media_time_at(&self, host_ticks: u64) -> Option<f64> {
        if !self.initialized {
            return None;
        }
        Some(self.rate * host_ticks as f64 + self.offset)
    }

    /// Feeds an observation of `(host_time, media_time)` to update the
    /// mapping.
    ///
    /// On the first call, this sets the mapping exactly. Subsequent calls
    /// smooth the rate and offset via EMA.
    ///
    /// A non-finite `media_time` (NaN or infinity) is ignored, leaving the
    /// mapping unchanged, so a single bad observation cannot poison the clock.
    pub fn update(&mut self, host_ticks: u64, media_time: f64) {
        if !media_time.is_finite() {
            return;
        }
        if !self.initialized {
            // First observation: set mapping exactly.
            // offset = media_time - rate * host_ticks
            self.offset = media_time - self.rate * host_ticks as f64;
            self.last_host = host_ticks;
            self.last_media = media_time;
            self.initialized = true;
            return;
        }

        let dt_host = host_ticks.saturating_sub(self.last_host);
        if dt_host > 0 {
            // Estimate instantaneous rate from this pair of observations.
            let dt_media = media_time - self.last_media;
            let observed_rate = dt_media / dt_host as f64;

            // Smooth rate.
            self.rate = self.rate_alpha * observed_rate + (1.0 - self.rate_alpha) * self.rate;
        }

        // Compute offset from current rate.
        let predicted_media = self.rate * host_ticks as f64 + self.offset;
        let offset_error = media_time - predicted_media;

        // Smooth offset correction.
        self.offset += self.offset_alpha * offset_error;

        self.last_host = host_ticks;
        self.last_media = media_time;
    }

    /// Resets all accumulated state — including the rate, which is restored to
    /// the value passed to [`new`](Self::new) — requiring new observations
    /// before queries return values.
    pub fn reset(&mut self) {
        self.rate = self.initial_rate;
        self.offset = 0.0;
        self.initialized = false;
        self.last_host = 0;
        self.last_media = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninitialized_returns_none() {
        let clock = AffineClock::new(1e-9, 0.1, 0.1);
        assert!(clock.media_time_at(1_000_000_000).is_none());
    }

    #[test]
    fn first_observation_sets_mapping_exactly() {
        let mut clock = AffineClock::new(1e-9, 0.1, 0.1);
        // 1 second of host ticks (at 1ns resolution) = 1.0s media time.
        clock.update(1_000_000_000, 1.0);

        let mt = clock.media_time_at(2_000_000_000).unwrap();
        // rate * 2e9 + offset = 1e-9 * 2e9 + (1.0 - 1e-9 * 1e9) = 2.0 + 0.0 = 2.0
        assert!((mt - 2.0).abs() < 1e-6, "expected ~2.0, got {mt}");
    }

    #[test]
    fn rate_converges() {
        // Rate is initially 1e-9 (1ns ticks = seconds).
        // Feed observations at exactly that rate; rate should stay stable.
        let mut clock = AffineClock::new(1e-9, 0.1, 0.1);
        clock.update(0, 0.0);
        for i in 1..=10 {
            let host = i * 1_000_000_000_u64;
            let media = i as f64;
            clock.update(host, media);
        }

        let mt = clock.media_time_at(11_000_000_000).unwrap();
        assert!((mt - 11.0).abs() < 0.1, "expected ~11.0, got {mt}");
    }

    #[test]
    fn reset_clears_state() {
        let mut clock = AffineClock::new(1e-9, 0.1, 0.1);
        clock.update(1_000_000_000, 1.0);
        assert!(clock.media_time_at(2_000_000_000).is_some());

        clock.reset();
        assert!(clock.media_time_at(2_000_000_000).is_none());
    }

    #[test]
    fn reset_restores_initial_rate() {
        let mut clock = AffineClock::new(1e-9, 0.5, 0.5);
        clock.update(0, 0.0);
        // Drive media at twice the initial rate so the EMA drifts toward 2e-9.
        for i in 1..=30_u64 {
            clock.update(i * 1_000_000_000, 2.0 * i as f64);
        }
        assert!(
            clock.rate > 1.5e-9,
            "precondition: rate should have drifted up"
        );

        clock.reset();

        assert_eq!(clock.rate, 1e-9, "reset must restore the initial rate");
    }

    #[test]
    fn ignores_non_finite_first_observation() {
        let mut clock = AffineClock::new(1e-9, 0.1, 0.1);
        // Garbage before any good observation must not initialize the clock.
        clock.update(1_000_000_000, f64::NAN);
        clock.update(1_000_000_000, f64::INFINITY);
        assert!(clock.media_time_at(2_000_000_000).is_none());

        // A finite observation still initializes normally.
        clock.update(1_000_000_000, 1.0);
        assert!(clock.media_time_at(2_000_000_000).is_some());
    }

    #[test]
    fn ignores_non_finite_observation_after_init() {
        let mut clock = AffineClock::new(1e-9, 0.1, 0.1);
        clock.update(0, 0.0);
        clock.update(1_000_000_000, 1.0);
        let before = clock.media_time_at(2_000_000_000).unwrap();

        // Garbage observations leave the mapping untouched.
        clock.update(2_000_000_000, f64::NAN);
        clock.update(3_000_000_000, f64::INFINITY);
        clock.update(4_000_000_000, f64::NEG_INFINITY);
        assert_eq!(clock.media_time_at(2_000_000_000).unwrap(), before);

        // A subsequent good observation is still applied.
        clock.update(2_000_000_000, 2.0);
        assert!((clock.media_time_at(2_000_000_000).unwrap() - 2.0).abs() < 1e-6);
    }
}
