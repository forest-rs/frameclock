// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `CVDisplayLink` integration for predictive frame timing.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::cell::{RefCell, UnsafeCell};
use core::ffi::c_void;
use core::fmt;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dispatch2::{DispatchQueue, MainThreadBound};
use frameclock::time::Timebase;
use frameclock::{Duration, FrameTick, HostTime, OutputId};
use objc2::MainThreadMarker;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVDisplayLink as CVDisplayLinkRaw, CVTimeStamp, CVTimeStampFlags, kCVReturnSuccess,
};

use crate::AppleFeedbackMode;

/// Errors from [`CvDisplayLink`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CvDisplayLinkError {
    /// `CVDisplayLinkCreateWithActiveCGDisplays` failed.
    CreateFailed(i32),
    /// `CVDisplayLinkSetOutputCallback` failed.
    CallbackFailed(i32),
    /// `CVDisplayLinkStart` failed.
    StartFailed(i32),
    /// `CVDisplayLinkStop` failed.
    StopFailed(i32),
}

impl fmt::Display for CvDisplayLinkError {
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

impl core::error::Error for CvDisplayLinkError {}

type HostCallback = RefCell<Box<dyn FnMut(FrameTick)>>;

/// What the display-link thread shares with the main thread.
struct Shared {
    /// The host callback, only ever borrowed on the main thread.
    callback: MainThreadBound<HostCallback>,
    /// Which queued tick may still run.
    gate: TickGate,
}

/// Lets only the newest tick queued for the main thread run.
///
/// It holds the number of the newest tick dispatched, or of the last stop,
/// so a tick that a newer one overtook, or one queued before the link
/// stopped, finds its number stale.
#[derive(Default)]
struct TickGate(AtomicU64);

impl TickGate {
    /// Numbers a tick about to be dispatched, making older ones stale.
    fn dispatch(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Whether the tick with this number is still the newest one.
    fn is_current(&self, number: u64) -> bool {
        self.0.load(Ordering::Relaxed) == number
    }

    /// Makes every tick already dispatched stale.
    fn invalidate(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A callback that runs on the display link's own thread.
struct OnLinkThread {
    callback: UnsafeCell<Box<dyn FnMut(FrameTick) + Send>>,
    /// Whether a tick is running the callback, so no other one does.
    running: AtomicBool,
    /// Set when the link is dropped, so no tick runs the callback after.
    closed: AtomicBool,
}

impl OnLinkThread {
    fn run(&self, tick: FrameTick) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.closed.load(Ordering::SeqCst) {
            // SAFETY: `running` was false and is now true, so this is the
            // only access until it is cleared below.
            let callback = unsafe { &mut *self.callback.get() };
            callback(tick);
        }
        self.running.store(false, Ordering::SeqCst);
    }

    /// Stops ticks from running the callback, waiting for one that is.
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        while self.running.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
    }
}

/// Where the callback runs.
enum Delivery {
    /// On the main thread, from the main dispatch queue.
    MainThread(Arc<Shared>),
    /// On the display link's own thread.
    LinkThread(OnLinkThread),
}

struct CallbackState {
    delivery: Delivery,
    output: OutputId,
    timebase: Timebase,
}

/// Safe wrapper around `CVDisplayLink` that produces [`FrameTick`] events.
///
/// Core Video calls the display link on its own thread. A link made with
/// [`new`](Self::new) dispatches each tick to the main queue and passes it to
/// the callback there, so the callback can own main-thread state without
/// `Send` or `Sync`. A busy main thread delays that dispatch, so a tick's
/// [`FrameTick::now`] is read when it reaches the main thread, not when
/// Core Video produced it, and a prediction that has passed by then is
/// stale. Ticks that a newer tick overtook while waiting are dropped, and so
/// are ticks still waiting when the link is stopped or dropped.
///
/// A link made with [`on_link_thread`](Self::on_link_thread) runs the
/// callback on Core Video's thread instead, as soon as the tick arrives, for
/// a host that renders there. Nothing waits for the main thread.
pub struct CvDisplayLink {
    raw: CFRetained<CVDisplayLinkRaw>,
    state: Pin<Box<CallbackState>>,
    /// A main-thread callback must be dropped on the main thread, so the
    /// link stays on the thread that made it.
    _main_thread: PhantomData<MainThreadMarker>,
}

impl fmt::Debug for CvDisplayLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CvDisplayLink")
            .field("output", &self.state.output)
            .finish_non_exhaustive()
    }
}

impl CvDisplayLink {
    /// The presentation feedback this display link provides: none, since
    /// its ticks carry no actual-present time.
    pub const FEEDBACK_MODE: AppleFeedbackMode = AppleFeedbackMode::CommitOnly;

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
    /// The link is created but not started. Once it is started, `callback`
    /// receives each tick on the main thread, from the main dispatch queue.
    /// A tick that arrives while the callback is still running, such as from
    /// a nested run loop inside it, is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`CvDisplayLinkError`] if the underlying `CoreVideo` calls fail.
    pub fn new<F>(
        callback: F,
        output: OutputId,
        mtm: MainThreadMarker,
    ) -> Result<Self, CvDisplayLinkError>
    where
        F: FnMut(FrameTick) + 'static,
    {
        let callback: Box<dyn FnMut(FrameTick)> = Box::new(callback);
        Self::with_delivery(
            Delivery::MainThread(Arc::new(Shared {
                callback: MainThreadBound::new(RefCell::new(callback), mtm),
                gate: TickGate::default(),
            })),
            output,
        )
    }

    /// Creates a new display link targeting all active displays, whose
    /// callback runs on Core Video's display-link thread.
    ///
    /// The link is created but not started. Once it is started, `callback`
    /// receives each tick on Core Video's thread as soon as it arrives, with
    /// [`FrameTick::now`] read there. A host that renders on that thread,
    /// owning its driver and renderer there, avoids the wait for the main
    /// queue that [`new`](Self::new)'s ticks take. A tick that arrives while
    /// the callback is still running is dropped, and none runs it once the
    /// link is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`CvDisplayLinkError`] if the underlying `CoreVideo` calls fail.
    pub fn on_link_thread<F>(callback: F, output: OutputId) -> Result<Self, CvDisplayLinkError>
    where
        F: FnMut(FrameTick) + Send + 'static,
    {
        Self::with_delivery(
            Delivery::LinkThread(OnLinkThread {
                callback: UnsafeCell::new(Box::new(callback)),
                running: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
            output,
        )
    }

    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    fn with_delivery(delivery: Delivery, output: OutputId) -> Result<Self, CvDisplayLinkError> {
        let state = Box::pin(CallbackState {
            delivery,
            output,
            timebase: crate::mach_time::timebase(),
        });

        let mut link_ptr: *mut CVDisplayLinkRaw = core::ptr::null_mut();
        let ret = unsafe {
            CVDisplayLinkRaw::create_with_active_cg_displays(NonNull::new_unchecked(&mut link_ptr))
        };
        if ret != kCVReturnSuccess {
            return Err(CvDisplayLinkError::CreateFailed(ret));
        }
        let raw_nn = NonNull::new(link_ptr).ok_or(CvDisplayLinkError::CreateFailed(ret))?;
        let raw = unsafe { CFRetained::from_raw(raw_nn) };

        let state_ptr: *const CallbackState = &*state;
        let ret = unsafe {
            raw.set_output_callback(Some(display_link_callback), state_ptr as *mut c_void)
        };
        if ret != kCVReturnSuccess {
            return Err(CvDisplayLinkError::CallbackFailed(ret));
        }

        Ok(Self {
            raw,
            state,
            _main_thread: PhantomData,
        })
    }

    /// Starts the display link.
    ///
    /// # Errors
    ///
    /// Returns [`CvDisplayLinkError::StartFailed`] if `CoreVideo` reports an error.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn start(&self) -> Result<(), CvDisplayLinkError> {
        let ret = self.raw.start();
        if ret != kCVReturnSuccess {
            return Err(CvDisplayLinkError::StartFailed(ret));
        }
        Ok(())
    }

    /// Stops the display link.
    ///
    /// # Errors
    ///
    /// Returns [`CvDisplayLinkError::StopFailed`] if `CoreVideo` reports an error.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn stop(&self) -> Result<(), CvDisplayLinkError> {
        let ret = self.raw.stop();
        if let Delivery::MainThread(shared) = &self.state.delivery {
            // Ticks already queued for the main thread are from before the
            // stop.
            shared.gate.invalidate();
        }
        if ret != kCVReturnSuccess {
            return Err(CvDisplayLinkError::StopFailed(ret));
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
    /// Returns [`CvDisplayLinkError`] if Core Video reports an error while starting
    /// or stopping the link.
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    pub fn set_paused(&self, paused: bool) -> Result<(), CvDisplayLinkError> {
        let running = self.raw.is_running();
        match (paused, running) {
            (true, true) => self.stop(),
            (false, false) => self.start(),
            _ => Ok(()),
        }
    }
}

impl Drop for CvDisplayLink {
    #[expect(
        deprecated,
        reason = "CVDisplayLink API is deprecated by Apple but still functional"
    )]
    fn drop(&mut self) {
        if self.raw.is_running() {
            let _ = self.raw.stop();
        }
        match &self.state.delivery {
            // Queued ticks keep the callback alive but must not run it.
            Delivery::MainThread(shared) => shared.gate.invalidate(),
            Delivery::LinkThread(on_link_thread) => on_link_thread.close(),
        }
    }
}

unsafe extern "C-unwind" fn display_link_callback(
    _display_link: NonNull<CVDisplayLinkRaw>,
    _in_now: NonNull<CVTimeStamp>,
    in_output_time: NonNull<CVTimeStamp>,
    _flags_in: u64,
    _flags_out: NonNull<u64>,
    user_info: *mut c_void,
) -> i32 {
    let state = unsafe { &*(user_info.cast::<CallbackState>()) };

    let out_ts = unsafe { in_output_time.as_ref() };

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

    let output = state.output;
    let shared = match &state.delivery {
        Delivery::MainThread(shared) => Arc::clone(shared),
        Delivery::LinkThread(on_link_thread) => {
            on_link_thread.run(FrameTick {
                now: crate::mach_time::now(),
                predicted_present,
                refresh_interval,
                output,
                prev_actual_present: None,
            });
            return kCVReturnSuccess;
        }
    };
    let number = shared.gate.dispatch();
    DispatchQueue::main().exec_async(move || {
        if !shared.gate.is_current(number) {
            // A newer tick is queued behind this one, or the link stopped.
            return;
        }
        // SAFETY: Work submitted to the main queue runs on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let Ok(mut callback) = shared.callback.get(mtm).try_borrow_mut() else {
            return;
        };
        // How long the tick waited for the main thread is time the host
        // no longer has, so `now` is read here.
        callback(FrameTick {
            now: crate::mach_time::now(),
            predicted_present,
            refresh_interval,
            output,
            prev_actual_present: None,
        });
    });

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

    fn tick() -> FrameTick {
        FrameTick {
            now: HostTime(1),
            predicted_present: None,
            refresh_interval: None,
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    fn counting(count: &Arc<AtomicU64>) -> OnLinkThread {
        let count = Arc::clone(count);
        OnLinkThread {
            callback: UnsafeCell::new(Box::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            })),
            running: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    #[test]
    fn a_link_thread_tick_runs_the_callback_until_the_link_closes() {
        let count = Arc::new(AtomicU64::new(0));
        let on_link_thread = counting(&count);
        on_link_thread.run(tick());
        on_link_thread.run(tick());
        assert_eq!(count.load(Ordering::Relaxed), 2);

        on_link_thread.close();
        on_link_thread.run(tick());
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_link_thread_tick_skips_a_callback_already_running() {
        let count = Arc::new(AtomicU64::new(0));
        let on_link_thread = counting(&count);
        on_link_thread.running.store(true, Ordering::SeqCst);
        on_link_thread.run(tick());
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn only_the_newest_queued_tick_runs() {
        let gate = TickGate::default();
        let first = gate.dispatch();
        let second = gate.dispatch();
        assert!(!gate.is_current(first));
        assert!(gate.is_current(second));
    }

    #[test]
    fn stopping_cancels_queued_ticks_and_restarting_delivers_again() {
        let gate = TickGate::default();
        let queued = gate.dispatch();
        gate.invalidate();
        assert!(!gate.is_current(queued));

        let after_restart = gate.dispatch();
        assert!(gate.is_current(after_restart));
    }

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
