// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `CAMetalDisplayLink` integration: Metal frames with explicit deadlines.

use alloc::boxed::Box;
use core::cell::{Cell, RefCell};
use core::fmt;
use core::marker::PhantomData;

use frameclock::time::Timebase;
use frameclock::{
    DisplayTiming, Duration, FrameOpportunity, FrameTick, HostTime, OutputId, PresentHints,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, ProtocolObject};
use objc2::{AnyThread, DefinedClass, Message, define_class};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::{
    CACurrentMediaTime, CAFrameRateRange, CAMetalDisplayLink as CAMetalDisplayLinkRaw,
    CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate, CAMetalDrawable, CAMetalLayer,
};

use crate::{AppleFeedbackMode, PreferredFrameRateRange, mach_time, preferred_frame_rate_range};

type UpdateCallback = RefCell<Box<dyn FnMut(MetalTick)>>;

struct DelegateIvars {
    callback: UpdateCallback,
    /// The previous update's target presentation time, in media seconds;
    /// NaN when there is none to measure the refresh interval from.
    last_target_present: Cell<f64>,
    /// How the link's updates have been spaced.
    spacing: Cell<Spacing>,
    output: OutputId,
    timebase: Timebase,
}

define_class! {
    #[unsafe(super(NSObject))]
    #[name = "FrameclockMetalDisplayLinkDelegate"]
    #[ivars = DelegateIvars]
    struct MetalDisplayLinkDelegate;

    unsafe impl NSObjectProtocol for MetalDisplayLinkDelegate {}

    unsafe impl CAMetalDisplayLinkDelegate for MetalDisplayLinkDelegate {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn metal_display_link_needs_update(
            &self,
            _link: &CAMetalDisplayLinkRaw,
            update: &CAMetalDisplayLinkUpdate,
        ) {
            self.handle_update(update);
        }
    }
}

impl MetalDisplayLinkDelegate {
    fn new<F>(callback: F, output: OutputId) -> Retained<Self>
    where
        F: FnMut(MetalTick) + 'static,
    {
        let this = Self::alloc().set_ivars(DelegateIvars {
            callback: RefCell::new(Box::new(callback)),
            last_target_present: Cell::new(f64::NAN),
            spacing: Cell::new(Spacing::default()),
            output,
            timebase: mach_time::timebase(),
        });
        unsafe { objc2::msg_send![super(this), init] }
    }

    fn forget_last_update(&self) {
        self.ivars().last_target_present.set(f64::NAN);
        self.ivars().spacing.set(Spacing::default());
    }

    fn handle_update(&self, update: &CAMetalDisplayLinkUpdate) {
        let ivars = self.ivars();
        let target_present = update.targetPresentationTimestamp();
        let deadline = update.targetTimestamp();

        // One conversion pair from Core Animation media time into Mach host
        // time serves every timestamp in this update.
        let now = mach_time::now();
        let media_now = CACurrentMediaTime();
        let to_host = |media_seconds: f64| {
            mach_time::media_time_to_host_time(media_seconds, now, media_now, ivars.timebase)
        };

        let last = ivars.last_target_present.replace(target_present);
        let mut spacing = ivars.spacing.get();
        let refresh_interval =
            spacing.observe(target_spacing(last, target_present, ivars.timebase));
        ivars.spacing.set(spacing);

        let tick = MetalTick {
            tick: FrameTick {
                now,
                predicted_present: to_host(target_present),
                refresh_interval,
                output: ivars.output,
                prev_actual_present: None,
            },
            commit_deadline: to_host(deadline),
            update: update.retain(),
        };

        if let Ok(mut callback) = ivars.callback.try_borrow_mut() {
            callback(tick);
        }
    }
}

/// One `CAMetalDisplayLink` update: a [`FrameTick`], the time by which the
/// frame must be presented, and the drawable to render it into.
///
/// Build the frame's [`FrameOpportunity`] with
/// [`frame_opportunity`](Self::frame_opportunity), render into
/// [`drawable`](Self::drawable) when the driver says the frame is ready, and
/// call the drawable's `present` before
/// [`commit_deadline`](Self::commit_deadline). Use plain `present`: Core
/// Animation asserts on the timed variants, such as `presentAtTime:`, for a
/// drawable from this link.
pub struct MetalTick {
    tick: FrameTick,
    commit_deadline: Option<HostTime>,
    update: Retained<CAMetalDisplayLinkUpdate>,
}

impl fmt::Debug for MetalTick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MetalTick")
            .field("tick", &self.tick)
            .field("commit_deadline", &self.commit_deadline)
            .finish_non_exhaustive()
    }
}

impl MetalTick {
    /// The frame tick: the callback time, the update's target presentation
    /// time as [`FrameTick::predicted_present`], and the spacing between
    /// consecutive targets as [`FrameTick::refresh_interval`].
    #[must_use]
    pub const fn tick(&self) -> FrameTick {
        self.tick
    }

    /// The time by which the drawable must be presented: the update's
    /// `targetTimestamp`. GPU work may continue after it, for as many frames
    /// as the link's preferred frame latency allows.
    #[must_use]
    pub const fn commit_deadline(&self) -> Option<HostTime> {
        self.commit_deadline
    }

    /// The drawable to render this update's frame into.
    #[must_use]
    pub fn drawable(&self) -> Retained<ProtocolObject<dyn CAMetalDrawable>> {
        self.update.drawable()
    }

    /// The Core Animation update this tick came from.
    #[must_use]
    pub fn update(&self) -> &CAMetalDisplayLinkUpdate {
        &self.update
    }

    /// Present hints from the update's own deadline and target.
    ///
    /// The hints are predictive, commit at the update's deadline rather than
    /// a guessed lead, and [start each frame at the
    /// tick](PresentHints::starting_at_tick), since the drawable arrives with
    /// the callback. The deadline is [hard](PresentHints::with_hard_deadline):
    /// the drawable must be presented by then, so pipeline depth cannot
    /// extend it, and an update that arrives after it expires instead of
    /// starting a frame. A stale target leaves pacing-only hints that keep
    /// the hard deadline; only without a deadline do they fall back to
    /// [`crate::present_hints`] with `fallback_refresh_interval`.
    #[must_use]
    pub fn present_hints(&self, fallback_refresh_interval: Duration) -> PresentHints {
        update_hints(&self.tick, self.commit_deadline, fallback_refresh_interval)
    }

    /// Builds the [`FrameOpportunity`] for this update.
    ///
    /// `fallback_timing` describes the layer's display, as for
    /// [`crate::display_timing`].
    #[must_use]
    pub fn frame_opportunity(&self, fallback_timing: DisplayTiming) -> FrameOpportunity {
        let hints = self.present_hints(fallback_timing.min_interval());
        let display_timing = crate::display_timing(&self.tick, fallback_timing);
        FrameOpportunity::new(self.tick, hints, display_timing)
    }
}

/// The refresh interval the link's updates are spaced at, followed across
/// skipped updates.
///
/// An update carries no interval of its own, so the interval is the spacing
/// of the targets the link asks for. But a link that skips an update, as
/// one does under load, spaces the next one two refreshes after the last:
/// taken as the refresh interval, that would plan the next frame at half
/// the display's rate. A spacing unlike the interval so far is taken only
/// once it repeats, as when a display's rate really changes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Spacing {
    /// The refresh interval the updates have been spaced at.
    interval: Option<Duration>,
    /// The last spacing unlike `interval`, waiting to repeat.
    candidate: Option<Duration>,
}

impl Spacing {
    /// Takes in the spacing of the latest update and returns the refresh
    /// interval.
    fn observe(&mut self, spacing: Option<Duration>) -> Option<Duration> {
        let Some(spacing) = spacing else {
            return self.interval;
        };
        let like = |a: Duration, b: Duration| a.ticks().abs_diff(b.ticks()) * 4 <= b.ticks();
        match self.interval {
            Some(interval) if !like(spacing, interval) => {
                if self
                    .candidate
                    .is_some_and(|candidate| like(spacing, candidate))
                {
                    self.interval = Some(spacing);
                    self.candidate = None;
                } else {
                    self.candidate = Some(spacing);
                }
            }
            _ => {
                self.interval = Some(spacing);
                self.candidate = None;
            }
        }
        self.interval
    }
}

/// The spacing of an update's target from the one before.
fn target_spacing(last_target: f64, target: f64, timebase: Timebase) -> Option<Duration> {
    let spacing = target - last_target;
    (spacing.is_finite() && spacing > 0.0)
        .then(|| Duration(mach_time::seconds_to_ticks(spacing, timebase)))
        .filter(|interval| !interval.is_zero())
}

/// Hints from an update's target and deadline, or the display-link
/// fallback without them.
///
/// The drawable's deadline stays a hard limit even when its target is stale:
/// the target only steers the plan, but the drawable still has to be
/// presented by its deadline.
fn update_hints(
    tick: &FrameTick,
    commit_deadline: Option<HostTime>,
    fallback_refresh_interval: Duration,
) -> PresentHints {
    match (tick.predicted_present, commit_deadline) {
        (Some(present), Some(deadline)) if present >= tick.now => {
            PresentHints::selected_timeline(present, deadline)
        }
        (_, Some(deadline)) => PresentHints::pacing_only(deadline)
            .starting_at_tick()
            .with_hard_deadline(),
        (_, None) => crate::present_hints(tick, fallback_refresh_interval),
    }
}

/// Why a [`CaMetalDisplayLink`] could not be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaMetalDisplayLinkError {
    /// The system has no `CAMetalDisplayLink`: it needs macOS 14, iOS 17,
    /// or later.
    Unavailable,
}

impl fmt::Display for CaMetalDisplayLinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("CAMetalDisplayLink needs macOS 14, iOS 17, or later"),
        }
    }
}

impl core::error::Error for CaMetalDisplayLinkError {}

/// Safe wrapper around `CAMetalDisplayLink`, which hands each update a
/// drawable with its deadline and target presentation time.
///
/// The link calls back on the run loop of the thread that
/// [starts](Self::start) it, which need not be the main thread, so a render
/// thread can own the callback, the renderer, and the
/// [`FrameDriver`](frameclock::FrameDriver). The wrapper is not `Send`: it
/// stays on the thread that created it.
///
/// Its frames are shown after their GPU work, so their fate is reported per
/// frame: submit with [`FEEDBACK_MODE`](Self::FEEDBACK_MODE) and report each
/// drawable's presented time, converted by [`crate::presented_outcome`],
/// through [`FrameDriver::report_frame`](frameclock::FrameDriver::report_frame).
pub struct CaMetalDisplayLink {
    raw: Retained<CAMetalDisplayLinkRaw>,
    delegate: Retained<MetalDisplayLinkDelegate>,
    run_loop: RefCell<Option<Retained<NSRunLoop>>>,
    _not_send: PhantomData<*const ()>,
}

impl fmt::Debug for CaMetalDisplayLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaMetalDisplayLink")
            .field("output", &self.delegate.ivars().output)
            .field("paused", &self.is_paused())
            .finish_non_exhaustive()
    }
}

impl CaMetalDisplayLink {
    /// Presentation feedback for this source: each frame is reported.
    pub const FEEDBACK_MODE: AppleFeedbackMode = AppleFeedbackMode::Reported;

    /// Returns the Mach host [`Timebase`] used for all frame ticks.
    #[must_use]
    pub fn timebase() -> Timebase {
        mach_time::timebase()
    }

    /// Returns the current Mach absolute time as a [`HostTime`].
    #[must_use]
    pub fn now() -> HostTime {
        mach_time::now()
    }

    /// Creates a display link for `layer` that calls `callback` with each
    /// update.
    ///
    /// The link does not tick until [`start`](Self::start).
    ///
    /// # Errors
    ///
    /// Returns [`CaMetalDisplayLinkError::Unavailable`] before macOS 14 or
    /// iOS 17.
    pub fn new<F>(
        layer: &CAMetalLayer,
        callback: F,
        output: OutputId,
    ) -> Result<Self, CaMetalDisplayLinkError>
    where
        F: FnMut(MetalTick) + 'static,
    {
        if AnyClass::get(c"CAMetalDisplayLink").is_none() {
            return Err(CaMetalDisplayLinkError::Unavailable);
        }
        let delegate = MetalDisplayLinkDelegate::new(callback, output);
        let raw = CAMetalDisplayLinkRaw::initWithMetalLayer(CAMetalDisplayLinkRaw::alloc(), layer);
        raw.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Ok(Self {
            raw,
            delegate,
            run_loop: RefCell::new(None),
            _not_send: PhantomData,
        })
    }

    /// Starts the link on the current thread's run loop, in the common
    /// run-loop modes.
    ///
    /// Updates arrive on this thread. Starting an already started link does
    /// nothing.
    pub fn start(&self) {
        let mut run_loop = self.run_loop.borrow_mut();
        if run_loop.is_some() {
            return;
        }
        let current = NSRunLoop::currentRunLoop();
        unsafe {
            self.raw
                .addToRunLoop_forMode(&current, NSRunLoopCommonModes);
        }
        *run_loop = Some(current);
    }

    /// Stops the link by removing it from the run loop it was started on.
    ///
    /// The first update after the link starts again reports no
    /// [`FrameTick::refresh_interval`].
    pub fn stop(&self) {
        if let Some(run_loop) = self.run_loop.borrow_mut().take() {
            unsafe {
                self.raw
                    .removeFromRunLoop_forMode(&run_loop, NSRunLoopCommonModes);
            }
        }
        self.delegate.forget_last_update();
    }

    /// Returns whether the link is paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.raw.isPaused()
    }

    /// Pauses or resumes updates without removing the link from its run
    /// loop.
    ///
    /// Pause while [`FrameDriver::needs_tick`](frameclock::FrameDriver::needs_tick)
    /// is false. The first update after a resume reports no
    /// [`FrameTick::refresh_interval`].
    pub fn set_paused(&self, paused: bool) {
        if paused {
            self.delegate.forget_last_update();
        }
        self.raw.setPaused(paused);
    }

    /// Returns how many frames ahead of display the link asks for each
    /// frame.
    #[must_use]
    pub fn preferred_frame_latency(&self) -> f32 {
        self.raw.preferredFrameLatency()
    }

    /// Sets how many frames ahead of display the link asks for each frame.
    ///
    /// More latency gives the GPU more time per frame and shows input later.
    /// The system may use more than requested, such as for a window on
    /// macOS.
    pub fn set_preferred_frame_latency(&self, frames: f32) {
        self.raw.setPreferredFrameLatency(frames);
    }

    /// Sets the Core Animation preferred frame-rate range.
    pub fn set_preferred_frame_rate_range(&self, range: PreferredFrameRateRange) {
        self.raw.setPreferredFrameRateRange(CAFrameRateRange::new(
            range.minimum,
            range.maximum,
            range.preferred,
        ));
    }

    /// Computes and applies a preferred frame-rate range for
    /// `frame_interval`.
    ///
    /// Returns `None` when the interval cannot be represented as a finite
    /// frames-per-second value.
    pub fn set_preferred_frame_interval(
        &self,
        frame_interval: Duration,
        display_timing: DisplayTiming,
    ) -> Option<PreferredFrameRateRange> {
        let range =
            preferred_frame_rate_range(frame_interval, display_timing, mach_time::timebase())?;
        self.set_preferred_frame_rate_range(range);
        Some(range)
    }
}

impl Drop for CaMetalDisplayLink {
    fn drop(&mut self) {
        // Removes the link from every run loop and releases its delegate.
        self.raw.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frameclock::timing::PresentationTiming;

    const TB: Timebase = Timebase::NANOS;

    fn tick(predicted_present: Option<HostTime>) -> FrameTick {
        FrameTick {
            now: HostTime(1_000_000),
            predicted_present,
            refresh_interval: Some(Duration(8_333_333)),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    #[test]
    fn hints_commit_at_the_update_deadline_and_start_at_the_tick() {
        let hints = update_hints(
            &tick(Some(HostTime(41_000_000))),
            Some(HostTime(9_000_000)),
            Duration(8_333_333),
        );
        assert_eq!(hints.presentation_timing(), PresentationTiming::Predictive);
        assert_eq!(hints.desired_present(), Some(HostTime(41_000_000)));
        assert_eq!(hints.latest_commit(), HostTime(9_000_000));
        assert!(hints.starts_at_tick());
        assert!(hints.has_hard_deadline());
    }

    #[test]
    fn hints_without_a_deadline_fall_back() {
        let fallback = Duration(8_333_333);
        let fresh = tick(Some(HostTime(41_000_000)));
        assert_eq!(
            update_hints(&fresh, None, fallback),
            crate::present_hints(&fresh, fallback)
        );
    }

    #[test]
    fn hints_with_a_stale_target_keep_the_native_deadline() {
        let stale = tick(Some(HostTime(500_000)));
        let hints = update_hints(&stale, Some(HostTime(9_000_000)), Duration(8_333_333));
        assert_eq!(hints.presentation_timing(), PresentationTiming::PacingOnly);
        assert_eq!(hints.desired_present(), None);
        assert_eq!(hints.latest_commit(), HostTime(9_000_000));
        assert!(hints.has_hard_deadline());
        assert!(hints.starts_at_tick());
    }

    #[test]
    fn an_update_with_a_stale_target_after_its_native_deadline_is_not_ready() {
        let tick = FrameTick {
            now: HostTime(42_000_000),
            ..tick(Some(HostTime(41_000_000)))
        };
        let opportunity = FrameOpportunity::new(
            tick,
            update_hints(&tick, Some(HostTime(9_000_000)), Duration(8_333_333)),
            DisplayTiming::fixed(Duration(8_333_333)),
        );
        let mut driver = frameclock::FrameDriver::new(frameclock::SchedulerConfig::predictive(TB));
        driver.request(frameclock::FrameDemand::INPUT);
        let result = driver.begin_frame(opportunity).result;
        assert!(
            matches!(result, frameclock::FrameBeginResult::Expired(_)),
            "native deadline has passed: {result:?}"
        );
    }

    #[test]
    fn a_skipped_update_does_not_change_the_refresh_interval() {
        let refresh = Duration(8_333_333);
        let mut spacing = Spacing::default();
        assert_eq!(spacing.observe(Some(refresh)), Some(refresh));
        assert_eq!(
            spacing.observe(Some(Duration(8_340_000))),
            Some(Duration(8_340_000))
        );
        // One update skipped: the next comes two refreshes later.
        assert_eq!(
            spacing.observe(Some(Duration(16_680_000))),
            Some(Duration(8_340_000))
        );
        assert_eq!(spacing.observe(Some(refresh)), Some(refresh));
        // No spacing to measure keeps the interval.
        assert_eq!(spacing.observe(None), Some(refresh));
        // The display really slows down: the new spacing repeats.
        let slower = Duration(16_666_667);
        assert_eq!(spacing.observe(Some(slower)), Some(refresh));
        assert_eq!(spacing.observe(Some(slower)), Some(slower));
    }

    #[test]
    fn refresh_interval_is_the_spacing_of_targets() {
        assert_eq!(
            target_spacing(1.0, 1.0 + 1.0 / 120.0, TB),
            Some(Duration(8_333_333))
        );
        assert_eq!(target_spacing(f64::NAN, 1.0, TB), None);
        assert_eq!(target_spacing(1.0, 1.0, TB), None);
        assert_eq!(target_spacing(1.0, 0.5, TB), None);
    }

    #[test]
    fn pipeline_depth_must_not_extend_the_current_drawables_native_deadline() {
        let tick = tick(Some(HostTime(41_000_000)));
        let native_deadline = HostTime(9_000_000);
        let opportunity = FrameOpportunity::new(
            tick,
            update_hints(&tick, Some(native_deadline), Duration(8_333_333)),
            DisplayTiming::fixed(Duration(8_333_333)),
        );
        let mut config = frameclock::SchedulerConfig::predictive(TB);
        config.initial_depth = 2;
        let mut scheduler = frameclock::scheduler::Scheduler::new(config);
        let plan = scheduler.plan(opportunity, frameclock::FrameDemand::ANIMATION, 0);
        assert!(
            plan.commit_deadline <= native_deadline,
            "native deadline {native_deadline:?}, plan deadline {:?}",
            plan.commit_deadline
        );
    }

    #[test]
    fn an_update_delivered_after_its_native_deadline_is_not_ready() {
        let tick = FrameTick {
            now: HostTime(10_000_000),
            ..tick(Some(HostTime(41_000_000)))
        };
        let opportunity = FrameOpportunity::new(
            tick,
            update_hints(&tick, Some(HostTime(9_000_000)), Duration(8_333_333)),
            DisplayTiming::fixed(Duration(8_333_333)),
        );
        let mut driver = frameclock::FrameDriver::new(frameclock::SchedulerConfig::predictive(TB));
        driver.request(frameclock::FrameDemand::INPUT);
        let result = driver.begin_frame(opportunity).result;
        assert!(
            !matches!(result, frameclock::FrameBeginResult::Ready(_)),
            "native deadline has passed: {result:?}"
        );
    }
}
