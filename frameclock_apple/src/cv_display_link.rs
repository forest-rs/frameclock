// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `CVDisplayLink` integration for predictive frame timing.

use alloc::boxed::Box;
use core::ffi::c_void;
use core::fmt;
use core::pin::Pin;
use core::ptr::NonNull;

use frameclock::time::Timebase;
use frameclock::{Duration, FrameTick, HostTime, OutputId};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVDisplayLink as CVDisplayLinkRaw, CVTimeStamp, CVTimeStampFlags, kCVReturnSuccess,
};

use crate::threading::TickSender;

/// Errors from [`DisplayLink`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayLinkError {
    /// `CVDisplayLinkCreateWithActiveCGDisplays` failed.
    CreateFailed(i32),
    /// `CVDisplayLinkSetOutputCallback` failed.
    CallbackFailed(i32),
    /// `CVDisplayLinkStart` failed.
    StartFailed(i32),
    /// `CVDisplayLinkStop` failed.
    StopFailed(i32),
}

impl fmt::Display for DisplayLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateFailed(code) => write!(f, "CVDisplayLink creation failed ({code})"),
            Self::CallbackFailed(code) => {
                write!(f, "CVDisplayLink set callback failed ({code})")
            }
            Self::StartFailed(code) => write!(f, "CVDisplayLink start failed ({code})"),
            Self::StopFailed(code) => write!(f, "CVDisplayLink stop failed ({code})"),
        }
    }
}

impl core::error::Error for DisplayLinkError {}

struct CallbackState {
    sender: TickSender,
    output: OutputId,
    timebase: Timebase,
}

/// Safe wrapper around `CVDisplayLink` that produces [`FrameTick`] events.
pub struct DisplayLink {
    raw: CFRetained<CVDisplayLinkRaw>,
    _state: Pin<Box<CallbackState>>,
}

impl fmt::Debug for DisplayLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplayLink")
            .field("output", &self._state.output)
            .finish_non_exhaustive()
    }
}

impl DisplayLink {
    /// Returns the Mach absolute time timebase.
    #[must_use]
    pub fn timebase() -> Timebase {
        crate::mach_time::timebase()
    }

    /// Returns the current Mach absolute time as a [`HostTime`].
    #[must_use]
    pub fn now() -> HostTime {
        crate::mach_time::now()
    }

    /// Creates a new display link targeting all active displays.
    ///
    /// # Errors
    ///
    /// Returns [`DisplayLinkError`] if the underlying `CoreVideo` calls fail.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn new(sender: TickSender, output: OutputId) -> Result<Self, DisplayLinkError> {
        let state = Box::pin(CallbackState {
            sender,
            output,
            timebase: crate::mach_time::timebase(),
        });

        let mut link_ptr: *mut CVDisplayLinkRaw = core::ptr::null_mut();
        let ret = unsafe {
            CVDisplayLinkRaw::create_with_active_cg_displays(NonNull::new_unchecked(&mut link_ptr))
        };
        if ret != kCVReturnSuccess {
            return Err(DisplayLinkError::CreateFailed(ret));
        }
        let raw_nn = NonNull::new(link_ptr).ok_or(DisplayLinkError::CreateFailed(ret))?;
        let raw = unsafe { CFRetained::from_raw(raw_nn) };

        let state_ptr: *const CallbackState = &*state;
        let ret = unsafe {
            raw.set_output_callback(Some(display_link_callback), state_ptr as *mut c_void)
        };
        if ret != kCVReturnSuccess {
            return Err(DisplayLinkError::CallbackFailed(ret));
        }

        Ok(Self { raw, _state: state })
    }

    /// Starts the display link.
    ///
    /// # Errors
    ///
    /// Returns [`DisplayLinkError::StartFailed`] if `CoreVideo` reports an error.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn start(&self) -> Result<(), DisplayLinkError> {
        let ret = self.raw.start();
        if ret != kCVReturnSuccess {
            return Err(DisplayLinkError::StartFailed(ret));
        }
        Ok(())
    }

    /// Stops the display link.
    ///
    /// # Errors
    ///
    /// Returns [`DisplayLinkError::StopFailed`] if `CoreVideo` reports an error.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn stop(&self) -> Result<(), DisplayLinkError> {
        let ret = self.raw.stop();
        if ret != kCVReturnSuccess {
            return Err(DisplayLinkError::StopFailed(ret));
        }
        Ok(())
    }

    /// Returns whether the display link is currently paused.
    ///
    /// `CVDisplayLink` exposes running/stopped state rather than a separate
    /// paused bit, so paused means "not running".
    #[must_use]
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn is_paused(&self) -> bool {
        !self.raw.is_running()
    }

    /// Pauses or resumes the display link.
    ///
    /// `CVDisplayLink` has no native pause bit, so this maps to stop/start and
    /// is idempotent with respect to the current running state.
    ///
    /// # Errors
    ///
    /// Returns [`DisplayLinkError`] if Core Video reports an error while starting
    /// or stopping the link.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn set_paused(&self, paused: bool) -> Result<(), DisplayLinkError> {
        let running = self.raw.is_running();
        match (paused, running) {
            (true, true) => self.stop(),
            (false, false) => self.start(),
            _ => Ok(()),
        }
    }
}

impl Drop for DisplayLink {
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    fn drop(&mut self) {
        if self.raw.is_running() {
            let _ = self.raw.stop();
        }
    }
}

unsafe extern "C-unwind" fn display_link_callback(
    _display_link: NonNull<CVDisplayLinkRaw>,
    in_now: NonNull<CVTimeStamp>,
    in_output_time: NonNull<CVTimeStamp>,
    _flags_in: u64,
    _flags_out: NonNull<u64>,
    user_info: *mut c_void,
) -> i32 {
    let state = unsafe { &*(user_info.cast::<CallbackState>()) };

    let now_ts = unsafe { in_now.as_ref() };
    let out_ts = unsafe { in_output_time.as_ref() };

    let now = HostTime(now_ts.hostTime);
    let predicted_present = has_flag(out_ts.flags, CVTimeStampFlags::HostTimeValid)
        .then_some(HostTime(out_ts.hostTime));
    // The output time is when the next frame is shown, not one refresh
    // after `now`: their difference is output latency, so the refresh
    // interval comes from the display's own refresh period.
    let refresh_interval = refresh_interval(
        out_ts.flags,
        out_ts.videoRefreshPeriod,
        out_ts.videoTimeScale,
        state.timebase,
    );

    let tick = FrameTick {
        now,
        predicted_present,
        refresh_interval,
        output: state.output,
        prev_actual_present: None,
    };

    state.sender.send(tick);

    kCVReturnSuccess
}

const fn has_flag(flags: u64, flag: CVTimeStampFlags) -> bool {
    flags & flag.0 == flag.0
}

/// Converts a `CVTimeStamp` refresh period, in `time_scale` units per
/// second, to host ticks of `timebase`.
///
/// Returns `None` unless the timestamp marks the period valid and both
/// terms are positive.
fn refresh_interval(
    flags: u64,
    period: i64,
    time_scale: i32,
    timebase: Timebase,
) -> Option<Duration> {
    if !has_flag(flags, CVTimeStampFlags::VideoRefreshPeriodValid) {
        return None;
    }
    let period = u128::try_from(period).ok().filter(|period| *period > 0)?;
    let time_scale = u128::try_from(time_scale).ok().filter(|scale| *scale > 0)?;
    // ticks = seconds * 1e9 * denom / numer, in one step so nothing rounds
    // through nanoseconds.
    let ticks = period * 1_000_000_000 * u128::from(timebase.denom())
        / (time_scale * u128::from(timebase.numer()));
    let ticks = u64::try_from(ticks).unwrap_or(u64::MAX);
    (ticks > 0).then_some(Duration(ticks))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFRESH_VALID: u64 = CVTimeStampFlags::VideoRefreshPeriodValid.0;

    #[test]
    fn refresh_interval_comes_from_the_refresh_period() {
        // 120 Hz in Mach ticks of a 24 MHz timebase.
        let mach = Timebase::new(125, 3);
        assert_eq!(
            refresh_interval(REFRESH_VALID, 1, 120, mach),
            Some(Duration(200_000))
        );
        assert_eq!(
            refresh_interval(REFRESH_VALID, 200_000, 24_000_000, mach),
            Some(Duration(200_000))
        );
        // 60 Hz on a nanosecond timebase.
        assert_eq!(
            refresh_interval(REFRESH_VALID, 1, 60, Timebase::NANOS),
            Some(Duration(16_666_666))
        );
    }

    #[test]
    fn refresh_interval_requires_a_valid_positive_period() {
        let tb = Timebase::NANOS;
        assert_eq!(refresh_interval(0, 1, 60, tb), None);
        assert_eq!(refresh_interval(REFRESH_VALID, 0, 60, tb), None);
        assert_eq!(refresh_interval(REFRESH_VALID, -1, 60, tb), None);
        assert_eq!(refresh_interval(REFRESH_VALID, 1, 0, tb), None);
    }
}
