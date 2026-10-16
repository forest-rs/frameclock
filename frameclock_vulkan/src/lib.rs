// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Experimental Vulkan presentation timing conversion and correlation.
//!
//! This crate owns timing data and submission identity. The host owns Vulkan
//! calls, extension negotiation, swapchains, synchronization, and pacing waits.
//! [`HostTime`] always remains the CPU clock used by the host's frame driver.
//! Vulkan clock readings become host times only through [`ClockCorrelation`].
//! The structures mirror the timing facts in `VK_EXT_present_timing` and
//! `VK_GOOGLE_display_timing`; they contain no Vulkan handles or borrowed FFI
//! pointers. Stage-local correlations include the swapchain, stage, and opaque
//! domain ID. Re-query and replace a correlation after domain changes.
//!
//! A successful present wait is useful backpressure, but supplies no timestamp
//! for [`PresentOutcome::Presented`]. Partial reports and zero/unavailable
//! stage timestamps likewise do not prove an image was displayed.
//!
//! Ask a timed present for the plan's
//! [`requested_present`](frameclock::timing::FramePlan::requested_present),
//! converted with [`ClockCorrelation::to_source`], not its target: the target
//! carries the presentation latency the scheduler learned, and a present held
//! until then makes that latency come true for good. Submit each frame with
//! [`FrameSubmission::reported`](frameclock::FrameSubmission::reported) and
//! pass each [`ResolvedFrame`] to
//! [`FrameDriver::report_frame`](frameclock::FrameDriver::report_frame): the
//! driver learns presentation latency, and drains a frame queued in the
//! presentation pipeline, only from reported presentation times.
//!
//! Protocol details follow the Khronos references for
//! [`VK_EXT_present_timing`](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_present_timing.html),
//! [`VkTimeDomainKHR`](https://docs.vulkan.org/refpages/latest/refpages/source/VkTimeDomainKHR.html),
//! and [`VK_GOOGLE_display_timing`](https://docs.vulkan.org/refpages/latest/refpages/source/VK_GOOGLE_display_timing.html).
//! This experiment accepts copied protocol facts; it performs no Vulkan calls.
//! Hosts must check extension features, supported stages and clock domains,
//! allocate the timing-results queue, and handle submission failures. Relative
//! EXT presentation requests require their own reference semantics; only
//! absolute targets are converted by [`ClockCorrelation::to_source`].
//! The separate `platform_timing_probe` example constructs real ash GOOGLE
//! timing structures and routes a synthetic report back through a frame driver.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use frameclock::time::Timebase;
use frameclock::{Duration, FrameReport, HostTime, PresentOutcome};

/// Host-assigned swapchain generation, changed whenever a swapchain is replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SwapchainId(pub u64);

/// A presentation stage reported by `VK_EXT_present_timing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresentStage {
    /// Queue operations, including semaphore waits, finished.
    QueueOperationsEnd,
    /// The presentation request left the internal queue.
    RequestDequeued,
    /// The presentation engine sent the first pixel towards a display.
    FirstPixelOut,
    /// The display made the first pixel visible.
    FirstPixelVisible,
}

/// Vulkan clock scope. This is distinct from a host clock reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Clock {
    /// POSIX `CLOCK_MONOTONIC`.
    Monotonic,
    /// POSIX `CLOCK_MONOTONIC_RAW`.
    MonotonicRaw,
    /// Windows performance counter, whose native units require a timebase.
    PerformanceCounter,
    /// Implementation-defined GOOGLE presentation clock, such as `MoltenVK`'s
    /// Core Animation media clock in nanoseconds. It requires an explicit
    /// host correlation and is not a Vulkan calibrated-timestamp domain.
    Presentation,
    /// An opaque clock local to a swapchain.
    SwapchainLocal,
    /// An opaque clock local to a swapchain and one presentation stage.
    StageLocal(PresentStage),
}

/// Exact scope of a clock calibration, including its opaque domain ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimeDomain {
    /// Swapchain generation for which calibration was obtained.
    pub swapchain: SwapchainId,
    /// Clock selected by the presentation engine.
    pub clock: Clock,
    /// Opaque `timeDomainId`, including IDs returned by fallback reports.
    pub id: u64,
}

/// Why a reading cannot safely be converted into the driver's host clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClockError {
    /// The report belongs to another swapchain, stage, or clock-domain ID.
    DomainMismatch,
    /// The calibration is outside its host-assigned validity window.
    Expired,
    /// Conversion would exceed the timestamp range.
    OutOfRange,
    /// The caller's accepted uncertainty is smaller than the calibration error.
    TooUncertain,
    /// The validity window ends before the calibration sample.
    InvalidWindow,
}

/// A converted reading, with an explicit error bound in host ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MappedTime {
    /// Estimated reading in the frame driver's CPU clock.
    pub time: HostTime,
    /// Calibration deviation plus conservative conversion rounding.
    pub uncertainty: Duration,
}

/// A calibrated pair of clock readings with explicit scales and validity.
///
/// The host supplies a pair obtained from a calibrated timestamp query or
/// another justified clock correlation. `max_error_ns` includes the query's
/// deviation and the host's extrapolation error budget over the validity
/// window. Source readings may be at most that window's span from the anchor.
/// No implicit common epoch or nanosecond host timebase is assumed.
#[derive(Clone, Copy, Debug)]
pub struct ClockCorrelation {
    domain: TimeDomain,
    source_sample: u64,
    source_timebase: Timebase,
    host_sample: HostTime,
    host_timebase: Timebase,
    valid_until: HostTime,
    uncertainty: Duration,
}

impl ClockCorrelation {
    /// Creates a correlation from a calibrated pair of readings.
    ///
    /// `source_timebase` describes the units of the supplied Vulkan reading;
    /// swapchain-local and stage-local readings use [`Timebase::NANOS`].
    ///
    /// # Errors
    /// Returns [`ClockError::InvalidWindow`] if validity ends before the sample,
    /// or [`ClockError::OutOfRange`] if its error bound cannot be represented.
    pub fn new(
        domain: TimeDomain,
        source_sample: u64,
        source_timebase: Timebase,
        host_sample: HostTime,
        host_timebase: Timebase,
        max_error_ns: u64,
        valid_until: HostTime,
    ) -> Result<Self, ClockError> {
        if valid_until < host_sample {
            return Err(ClockError::InvalidWindow);
        }
        let error = scale(max_error_ns, Timebase::NANOS, host_timebase, true)?;
        let uncertainty = Duration(error.checked_add(1).ok_or(ClockError::OutOfRange)?);
        Ok(Self {
            domain,
            source_sample,
            source_timebase,
            host_sample,
            host_timebase,
            valid_until,
            uncertainty,
        })
    }

    /// Converts a reading, rejecting stale or differently scoped calibration.
    ///
    /// # Errors
    /// Returns a domain, validity, or range error; timestamps never saturate
    /// into plausible presentation evidence.
    pub fn to_host(
        self,
        domain: TimeDomain,
        reading: u64,
        now: HostTime,
    ) -> Result<MappedTime, ClockError> {
        self.check(domain, now)?;
        let delta = scale(
            reading.abs_diff(self.source_sample),
            self.source_timebase,
            self.host_timebase,
            false,
        )?;
        if delta
            > self
                .valid_until
                .saturating_duration_since(self.host_sample)
                .ticks()
        {
            return Err(ClockError::Expired);
        }
        let time = if reading >= self.source_sample {
            self.host_sample.checked_add(Duration(delta))
        } else {
            self.host_sample.checked_sub(Duration(delta))
        }
        .ok_or(ClockError::OutOfRange)?;
        Ok(MappedTime {
            time,
            uncertainty: self.uncertainty,
        })
    }

    /// Converts a host presentation request into this Vulkan domain.
    ///
    /// Conversion rounds toward a later source reading. Zero is reserved for
    /// an untimed Vulkan request and is rejected as an absolute target here.
    ///
    /// # Errors
    /// Returns a domain, validity, range, or uncertainty error.
    pub fn to_source(
        self,
        domain: TimeDomain,
        target: HostTime,
        now: HostTime,
        max_uncertainty: Duration,
    ) -> Result<u64, ClockError> {
        self.check(domain, now)?;
        if self.uncertainty > max_uncertainty {
            return Err(ClockError::TooUncertain);
        }
        let later = target >= self.host_sample;
        if target.ticks().abs_diff(self.host_sample.ticks())
            > self
                .valid_until
                .saturating_duration_since(self.host_sample)
                .ticks()
        {
            return Err(ClockError::Expired);
        }
        let delta = scale(
            target.ticks().abs_diff(self.host_sample.ticks()),
            self.host_timebase,
            self.source_timebase,
            later,
        )?;
        let reading = if later {
            self.source_sample.checked_add(delta)
        } else {
            self.source_sample.checked_sub(delta)
        };
        reading
            .filter(|reading| *reading != 0)
            .ok_or(ClockError::OutOfRange)
    }

    fn check(self, domain: TimeDomain, now: HostTime) -> Result<(), ClockError> {
        if domain != self.domain {
            Err(ClockError::DomainMismatch)
        } else if now < self.host_sample || now > self.valid_until {
            Err(ClockError::Expired)
        } else {
            Ok(())
        }
    }
}

fn scale(value: u64, from: Timebase, to: Timebase, round_up: bool) -> Result<u64, ClockError> {
    let numerator = u128::from(value) * u128::from(from.numer()) * u128::from(to.denom());
    let denominator = u128::from(from.denom()) * u128::from(to.numer());
    let scaled = if round_up {
        numerator.div_ceil(denominator)
    } else {
        numerator / denominator
    };
    u64::try_from(scaled).map_err(|_| ClockError::OutOfRange)
}

/// A present identifier, preserving the different Vulkan identifier spaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PresentId {
    /// `VkPresentTimeGOOGLE::presentID`, including zero.
    Google(u32),
    /// Nonzero KHR present ID (the host enforces increasing submission order).
    Khr(core::num::NonZeroU64),
}

/// Identity of one present, scoped to a swapchain generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PresentKey {
    /// Swapchain generation that owns the identifier.
    pub swapchain: SwapchainId,
    /// Protocol identifier returned in feedback.
    pub id: PresentId,
}

/// One EXT present-stage result; zero means unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StageTime {
    /// Stage reported by the presentation engine.
    pub stage: PresentStage,
    /// Raw reading, with units identified by the report's domain correlation.
    pub time: u64,
}

/// Why registration or feedback identity cannot be correlated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CorrelationError {
    /// Feedback was decoded using the wrong presentation protocol's identifier.
    WrongProtocol,
    /// A present ID is still outstanding on the same swapchain.
    Duplicate,
    /// The host must resolve outstanding entries before registering more.
    Full,
    /// No outstanding frame matches this swapchain and ID.
    UnknownPresent,
}

/// A report ready for [`frameclock::FrameDriver::report_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResolvedFrame {
    /// Frameclock content-frame identity correlated at submission.
    pub frame_index: u64,
    /// Converted presentation result. GPU work may be attached by the host.
    pub report: FrameReport,
    /// Why a timestamp could not be accepted, if there was one.
    pub clock_error: Option<ClockError>,
}

/// Bounded correlation of Vulkan feedback to frameclock content frames.
///
/// Register only after successful submission. Out-of-order reports resolve
/// their own frame. Swapchain replacement uses a fresh [`SwapchainId`], so a
/// reused native handle or present ID cannot alias old feedback. On teardown,
/// [`abandon_swapchain`](Self::abandon_swapchain) returns unresolved frames for
/// the host to report as [`PresentOutcome::Unknown`].
#[derive(Debug)]
pub struct PresentTracker {
    capacity: usize,
    pending: Vec<(PresentKey, u64)>,
}

impl PresentTracker {
    /// Creates a tracker with at least one entry of capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            pending: Vec::new(),
        }
    }

    /// Registers a successfully queued present and its frameclock frame.
    ///
    /// A present the engine replaced without showing, as a mailbox swapchain
    /// can, is never reported. Resolve such frames as
    /// [`PresentOutcome::Unknown`] before the tracker fills.
    ///
    /// # Errors
    /// Returns [`CorrelationError::Duplicate`] or [`CorrelationError::Full`].
    pub fn register(&mut self, key: PresentKey, frame_index: u64) -> Result<(), CorrelationError> {
        if self.pending.iter().any(|(pending, _)| *pending == key) {
            return Err(CorrelationError::Duplicate);
        }
        if self.pending.len() == self.capacity {
            return Err(CorrelationError::Full);
        }
        self.pending.push((key, frame_index));
        Ok(())
    }

    /// Returns the number of outstanding frame correlations.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Adapts `VkPastPresentationTimingEXT` stage results.
    ///
    /// Only `FirstPixelVisible` proves presentation. Incomplete reports without
    /// it leave the frame outstanding. Completed reports lacking a visible
    /// timestamp resolve as unknown; zero never means "not presented" here.
    /// The caller explicitly chooses its acceptable host-clock uncertainty.
    ///
    /// # Errors
    /// Returns [`CorrelationError::UnknownPresent`] for unmatched feedback.
    pub fn resolve_ext(
        &mut self,
        key: PresentKey,
        domain: TimeDomain,
        stages: &[StageTime],
        complete: bool,
        correlation: Option<ClockCorrelation>,
        now: HostTime,
        max_uncertainty: Duration,
    ) -> Result<Option<ResolvedFrame>, CorrelationError> {
        if !matches!(key.id, PresentId::Khr(_)) {
            return Err(CorrelationError::WrongProtocol);
        }
        if !self.pending.iter().any(|(pending, _)| *pending == key) {
            return Err(CorrelationError::UnknownPresent);
        }
        let visible = stages
            .iter()
            .find(|sample| sample.stage == PresentStage::FirstPixelVisible && sample.time != 0)
            .map(|sample| sample.time);
        if visible.is_none() && !complete {
            return Ok(None);
        }
        let mapping = visible.map(|reading| {
            if domain.swapchain != key.swapchain
                || matches!(domain.clock, Clock::StageLocal(stage) if stage != PresentStage::FirstPixelVisible) {
                return Err(ClockError::DomainMismatch);
            }
            correlation.ok_or(ClockError::DomainMismatch)?.to_host(domain, reading, now)
        });
        self.finish(key, mapping, max_uncertainty).map(Some)
    }

    /// Adapts `VkPastPresentationTimingGOOGLE::actualPresentTime`.
    ///
    /// The host supplies the implementation's nanosecond domain and a justified
    /// correlation to its CPU clock, even when both happen to be monotonic.
    ///
    /// # Errors
    /// Returns [`CorrelationError::UnknownPresent`] for unmatched feedback.
    pub fn resolve_google(
        &mut self,
        key: PresentKey,
        domain: TimeDomain,
        actual_present: u64,
        correlation: Option<ClockCorrelation>,
        now: HostTime,
        max_uncertainty: Duration,
    ) -> Result<ResolvedFrame, CorrelationError> {
        if !matches!(key.id, PresentId::Google(_)) {
            return Err(CorrelationError::WrongProtocol);
        }
        let sample = (actual_present != 0).then(|| {
            if domain.swapchain != key.swapchain {
                return Err(ClockError::DomainMismatch);
            }
            correlation
                .ok_or(ClockError::DomainMismatch)?
                .to_host(domain, actual_present, now)
        });
        self.finish(key, sample, max_uncertainty)
    }

    fn finish(
        &mut self,
        key: PresentKey,
        mapped: Option<Result<MappedTime, ClockError>>,
        max_uncertainty: Duration,
    ) -> Result<ResolvedFrame, CorrelationError> {
        let index = self
            .pending
            .iter()
            .position(|(pending, _)| *pending == key)
            .ok_or(CorrelationError::UnknownPresent)?;
        let (_, frame_index) = self.pending.swap_remove(index);
        let (outcome, clock_error) = match mapped {
            Some(Ok(time)) if time.uncertainty <= max_uncertainty => {
                (PresentOutcome::Presented(time.time), None)
            }
            Some(Ok(_)) => (PresentOutcome::Unknown, Some(ClockError::TooUncertain)),
            Some(Err(error)) => (PresentOutcome::Unknown, Some(error)),
            None => (PresentOutcome::Unknown, None),
        };
        Ok(ResolvedFrame {
            frame_index,
            report: FrameReport {
                work: None,
                outcome,
            },
            clock_error,
        })
    }

    /// Removes a retired swapchain's unresolved frames without inventing fate.
    #[must_use]
    pub fn abandon_swapchain(&mut self, swapchain: SwapchainId) -> Vec<u64> {
        let mut frames = Vec::new();
        self.pending.retain(|(key, frame)| {
            if key.swapchain == swapchain {
                frames.push(*frame);
                false
            } else {
                true
            }
        });
        frames
    }
}

#[cfg(test)]
mod tests;
