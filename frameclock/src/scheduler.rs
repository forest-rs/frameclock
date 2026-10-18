// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Frame scheduling with adaptive pipeline depth and safety margins.
//!
//! The [`Scheduler`] converts a [`FrameOpportunity`] and [`FrameDemand`] into
//! a [`FramePlan`], adapting its pipeline depth and safety margin based on
//! observed [`PresentFeedback`]. See the [`Scheduler`] struct docs for
//! details on pipeline depth and adaptive behavior.

use crate::demand::{FrameDemand, FrameDemandClass};
use crate::time::{Duration, HostTime, Timebase};
use crate::timing::{
    DisplayTiming, FrameOpportunity, FramePlan, PresentFeedback, PresentationTiming,
};

/// Controls how the scheduler adapts pipeline depth in response to deadline
/// misses and hits.
///
/// Passed to the [`Scheduler`] via [`SchedulerConfig::degradation_policy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DegradationPolicy {
    /// Adapt pipeline depth automatically.
    ///
    /// Increases depth by 1 when `miss_threshold` of the last `miss_window`
    /// frames missed, so misses that come every other frame count as surely
    /// as misses in a row. Decreases depth by 1 after `recovery_threshold`
    /// consecutive hits, and only while each frame planned meanwhile would
    /// also have had time for its work one depth shallower. Bounded by
    /// [`SchedulerConfig::min_depth`] and [`SchedulerConfig::max_depth`].
    ///
    /// Only frames planned at the current depth count: frames already in
    /// flight when it changed say nothing about the new depth. Pacing
    /// overruns, the weaker signal, take twice `miss_threshold`.
    Adaptive {
        /// How many misses among the last `miss_window` frames increase
        /// depth.
        miss_threshold: u32,
        /// How many of the most recent frames misses are counted over, at
        /// most 64.
        miss_window: u32,
        /// Number of consecutive hits before decreasing depth.
        recovery_threshold: u32,
    },
    /// Keep pipeline depth fixed at [`SchedulerConfig::initial_depth`].
    ///
    /// Build cost EMA and safety margin are still tracked, but depth
    /// never changes.
    Fixed,
}

/// Configuration passed to [`Scheduler::new`] or
/// [`FrameDriver::new`](crate::FrameDriver::new).
#[derive(Clone, Copy, Debug)]
pub struct SchedulerConfig {
    /// Initial pipeline depth.
    ///
    /// A value of 1 means no extra whole-frame lookahead. Higher values shift
    /// non-input plans further into the future by whole selected frame
    /// intervals.
    pub initial_depth: u8,
    /// Minimum pipeline depth.
    pub min_depth: u8,
    /// Maximum pipeline depth.
    pub max_depth: u8,
    /// EMA smoothing factor for build cost estimation (0.0–1.0).
    /// Smaller values = more smoothing.
    pub ema_alpha: f64,
    /// Safety margin multiplier applied to the EMA build cost.
    pub safety_multiplier: f64,
    /// Minimum margin subtracted from the commit deadline to compute
    /// [`FramePlan::frame_start`], in host ticks.
    ///
    /// The scheduler uses the larger of this value and the learned safety
    /// margin. This gives first-frame scheduling a conservative work window
    /// before feedback has trained the build-cost estimate.
    pub minimum_frame_start_margin: Duration,
    /// Nominal latency, in host ticks, used for pacing-only mode when no
    /// predicted present time is available.
    pub nominal_latency: Duration,
    /// Policy for adapting pipeline depth.
    pub degradation_policy: DegradationPolicy,
    /// How long a slower cadence is kept before the scheduler speeds frames
    /// back up, in host ticks.
    ///
    /// The cadence speeds up only once the learned build cost fits the
    /// faster interval with a sixteenth of a refresh to spare and the
    /// slower one has been held this long. Animation and background frames,
    /// planned with slack to spare, slow down only once the work needs a
    /// longer interval by that much; continuous-input frames, planned with
    /// none, slow down as soon as it does. A build cost near a boundary
    /// would otherwise switch the cadence back and forth, and each switch is
    /// itself a hitch.
    pub cadence_hold: Duration,
    /// How many frames in a row must be shown more than half a refresh
    /// later than the lowest latency seen before the scheduler holds the
    /// next frame back a refresh, to drain a frame queued in the
    /// presentation pipeline; `0` turns it off.
    ///
    /// A present that misses the compositor's cutoff waits a refresh, and a
    /// host that presents one frame a refresh keeps that frame queued, so
    /// every later frame is shown a refresh later too, for as long as the
    /// host keeps up. Nothing reports it: the frames are steady, only late.
    /// Holding one frame back leaves the queue a refresh to empty. It costs
    /// that frame a refresh and every later one wins a refresh back.
    ///
    /// Latency here is from when a frame's build began to when it was shown,
    /// or, for a frame that starts when planned, from its commit deadline:
    /// such a frame starts as late as its learned margin allows, which moves
    /// with its build cost. It comes from feedback that carries each frame's
    /// [`PresentFeedback::actual_present`], for frames committed by their
    /// deadlines. The lowest latency seen is the lowest median of three
    /// frames in a row, so one fast frame does not lower it. Input frames
    /// are not held back: one is answering input, and the frame after it
    /// is held instead. If holding a frame back does not
    /// bring the latency down, the pipeline has changed, such as a window
    /// moved to another display, and the latency it has now becomes the
    /// lowest, so a change costs at most three held frames: one does not
    /// always drain the queue.
    ///
    /// [`PresentFeedback::actual_present`]: crate::timing::PresentFeedback::actual_present
    pub queue_drain_after: u32,
    /// EMA smoothing factor for how late frames are actually shown
    /// (0.0-1.0); `0.0` turns the compensation off.
    ///
    /// A host whose frames reach the screen some refreshes after the
    /// predicted present — a renderer with GPU work and a compositor
    /// behind its submission — only learns this from feedback that
    /// carries each frame's own [`PresentFeedback::actual_present`]. The
    /// scheduler then plans, and samples, for when a frame will really be
    /// seen: [`FramePlan::target_present`] moves by whole refresh
    /// intervals, and the frame rate does not change.
    ///
    /// Only frames committed by their deadline teach the estimate: a frame
    /// whose own build ran late is late for that reason alone. The estimate
    /// starts at zero, so one late frame moves it by this factor of its
    /// lateness rather than setting it.
    ///
    /// [`PresentFeedback::actual_present`]: crate::timing::PresentFeedback::actual_present
    pub present_latency_alpha: f64,
    /// The host clock's timebase: every [`HostTime`] and [`Duration`] the
    /// scheduler is given or returns is in its ticks.
    ///
    /// The constructors set it. Debug builds use it to catch durations
    /// written in nanoseconds for a host whose ticks are not nanoseconds.
    pub timebase: Timebase,
}

/// The default [`SchedulerConfig::minimum_frame_start_margin`]: 1 ms.
const DEFAULT_FRAME_START_MARGIN_NANOS: u64 = 1_000_000;

/// The default pacing-only [`SchedulerConfig::nominal_latency`]: 16 ms.
const DEFAULT_PACING_LATENCY_NANOS: u64 = 16_000_000;

/// The default [`SchedulerConfig::cadence_hold`]: half a second.
const DEFAULT_CADENCE_HOLD_NANOS: u64 = 500_000_000;

/// The default [`SchedulerConfig::degradation_policy`]: deeper after 3
/// misses in 30 frames, about one in ten, and shallower after 60 hits in a
/// row, about a second at 60 Hz. A deeper pipeline costs latency, but the
/// misses that made it deeper cost visible hitches, so it comes back more
/// slowly than it went.
const DEFAULT_DEGRADATION_POLICY: DegradationPolicy = DegradationPolicy::Adaptive {
    miss_threshold: 3,
    miss_window: 30,
    recovery_threshold: 60,
};

/// The default [`SchedulerConfig::queue_drain_after`]: a quarter second at
/// 120 Hz.
const DEFAULT_QUEUE_DRAIN_AFTER: u32 = 30;

/// The most frames [`DegradationPolicy::Adaptive`] counts misses over.
const MAX_MISS_WINDOW: u32 = 64;

/// The start of a frame that may only start at a tick.
///
/// A frame due at the next refresh starts at this tick. One the cadence
/// holds back by whole refreshes starts half a refresh before the tick that
/// precedes its refresh, so that tick releases it despite delivery jitter
/// and the ticks in between do not.
fn tick_aligned_start(
    now: HostTime,
    frame_interval: Duration,
    source_interval: Duration,
) -> HostTime {
    let held_back = frame_interval.saturating_sub(source_interval);
    if held_back.is_zero() {
        return now;
    }
    now.checked_add(held_back.saturating_sub(source_interval.div_u64(2)))
        .unwrap_or(now)
}

/// The slowest fastest-refresh interval a real display has: 4 Hz. Display
/// timing slower than this is almost certainly nanoseconds passed as ticks.
const MAX_PLAUSIBLE_REFRESH_NANOS: u64 = 250_000_000;

impl SchedulerConfig {
    /// Default configuration for predictive timing.
    ///
    /// Use this when a platform exposes a strong predicted presentation time.
    /// `timebase` is the host clock's, such as the platform adapter's
    /// `timebase()`. The default durations are converted to its ticks, so they
    /// are the same length on every platform.
    #[must_use]
    pub const fn predictive(timebase: Timebase) -> Self {
        Self {
            initial_depth: 1,
            min_depth: 1,
            max_depth: 3,
            ema_alpha: 0.2,
            safety_multiplier: 1.5,
            minimum_frame_start_margin: Duration::from_nanos(
                DEFAULT_FRAME_START_MARGIN_NANOS,
                timebase,
            ),
            nominal_latency: Duration(0),
            degradation_policy: DEFAULT_DEGRADATION_POLICY,
            cadence_hold: Duration::from_nanos(DEFAULT_CADENCE_HOLD_NANOS, timebase),
            queue_drain_after: DEFAULT_QUEUE_DRAIN_AFTER,
            present_latency_alpha: 0.1,
            timebase,
        }
    }

    /// Default configuration for estimated timing.
    ///
    /// Use this when a platform exposes useful but less reliable predicted
    /// presentation timing. `timebase` is interpreted as by
    /// [`predictive`](Self::predictive).
    #[must_use]
    pub const fn estimated(timebase: Timebase) -> Self {
        Self {
            initial_depth: 1,
            min_depth: 1,
            max_depth: 3,
            ema_alpha: 0.2,
            safety_multiplier: 2.0,
            minimum_frame_start_margin: Duration::from_nanos(
                DEFAULT_FRAME_START_MARGIN_NANOS,
                timebase,
            ),
            nominal_latency: Duration(0),
            degradation_policy: DEFAULT_DEGRADATION_POLICY,
            cadence_hold: Duration::from_nanos(DEFAULT_CADENCE_HOLD_NANOS, timebase),
            queue_drain_after: DEFAULT_QUEUE_DRAIN_AFTER,
            present_latency_alpha: 0.1,
            timebase,
        }
    }

    /// Default configuration for pacing-only timing.
    ///
    /// Use this when a platform exposes frame pacing but no reliable target
    /// presentation time. `timebase` is interpreted as by
    /// [`predictive`](Self::predictive).
    #[must_use]
    pub const fn pacing_only(timebase: Timebase) -> Self {
        Self {
            initial_depth: 1,
            min_depth: 1,
            max_depth: 3,
            ema_alpha: 0.15,
            safety_multiplier: 2.0,
            minimum_frame_start_margin: Duration::from_nanos(
                DEFAULT_FRAME_START_MARGIN_NANOS,
                timebase,
            ),
            nominal_latency: Duration::from_nanos(DEFAULT_PACING_LATENCY_NANOS, timebase),
            degradation_policy: DEFAULT_DEGRADATION_POLICY,
            cadence_hold: Duration::from_nanos(DEFAULT_CADENCE_HOLD_NANOS, timebase),
            // Without an actual-present timestamp there is nothing to
            // learn how late a frame was, or to drain.
            queue_drain_after: 0,
            present_latency_alpha: 0.0,
            timebase,
        }
    }
}

/// Snapshot of scheduler adaptation state for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SchedulerState {
    /// Current pipeline depth.
    pub pipeline_depth: u8,
    /// How late frames are being shown, in host-time ticks: what
    /// [`SchedulerConfig::present_latency_alpha`] learned.
    pub present_latency_ticks: u64,
    /// Current estimated safety margin in host-time ticks.
    pub safety_margin_ticks: u64,
    /// Misses and pacing overruns among the frames counted toward
    /// [`DegradationPolicy::Adaptive`]'s `miss_window`: the most recent
    /// frames planned at the current depth.
    pub recent_misses: u32,
    /// Consecutive strong hits currently accumulated.
    pub consecutive_hits: u32,
    /// How much earlier than its margins a queued frame now starts, in
    /// host-time ticks, because the host wakes late for it: what
    /// [`Scheduler::observe_wake`] learned.
    pub wake_lateness_ticks: u64,
    /// How many times the scheduler has held a frame back a refresh to drain
    /// a frame queued in the presentation pipeline; see
    /// [`SchedulerConfig::queue_drain_after`].
    pub queue_drains: u64,
}

/// The most refresh intervals [`SchedulerConfig::present_latency_alpha`]
/// will plan ahead for, however late frames are shown.
const MAX_LATENCY_INTERVALS: u64 = 8;

/// Which of the most recent frames missed: bit 0 is the latest.
#[derive(Clone, Copy, Debug, Default)]
struct MissWindow {
    missed: u64,
}

impl MissWindow {
    fn push(&mut self, missed: bool, window: u32) {
        let mask = if window >= u64::BITS {
            u64::MAX
        } else {
            (1_u64 << window) - 1
        };
        self.missed = ((self.missed << 1) | u64::from(missed)) & mask;
    }

    const fn misses(self) -> u32 {
        self.missed.count_ones()
    }
}

/// The cadence animation and background frames hold to; see
/// [`SchedulerConfig::cadence_hold`].
#[derive(Clone, Copy, Debug)]
struct HeldCadence {
    class: FrameDemandClass,
    /// The fastest interval on offer when it was chosen.
    fastest: Duration,
    interval: Duration,
    /// When it slowed down to this cadence, or `None` for a cadence chosen
    /// afresh or sped up to, which nothing holds.
    slowed_at: Option<HostTime>,
}

impl HeldCadence {
    /// The held interval at the fastest interval now on offer, or `None`
    /// when the display's rate has changed and the cadence starts afresh.
    ///
    /// A fixed display's measured interval wanders by a few ticks, and a
    /// cadence held at some refreshes stays at that many.
    fn at(self, fastest: Duration, display: DisplayTiming) -> Option<Duration> {
        let drift = fastest.ticks().abs_diff(self.fastest.ticks());
        if self.fastest.is_zero() || drift > fastest.ticks() / 8 {
            return None;
        }
        let scaled = u128::from(self.interval.ticks()) * u128::from(fastest.ticks())
            / u128::from(self.fastest.ticks());
        let scaled = Duration(u64::try_from(scaled).unwrap_or(u64::MAX));
        // Scaling rounds down, so step back a tick before rounding up to the
        // display's grid: a multiple of the old interval lands on the same
        // multiple of the new one.
        Some(
            display
                .choose_interval(scaled.saturating_sub(Duration(1)))
                .max(fastest),
        )
    }
}

/// How many held frames in a row may fail to bring latency down before the
/// scheduler takes it as the pipeline's latency now. A held frame drains a
/// queued one most of the time, not always.
const DRAIN_ATTEMPTS: u32 = 3;

/// What the scheduler knows about frames queued in the presentation
/// pipeline; see [`SchedulerConfig::queue_drain_after`].
#[derive(Clone, Copy, Debug, Default)]
struct QueueDrain {
    /// The lowest latency seen, in host ticks: how long after its build
    /// began, or its deadline, a frame is shown with nothing queued ahead
    /// of it.
    floor: Option<u64>,
    /// Frames in a row shown more than half a refresh later than the floor.
    late: u32,
    /// The latest three latencies, oldest first: the floor follows their
    /// median, so one outlier either way does not move it.
    recent: [u64; 3],
    /// How many latencies `recent` holds.
    seen: u8,
    /// Whether the next plan holds its frame back a refresh.
    pending: bool,
    /// How many frames have been held back since the floor was last
    /// reached: a held frame does not always drain the queue, but latency
    /// that stays up through [`DRAIN_ATTEMPTS`] means the pipeline changed.
    tried: u32,
    /// The refresh interval of the last plan, in host ticks.
    interval: u64,
    drains: u64,
}

impl QueueDrain {
    /// Learns from one frame shown `latency` ticks after its build began,
    /// and decides whether to drain.
    fn observe(&mut self, latency: u64, after: u32) {
        if self.interval == 0 {
            // Nothing planned yet to say what a refresh is.
            return;
        }
        self.recent = [self.recent[1], self.recent[2], latency];
        self.seen = (self.seen + 1).min(3);
        if self.seen < 3 {
            return;
        }
        let mut sorted = self.recent;
        sorted.sort_unstable();
        let floor = self.floor.map_or(sorted[1], |floor| floor.min(sorted[1]));
        self.floor = Some(floor);
        if latency < floor.saturating_add(self.interval / 2) {
            // Nothing queued, or less than a refresh: the floor holds.
            self.late = 0;
            self.tried = 0;
            return;
        }
        self.late = self.late.saturating_add(1);
        if self.late < after || self.pending {
            return;
        }
        self.late = 0;
        if self.tried >= DRAIN_ATTEMPTS {
            // Holding frames back did not bring it down: the pipeline
            // changed, and this is its latency now.
            self.floor = Some(latency);
            self.tried = 0;
        } else {
            self.pending = true;
            self.tried += 1;
        }
    }
}

/// Exponential moving average tracker.
#[derive(Clone, Copy, Debug)]
struct Ema {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl Ema {
    const fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha,
            initialized: false,
        }
    }

    /// An average that starts from `value`, so no single sample sets it.
    const fn seeded(alpha: f64, value: f64) -> Self {
        Self {
            value,
            alpha,
            initialized: true,
        }
    }

    fn update(&mut self, sample: f64) {
        if self.initialized {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        } else {
            self.value = sample;
            self.initialized = true;
        }
    }

    const fn get(&self) -> f64 {
        self.value
    }

    const fn initialized(&self) -> bool {
        self.initialized
    }
}

fn sanitize_ema_alpha(alpha: f64) -> f64 {
    if !alpha.is_finite() {
        return 1.0;
    }
    alpha.clamp(0.0, 1.0)
}

fn sanitize_safety_multiplier(multiplier: f64) -> f64 {
    if !multiplier.is_finite() || multiplier < 0.0 {
        return 1.0;
    }
    multiplier
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite non-negative tick values are clamped before conversion"
)]
fn f64_ticks_to_u64(ticks: f64) -> u64 {
    if !ticks.is_finite() || ticks <= 0.0 {
        return 0;
    }
    if ticks >= u64::MAX as f64 {
        return u64::MAX;
    }
    ticks as u64
}

/// Low-level planner that converts [`FrameOpportunity`] values into
/// [`FramePlan`]s and adapts over time.
///
/// Custom integrations call [`Scheduler::plan`] when they are ready to make a
/// frame plan, then call [`Scheduler::observe`] with [`PresentFeedback`] after
/// submission. Most host applications should start with
/// [`FrameDriver`](crate::FrameDriver), which owns queued demand, retained
/// frame-start waits, and timing-summary construction around this scheduler.
///
/// # Pipeline depth
///
/// Pipeline depth controls how many selected frame intervals ahead the
/// scheduler plans smooth background work, bounded by
/// [`SchedulerConfig::min_depth`] and [`SchedulerConfig::max_depth`]. A depth
/// of 1 means no extra whole-frame lookahead (lowest latency, highest risk of
/// missing deadlines). A depth of 3 means animation/background plans target
/// two selected intervals beyond the next eligible present slot. One-shot
/// input and continuous input remain latency-first and are not shifted by
/// pipeline depth.
///
/// # Adaptive behavior
///
/// With [`DegradationPolicy::Adaptive`] (set via
/// [`SchedulerConfig::degradation_policy`]), the scheduler increases depth
/// when too many recent frames miss their deadlines (trading latency for
/// safety) and decreases it after sustained hits, once the work would also
/// fit one depth shallower (reclaiming latency). The EMA-smoothed build cost
/// feeds into a safety margin (build cost × multiplier) that backends can
/// query via [`Scheduler::safety_margin_ticks()`]. The
/// [`FramePlan::frame_start`] field applies that margin directly so hosts can
/// schedule a redraw wake without duplicating scheduler policy.
///
/// # Cadence
///
/// Animation, background, and continuous-input frames run at the fastest
/// interval their build cost fits. The cadence speeds up only when the cost
/// is clearly under a boundary, and holds a slower one for at least
/// [`SchedulerConfig::cadence_hold`]. Input frames run at the display's
/// fastest interval.
///
/// # Usage
///
/// ```rust,ignore
/// let plan = scheduler.plan(opportunity, demand, frame_index);
/// // ... build and submit frame ...
/// scheduler.observe(&feedback);
/// ```
#[derive(Debug)]
pub struct Scheduler {
    config: SchedulerConfig,
    pipeline_depth: u8,
    build_cost_ema: Ema,
    /// How far past its target a frame is really shown.
    present_latency_ema: Ema,
    present_latency_ticks: u64,
    safety_margin_ticks: u64,
    /// How late the host wakes for a frame's start.
    wake_lateness_ema: Ema,
    wake_lateness_ticks: u64,
    misses: MissWindow,
    consecutive_hits: u32,
    /// Consecutive plans whose work would also have had time one depth
    /// shallower.
    shallower_fits: u32,
    cadence: Option<HeldCadence>,
    drain: QueueDrain,
}

impl Scheduler {
    /// Creates a low-level scheduler with the given configuration.
    ///
    /// Use [`FrameDriver::new`](crate::FrameDriver::new) instead when the host
    /// wants frameclock to own pending demand and queued frame lifecycle state.
    #[must_use]
    pub fn new(mut config: SchedulerConfig) -> Self {
        config.min_depth = config.min_depth.max(1);
        config.max_depth = config.max_depth.max(config.min_depth);
        config.initial_depth = config
            .initial_depth
            .clamp(config.min_depth, config.max_depth);
        config.ema_alpha = sanitize_ema_alpha(config.ema_alpha);
        config.safety_multiplier = sanitize_safety_multiplier(config.safety_multiplier);
        if let DegradationPolicy::Adaptive {
            miss_threshold,
            miss_window,
            ..
        } = &mut config.degradation_policy
        {
            *miss_window = (*miss_window).clamp(1, MAX_MISS_WINDOW);
            *miss_threshold = (*miss_threshold).clamp(1, *miss_window);
        }

        Self {
            pipeline_depth: config.initial_depth,
            build_cost_ema: Ema::new(config.ema_alpha),
            present_latency_ema: Ema::seeded(sanitize_ema_alpha(config.present_latency_alpha), 0.0),
            present_latency_ticks: 0,
            safety_margin_ticks: 0,
            wake_lateness_ema: Ema::new(config.ema_alpha),
            wake_lateness_ticks: 0,
            misses: MissWindow::default(),
            consecutive_hits: 0,
            shallower_fits: 0,
            cadence: None,
            drain: QueueDrain::default(),
            config,
        }
    }

    /// Produces a [`FramePlan`] from a frame opportunity, demand, and frame id.
    ///
    /// Hosts should usually call this only with non-empty [`FrameDemand`].
    /// `FrameDemand::NONE` is accepted for passive pacing diagnostics or
    /// backend bookkeeping, but it should not be treated as ordinary render
    /// demand.
    ///
    /// `frame_index` identifies the planned content frame for diagnostics and
    /// summaries. [`FrameDriver`](crate::FrameDriver) owns this counter for
    /// retained hosts; low-level scheduler integrations pass their own
    /// lifecycle id here.
    #[must_use]
    pub fn plan(
        &mut self,
        opportunity: FrameOpportunity,
        demand: FrameDemand,
        frame_index: u64,
    ) -> FramePlan {
        debug_assert!(
            self.config
                .timebase
                .ticks_to_nanos(opportunity.display_timing.min_interval().ticks())
                <= MAX_PLAUSIBLE_REFRESH_NANOS,
            "display timing's fastest interval is {} ticks, {} ns in the configured \
             timebase; durations are host ticks, so build them with \
             `Duration::from_nanos(nanos, timebase)`",
            opportunity.display_timing.min_interval().ticks(),
            self.config
                .timebase
                .ticks_to_nanos(opportunity.display_timing.min_interval().ticks()),
        );
        let tick = opportunity.tick;
        let hints = opportunity.hints;
        let source_interval = self.source_interval(opportunity);
        let build_cost = self.build_cost_estimate();
        let frame_interval = self.frame_interval(
            tick.now,
            demand,
            opportunity.display_timing,
            source_interval,
            build_cost,
        );
        // A hard deadline belongs to a resource that pipeline depth cannot
        // give more time, so only the cadence moves it.
        self.drain.interval = source_interval.ticks();
        // Holding a frame back a refresh drains a frame queued ahead of it.
        let held_back = if self.drain.pending && demand.dominant_class() != FrameDemandClass::Input
        {
            self.drain.pending = false;
            self.drain.drains += 1;
            source_interval
        } else {
            Duration::ZERO
        };
        let schedule_delta = if hints.has_hard_deadline() {
            frame_interval.saturating_sub(source_interval)
        } else {
            self.schedule_delta(demand, frame_interval, source_interval)
        }
        .saturating_add(held_back);
        let presentation_timing = hints.presentation_timing();
        let platform_present = if presentation_timing.has_target_present() {
            hints
                .desired_present()
                .filter(|present| *present >= tick.now)
        } else {
            None
        };
        let base_present = platform_present
            .unwrap_or_else(|| tick.now.checked_add(source_interval).unwrap_or(tick.now));
        let scheduled_present = base_present
            .checked_add(schedule_delta)
            .unwrap_or(base_present);
        let present_latency = self.present_latency_shift(source_interval);
        let base_commit_deadline = if hints.has_hard_deadline() {
            hints.latest_commit()
        } else {
            hints.latest_commit().max(tick.now)
        };
        let commit_deadline = base_commit_deadline
            .checked_add(schedule_delta)
            .unwrap_or(base_commit_deadline);
        if hints.has_hard_deadline() {
            // Depth cannot extend a hard deadline, so the frame would fit
            // one depth shallower too.
            self.note_fit();
        } else {
            self.note_shallower_fit(tick.now, commit_deadline, demand, frame_interval);
        }

        let (target_present, sample_time) = match presentation_timing {
            PresentationTiming::Predictive | PresentationTiming::Estimated => {
                // Where the frame will really be seen: the platform's
                // predicted present plus however late frames have been.
                let shown_at = scheduled_present
                    .checked_add(present_latency)
                    .unwrap_or(scheduled_present);
                let target_present = platform_present.map(|_| shown_at);
                let sample_time = target_present.unwrap_or(shown_at);
                (target_present, sample_time)
            }
            PresentationTiming::PacingOnly => {
                // No reliable present time; sample at the scheduler-selected
                // pacing target but do not report it as presentation truth.
                (None, scheduled_present)
            }
        };

        FramePlan {
            demand,
            frame_interval,
            frame_start: if hints.starts_at_tick() {
                tick_aligned_start(
                    tick.now,
                    frame_interval.saturating_add(held_back),
                    source_interval,
                )
            } else {
                self.frame_start(tick.now, commit_deadline, demand)
            },
            starts_at_tick: hints.starts_at_tick(),
            sample_time,
            target_present,
            presentation_timing,
            commit_deadline,
            pipeline_depth: self.pipeline_depth,
            present_latency,
            held_back,
            output: tick.output,
            frame_index,
        }
    }

    /// The learned presentation latency, rounded to whole refresh
    /// intervals: a frame is shown at a refresh, not between two.
    fn present_latency_shift(&self, source_interval: Duration) -> Duration {
        let interval = source_interval.ticks();
        if self.present_latency_ticks == 0 || interval == 0 {
            return Duration::ZERO;
        }
        let intervals = (self.present_latency_ticks + interval / 2) / interval;
        Duration(
            intervals
                .min(MAX_LATENCY_INTERVALS)
                .saturating_mul(interval),
        )
    }

    fn schedule_delta(
        &self,
        demand: FrameDemand,
        frame_interval: Duration,
        source_interval: Duration,
    ) -> Duration {
        let cadence_delta = frame_interval.saturating_sub(source_interval);
        let depth_delta = self.depth_lookahead_delta(demand, frame_interval);
        cadence_delta.saturating_add(depth_delta)
    }

    fn depth_lookahead_delta(&self, demand: FrameDemand, frame_interval: Duration) -> Duration {
        if matches!(
            demand.dominant_class(),
            FrameDemandClass::Input | FrameDemandClass::ContinuousInput
        ) {
            return Duration::ZERO;
        }

        frame_interval.saturating_mul(u64::from(self.pipeline_depth.saturating_sub(1)))
    }

    fn source_interval(&self, opportunity: FrameOpportunity) -> Duration {
        opportunity
            .tick
            .refresh_interval
            .filter(|interval| !interval.is_zero())
            .unwrap_or_else(|| {
                let display_min = opportunity.display_timing.min_interval();
                if display_min.is_zero() {
                    self.config.nominal_latency
                } else {
                    display_min
                }
            })
    }

    /// Notes whether a plan's work would also have had time one depth
    /// shallower: whether it could still have started at its tick with its
    /// deadline a frame interval sooner.
    fn note_shallower_fit(
        &mut self,
        now: HostTime,
        commit_deadline: HostTime,
        demand: FrameDemand,
        frame_interval: Duration,
    ) {
        if self.pipeline_depth <= self.config.min_depth {
            return;
        }
        // Depth does not move an input frame's timing, so it fits however
        // shallow the pipeline is.
        let fits = self.depth_lookahead_delta(demand, frame_interval).is_zero()
            || commit_deadline
                .checked_sub(frame_interval.saturating_add(self.frame_start_margin()))
                .is_some_and(|start| start >= now);
        self.shallower_fits = if fits {
            self.shallower_fits.saturating_add(1)
        } else {
            0
        };
    }

    /// Notes a plan whose timing pipeline depth does not move.
    fn note_fit(&mut self) {
        if self.pipeline_depth > self.config.min_depth {
            self.shallower_fits = self.shallower_fits.saturating_add(1);
        }
    }

    fn frame_interval(
        &mut self,
        now: HostTime,
        demand: FrameDemand,
        display: DisplayTiming,
        source_interval: Duration,
        build_cost: Duration,
    ) -> Duration {
        // The fastest cadence on offer. A fixed display refreshes at the
        // tick's interval. A variable one ticks at whatever interval it last
        // ran at, which says nothing about how fast it can go, so its range
        // decides: otherwise a display that slowed down could never speed
        // back up.
        let fastest = if display.is_variable() || source_interval.is_zero() {
            display.min_interval()
        } else {
            source_interval
        };
        let policy = demand.dominant_class();
        let needed = match policy {
            FrameDemandClass::None | FrameDemandClass::Input => return fastest,
            FrameDemandClass::ContinuousInput => build_cost,
            FrameDemandClass::Animation => build_cost.saturating_add(fastest.div_u64(4)),
            FrameDemandClass::Background => build_cost
                .saturating_add(fastest.div_u64(4))
                .max(fastest.saturating_mul(2)),
        };

        let wanted = display.choose_interval(needed).max(fastest);
        let held = self
            .cadence
            .filter(|held| held.class == policy)
            .and_then(|held| Some((held, held.at(fastest, display)?)));
        let Some((held, interval)) = held else {
            self.hold_cadence(policy, fastest, wanted, None);
            return wanted;
        };
        // A sixteenth of a refresh either way of the boundary keeps the
        // cadence: build cost that wanders across it does not move it.
        // Continuous input plans with no slack, so its work would overrun
        // an interval held inside the band; it slows down at the boundary.
        let band = fastest.div_u64(16);
        let slow_band = if policy == FrameDemandClass::ContinuousInput {
            Duration::ZERO
        } else {
            band
        };
        let slower = display
            .choose_interval(needed.saturating_sub(slow_band))
            .max(fastest);
        if slower > interval {
            self.hold_cadence(policy, fastest, slower, Some(now));
            return slower;
        }
        let faster = display
            .choose_interval(needed.saturating_add(band))
            .max(fastest);
        let held_long_enough = held.slowed_at.is_none_or(|slowed_at| {
            now.saturating_duration_since(slowed_at) >= self.config.cadence_hold
        });
        if faster < interval && held_long_enough {
            self.hold_cadence(policy, fastest, faster, None);
            return faster;
        }
        interval
    }

    fn hold_cadence(
        &mut self,
        class: FrameDemandClass,
        fastest: Duration,
        interval: Duration,
        slowed_at: Option<HostTime>,
    ) {
        self.cadence = Some(HeldCadence {
            class,
            fastest,
            interval,
            slowed_at,
        });
    }

    fn frame_start(
        &self,
        now: HostTime,
        commit_deadline: HostTime,
        demand: FrameDemand,
    ) -> HostTime {
        if demand.dominant_class() == FrameDemandClass::Input {
            return now;
        }

        commit_deadline
            .checked_sub(self.frame_start_margin())
            .unwrap_or(now)
            .max(now)
    }

    fn frame_start_margin(&self) -> Duration {
        let learned = Duration(self.safety_margin_ticks);
        let build = if learned > self.config.minimum_frame_start_margin {
            learned
        } else {
            self.config.minimum_frame_start_margin
        };
        build.saturating_add(Duration(self.wake_lateness_ticks))
    }

    /// Learns how late the host woke to start a frame.
    ///
    /// `lateness` is how long after a plan's [`FramePlan::frame_start`] the
    /// host began the frame. A host's timer fires some time after the time
    /// it was set for, and a frame planned to start as late as its margins
    /// allow has none of that to spare: woken a millisecond late, it starts
    /// a millisecond short. The scheduler starts later frames earlier by
    /// this lateness, smoothed by [`SchedulerConfig::ema_alpha`] and scaled
    /// by [`SchedulerConfig::safety_multiplier`].
    ///
    /// [`FrameDriver`](crate::FrameDriver) calls this when a queued frame
    /// that starts [when planned](crate::PresentHints::starting_when_planned)
    /// is released or expires, so hosts using it do not. Frames that
    /// [start at a tick](crate::PresentHints::starting_at_tick) are released
    /// by the tick, not a wake, and teach nothing here.
    pub fn observe_wake(&mut self, lateness: Duration) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "host ticks of a wake's lateness"
        )]
        self.wake_lateness_ema.update(lateness.ticks() as f64);
        self.wake_lateness_ticks =
            f64_ticks_to_u64(self.wake_lateness_ema.get() * self.config.safety_multiplier);
    }

    fn build_cost_estimate(&self) -> Duration {
        if !self.build_cost_ema.initialized() {
            return Duration::ZERO;
        }

        Duration(f64_ticks_to_u64(self.build_cost_ema.get()))
    }

    /// Feeds presentation feedback to adapt scheduling parameters.
    ///
    /// Call this after submitting a frame planned by [`Self::plan`]. Hosts using
    /// [`FrameDriver::submit_frame`](crate::FrameDriver::submit_frame) do not
    /// call this directly because the driver observes feedback internally.
    pub fn observe(&mut self, feedback: &PresentFeedback) {
        // Update build cost EMA, over the whole frame when the host can
        // say what its submission cost beyond the host's own work.
        let build_ticks = feedback.build_cost().ticks();
        self.build_cost_ema.update(build_ticks as f64);

        // Update safety margin.
        self.safety_margin_ticks =
            f64_ticks_to_u64(self.build_cost_ema.get() * self.config.safety_multiplier);

        // Learn how late frames are shown. The frame was planned for a
        // target that already carried the estimate, so what is left over
        // corrects it: late frames raise it, early ones lower it. A frame
        // committed after its deadline was late because of its own build,
        // not the presentation pipeline, so it teaches nothing here.
        if self.config.present_latency_alpha > 0.0
            && feedback.submitted_at <= feedback.commit_deadline
            && let (Some(actual), Some(expected)) =
                (feedback.actual_present, feedback.expected_present)
        {
            #[expect(
                clippy::cast_precision_loss,
                reason = "host ticks of a frame's lateness"
            )]
            let residual = actual.ticks() as f64 - expected.ticks() as f64;
            #[expect(clippy::cast_precision_loss, reason = "the learned estimate")]
            let latency = (feedback.present_latency.ticks() as f64 + residual).max(0.0);
            self.present_latency_ema.update(latency);
            self.present_latency_ticks = f64_ticks_to_u64(self.present_latency_ema.get());
        }
        if self.config.queue_drain_after > 0
            && feedback.submitted_at <= feedback.commit_deadline
            && let Some(actual) = feedback.actual_present
        {
            // From when its build began to when it was shown. A host starts
            // each frame at about the same point in the refresh, so this is
            // steady, and a refresh longer for each frame queued ahead,
            // whatever the plan aimed for: depth and cadence move targets,
            // not the queue. A frame that starts when planned starts its
            // learned margin before its deadline, and the margin moves with
            // its build cost, so its latency runs from the deadline.
            let from = if feedback.starts_at_tick {
                feedback.build_start
            } else {
                feedback.commit_deadline
            };
            let latency = actual.saturating_duration_since(from);
            self.drain
                .observe(latency.ticks(), self.config.queue_drain_after);
        }

        if let DegradationPolicy::Adaptive {
            miss_threshold,
            miss_window,
            recovery_threshold,
        } = self.config.degradation_policy
        {
            // A frame planned at another depth says nothing about this one.
            if feedback
                .pipeline_depth
                .is_none_or(|depth| depth == self.pipeline_depth)
            {
                self.adapt_depth(feedback, miss_threshold, miss_window, recovery_threshold);
            }
        }
    }

    /// Adapts pipeline depth to one frame's feedback.
    ///
    /// `missed_deadline` is the strong signal: the backend believes it can
    /// classify the frame as a real hit or miss. `pacing_overrun` is weaker:
    /// it only says the frame ran past a pacing boundary, so it takes twice
    /// the misses to raise depth, and a frame that did not overrun is not
    /// a hit. Feedback with neither says nothing, and breaks a run of hits.
    ///
    /// A frame shown late that was committed by its deadline is not a miss
    /// here. Depth moves a frame's target and deadline together, so it gives
    /// more time only to a frame whose build did not fit; one built in time
    /// and shown late was held by the presentation pipeline, which
    /// [`SchedulerConfig::present_latency_alpha`] learns and
    /// [`SchedulerConfig::queue_drain_after`] drains.
    fn adapt_depth(
        &mut self,
        feedback: &PresentFeedback,
        miss_threshold: u32,
        miss_window: u32,
        recovery_threshold: u32,
    ) {
        let built_late = feedback.submitted_at > feedback.commit_deadline;
        let (missed, strong) = match (feedback.missed_deadline, feedback.pacing_overrun) {
            (Some(missed), _) => (missed && built_late, true),
            (None, Some(overran)) => (overran, false),
            (None, None) => {
                self.consecutive_hits = 0;
                return;
            }
        };
        self.misses.push(missed, miss_window);
        if missed {
            self.consecutive_hits = 0;
            let threshold = if strong {
                miss_threshold
            } else {
                miss_threshold.saturating_mul(2).min(miss_window)
            };
            if self.misses.misses() >= threshold && self.pipeline_depth < self.config.max_depth {
                self.set_depth(self.pipeline_depth + 1);
            }
        } else if strong {
            self.consecutive_hits = self.consecutive_hits.saturating_add(1);
            if self.consecutive_hits >= recovery_threshold
                && self.shallower_fits >= recovery_threshold
                && self.pipeline_depth > self.config.min_depth
            {
                self.set_depth(self.pipeline_depth - 1);
            }
        } else {
            self.consecutive_hits = 0;
        }
    }

    fn set_depth(&mut self, depth: u8) {
        self.pipeline_depth = depth;
        self.misses = MissWindow::default();
        self.consecutive_hits = 0;
        self.shallower_fits = 0;
    }

    /// Returns the current pipeline depth.
    #[must_use]
    pub fn pipeline_depth(&self) -> u8 {
        self.pipeline_depth
    }

    /// Returns how late frames are being shown, in host-time ticks.
    ///
    /// Zero until feedback carrying actual-present timestamps arrives, or
    /// when [`SchedulerConfig::present_latency_alpha`] is zero.
    #[must_use]
    pub fn present_latency_ticks(&self) -> u64 {
        self.present_latency_ticks
    }

    /// Returns the current estimated safety margin in ticks.
    #[must_use]
    pub fn safety_margin_ticks(&self) -> u64 {
        self.safety_margin_ticks
    }

    /// Returns how much earlier queued frames start because the host wakes
    /// late for them, in host-time ticks. See [`Self::observe_wake`].
    #[must_use]
    pub fn wake_lateness_ticks(&self) -> u64 {
        self.wake_lateness_ticks
    }

    /// Returns a snapshot of the current scheduler adaptation state.
    #[must_use]
    pub const fn state(&self) -> SchedulerState {
        SchedulerState {
            pipeline_depth: self.pipeline_depth,
            present_latency_ticks: self.present_latency_ticks,
            safety_margin_ticks: self.safety_margin_ticks,
            recent_misses: self.misses.misses(),
            consecutive_hits: self.consecutive_hits,
            wake_lateness_ticks: self.wake_lateness_ticks,
            queue_drains: self.drain.drains,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::output::OutputId;
    use crate::time::HostTime;
    use crate::timing::{FrameTick, PresentHints};

    use super::*;

    const REFRESH_INTERVAL: Duration = Duration(16_666_667);

    fn make_tick(now: u64, predicted: Option<u64>) -> FrameTick {
        FrameTick {
            now: HostTime(now),
            predicted_present: predicted.map(HostTime),
            refresh_interval: Some(REFRESH_INTERVAL),
            output: OutputId(0),
            prev_actual_present: None,
        }
    }

    fn make_hints(
        presentation_timing: PresentationTiming,
        predicted: Option<u64>,
        deadline: u64,
    ) -> PresentHints {
        if let Some(predicted) = predicted
            && presentation_timing.has_target_present()
        {
            return PresentHints::new(
                presentation_timing,
                Some(HostTime(predicted)),
                HostTime(deadline),
            );
        }

        PresentHints::new(presentation_timing, None, HostTime(deadline))
    }

    fn make_opportunity(
        presentation_timing: PresentationTiming,
        now: u64,
        predicted: Option<u64>,
        deadline: u64,
    ) -> FrameOpportunity {
        let tick = make_tick(now, predicted);
        let hints = make_hints(presentation_timing, predicted, deadline);
        FrameOpportunity {
            tick,
            hints,
            display_timing: DisplayTiming::fixed(REFRESH_INTERVAL),
        }
    }

    /// Plans an animation frame with plenty of time before its deadline,
    /// then observes `feedback` for it.
    fn plan_roomy_and_observe(sched: &mut Scheduler, feedback: &PresentFeedback) {
        _ = sched.plan(
            make_opportunity(
                PresentationTiming::Predictive,
                1_000,
                Some(17_000_000),
                16_000_000,
            ),
            FrameDemand::ANIMATION,
            0,
        );
        sched.observe(feedback);
    }

    #[test]
    fn default_durations_follow_the_host_timebase() {
        // Mach ticks on Apple silicon: 125/3 ns each.
        let mach = Timebase::new(125, 3);
        for config in [
            SchedulerConfig::predictive(mach),
            SchedulerConfig::estimated(mach),
            SchedulerConfig::pacing_only(mach),
        ] {
            assert_eq!(config.minimum_frame_start_margin, Duration(24_000));
        }
        assert_eq!(
            SchedulerConfig::pacing_only(mach).nominal_latency,
            Duration(384_000)
        );

        // Browser microsecond ticks.
        let micros = Timebase::new(1000, 1);
        let config = SchedulerConfig::pacing_only(micros);
        assert_eq!(config.minimum_frame_start_margin, Duration(1_000));
        assert_eq!(config.nominal_latency, Duration(16_000));
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "durations are host ticks"))]
    fn nanosecond_display_timing_in_mach_ticks_is_caught() {
        // 16.7 ms written as nanoseconds is about 0.7 s of Mach ticks.
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::new(125, 3)));
        let opportunity = make_opportunity(
            PresentationTiming::Predictive,
            1_000_000,
            Some(1_400_000),
            1_300_000,
        );
        _ = sched.plan(opportunity, FrameDemand::ANIMATION, 0);
    }

    #[test]
    fn display_timing_converted_to_mach_ticks_is_accepted() {
        let timebase = Timebase::new(125, 3);
        let mut sched = Scheduler::new(SchedulerConfig::predictive(timebase));
        let opportunity = FrameOpportunity {
            display_timing: DisplayTiming::fixed(Duration::from_nanos(16_666_667, timebase)),
            ..make_opportunity(
                PresentationTiming::Predictive,
                1_000_000,
                Some(1_400_000),
                1_300_000,
            )
        };
        _ = sched.plan(opportunity, FrameDemand::ANIMATION, 0);
    }

    #[test]
    fn predictive_plan_uses_predicted_present() {
        let config = SchedulerConfig::predictive(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(plan.demand, FrameDemand::ANIMATION);
        assert_eq!(plan.frame_interval, REFRESH_INTERVAL);
        assert_eq!(plan.target_present, Some(HostTime(2000)));
        assert_eq!(plan.sample_time, HostTime(2000));
        assert_eq!(plan.commit_deadline, HostTime(1800));
        assert_eq!(plan.frame_start, HostTime(1000));
    }

    #[test]
    fn a_frame_shown_late_moves_the_next_frames_target_present() {
        // Fixed depth, so only the latency compensation moves the target.
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        let mut sched = Scheduler::new(config);
        let predicted = 2000;
        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(predicted), 1800),
            FrameDemand::ANIMATION,
            0,
        );
        assert_eq!(plan.target_present, Some(HostTime(predicted)));

        // It was shown two refreshes after the time it was planned for.
        let late = HostTime(predicted + 2 * REFRESH_INTERVAL.ticks());
        for _ in 0..20 {
            sched.observe(&PresentFeedback {
                present_latency: Duration::ZERO,
                pipeline_depth: None,
                submitted_at: HostTime(1500),
                commit_deadline: plan.commit_deadline,
                build_start: HostTime(1000),
                starts_at_tick: false,
                work: None,
                expected_present: plan.target_present,
                actual_present: Some(late),
                missed_deadline: Some(true),
                pacing_overrun: None,
            });
        }

        let planned = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(predicted), 1800),
            FrameDemand::ANIMATION,
            1,
        );

        assert_eq!(
            planned.target_present,
            Some(HostTime(predicted + 2 * REFRESH_INTERVAL.ticks())),
            "the next frame is planned, and sampled, for when it will be seen"
        );
        assert_eq!(
            planned.sample_time,
            HostTime(predicted + 2 * REFRESH_INTERVAL.ticks())
        );
        // Pacing is unchanged: the frame still starts and commits as before.
        assert_eq!(planned.frame_interval, REFRESH_INTERVAL);
        assert_eq!(planned.commit_deadline, HostTime(1800));
        assert!(sched.present_latency_ticks() > 0);
    }

    #[test]
    fn a_frame_committed_late_does_not_teach_present_latency() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        let mut sched = Scheduler::new(config);
        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            0,
        );

        // A long first build, such as shader compilation: the frame missed
        // its commit deadline and was shown 200 ms late.
        sched.observe(&PresentFeedback::new(
            &plan,
            HostTime(1000),
            HostTime(200_001_000),
            Some(HostTime(200_002_000)),
        ));

        assert_eq!(sched.present_latency_ticks(), 0);
        let next = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            1,
        );
        assert_eq!(next.present_latency, Duration::ZERO);
    }

    #[test]
    fn one_frame_shown_late_does_not_set_present_latency_outright() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        let mut sched = Scheduler::new(config);
        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            0,
        );

        // Committed on time, but a compositor stall showed it 12 refreshes
        // late. One sample moves the estimate by `present_latency_alpha` of
        // that, not all of it.
        let stall = 12 * REFRESH_INTERVAL.ticks();
        sched.observe(&PresentFeedback::new(
            &plan,
            HostTime(1000),
            HostTime(1500),
            Some(HostTime(2000 + stall)),
        ));

        let next = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            1,
        );
        assert_eq!(next.present_latency, REFRESH_INTERVAL);
    }

    #[test]
    fn present_latency_compensation_is_off_without_actual_present_times() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        let mut sched = Scheduler::new(config);
        for _ in 0..20 {
            sched.observe(&PresentFeedback {
                present_latency: Duration::ZERO,
                pipeline_depth: None,
                submitted_at: HostTime(1500),
                commit_deadline: HostTime(u64::MAX),
                build_start: HostTime(1000),
                starts_at_tick: false,
                work: None,
                expected_present: Some(HostTime(2000)),
                actual_present: None,
                missed_deadline: None,
                pacing_overrun: Some(true),
            });
        }

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(sched.present_latency_ticks(), 0);
        assert_eq!(plan.target_present, Some(HostTime(2000)));
    }

    #[test]
    fn plan_uses_explicit_frame_index() {
        let config = SchedulerConfig::predictive(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            42,
        );

        assert_eq!(plan.frame_index, 42);
    }

    #[test]
    fn pacing_only_plan_has_no_target_present() {
        let config = SchedulerConfig::pacing_only(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::PacingOnly, 1_000_000, None, 17_000_000),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(plan.target_present, None);
        // sample_time = now + the selected pacing interval.
        assert_eq!(plan.sample_time, HostTime(17_666_667));
        assert_eq!(plan.frame_start, HostTime(16_000_000));
    }

    #[test]
    fn frame_start_uses_configured_start_margin() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(plan.frame_start, HostTime(1550));
    }

    #[test]
    fn frame_start_uses_learned_safety_margin_when_larger() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.minimum_frame_start_margin = Duration(100);
        config.safety_multiplier = 2.0;
        let mut sched = Scheduler::new(config);
        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(1_200),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        sched.observe(&feedback);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 2_000),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(sched.safety_margin_ticks(), 400);
        assert_eq!(plan.frame_start, HostTime(1_600));
    }

    #[test]
    fn scheduler_sanitizes_non_finite_float_config() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.ema_alpha = f64::NAN;
        config.safety_multiplier = f64::INFINITY;
        let mut sched = Scheduler::new(config);
        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(1_200),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };

        sched.observe(&feedback);

        assert_eq!(sched.safety_margin_ticks(), 200);
    }

    #[test]
    fn frame_start_clamps_to_tick_now_when_start_is_due() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.minimum_frame_start_margin = Duration(1_000);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_500, Some(2_000), 1_800),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(plan.frame_start, HostTime(1_500));
    }

    #[test]
    fn hints_starting_at_tick_start_the_frame_at_the_tick() {
        let mut config = SchedulerConfig::pacing_only(Timebase::NANOS);
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);
        let mut opportunity = make_opportunity(PresentationTiming::PacingOnly, 1_000, None, 9_000);

        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 0);
        assert_eq!(plan.frame_start, HostTime(8_750));

        opportunity.hints = opportunity.hints.starting_at_tick();
        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 1);
        assert_eq!(plan.frame_start, HostTime(1_000));
        assert_eq!(plan.commit_deadline, HostTime(9_000));
    }

    #[test]
    fn variable_refresh_speeds_back_up_after_running_slowly() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        // An 8-32 ms display whose last ticks came at its slowest rate.
        let mut opportunity = make_opportunity(
            PresentationTiming::Predictive,
            1_000,
            Some(33_001_000),
            32_001_000,
        );
        opportunity.tick.refresh_interval = Some(Duration(32_000_000));
        opportunity.display_timing =
            DisplayTiming::variable(Duration(8_000_000), Duration(32_000_000), None);

        for demand in [FrameDemand::INPUT, FrameDemand::ANIMATION] {
            let plan = sched.plan(opportunity, demand, 0);
            assert_eq!(plan.frame_interval, Duration(8_000_000), "{demand:?}");
        }
        let plan = sched.plan(opportunity, FrameDemand::BACKGROUND, 0);
        assert_eq!(plan.frame_interval, Duration(16_000_000));
    }

    #[test]
    fn hints_starting_at_tick_keep_a_slower_cadence() {
        let mut sched = Scheduler::new(SchedulerConfig::pacing_only(Timebase::NANOS));
        let mut opportunity = make_opportunity(PresentationTiming::PacingOnly, 1_000, None, 9_000);
        opportunity.hints = opportunity.hints.starting_at_tick();

        // Background work runs every other refresh, so its frame waits for
        // the next tick: half a refresh before it.
        let plan = sched.plan(opportunity, FrameDemand::BACKGROUND, 0);
        assert_eq!(plan.frame_interval, REFRESH_INTERVAL.saturating_mul(2));
        assert_eq!(
            plan.frame_start,
            HostTime(1_000 + REFRESH_INTERVAL.ticks() - REFRESH_INTERVAL.ticks() / 2)
        );
        assert_eq!(
            plan.commit_deadline,
            HostTime(9_000 + REFRESH_INTERVAL.ticks())
        );

        // Input still starts at the tick that offered it.
        let plan = sched.plan(opportunity, FrameDemand::INPUT, 1);
        assert_eq!(plan.frame_start, HostTime(1_000));
    }

    #[test]
    fn stale_predictive_present_is_ignored() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);
        let tick = make_tick(2_000, Some(1_500));
        let opportunity = FrameOpportunity {
            tick,
            hints: PresentHints::predictive(HostTime(1_500), HostTime(1_250)),
            display_timing: DisplayTiming::fixed(REFRESH_INTERVAL),
        };

        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 0);

        assert_eq!(plan.target_present, None);
        assert_eq!(plan.sample_time, HostTime(2_000) + REFRESH_INTERVAL);
        assert_eq!(plan.commit_deadline, HostTime(2_000));
        assert_eq!(plan.frame_start, HostTime(2_000));
    }

    #[test]
    fn input_demand_starts_immediately() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 1_800),
            FrameDemand::INPUT,
            0,
        );

        assert_eq!(plan.demand, FrameDemand::INPUT);
        assert_eq!(plan.frame_interval, REFRESH_INTERVAL);
        assert_eq!(plan.frame_start, HostTime(1_000));
    }

    #[test]
    fn animation_uses_stable_fixed_rate_divisor_when_work_is_slow() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.ema_alpha = 1.0;
        config.safety_multiplier = 1.0;
        let mut sched = Scheduler::new(config);
        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(21_000_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        sched.observe(&feedback);

        let plan = sched.plan(
            make_opportunity(
                PresentationTiming::Predictive,
                1_000_000,
                Some(17_666_667),
                16_666_667,
            ),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(plan.frame_interval, REFRESH_INTERVAL.saturating_mul(2));
        assert_eq!(plan.target_present, Some(HostTime(34_333_334)));
        assert_eq!(plan.commit_deadline, HostTime(33_333_334));
        assert_eq!(plan.frame_start, HostTime(13_333_334));
    }

    #[test]
    fn safety_multiplier_does_not_force_cadence_drop() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.ema_alpha = 1.0;
        config.safety_multiplier = 4.0;
        config.minimum_frame_start_margin = Duration::ZERO;
        let mut sched = Scheduler::new(config);
        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(11_000_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        sched.observe(&feedback);

        let plan = sched.plan(
            make_opportunity(
                PresentationTiming::Predictive,
                1_000_000,
                Some(50_000_000),
                50_000_000,
            ),
            FrameDemand::ANIMATION,
            0,
        );

        assert_eq!(sched.safety_margin_ticks(), 40_000_000);
        assert_eq!(plan.frame_interval, REFRESH_INTERVAL);
        assert_eq!(plan.commit_deadline, HostTime(50_000_000));
        assert_eq!(plan.frame_start, HostTime(10_000_000));
    }

    /// Teaches a scheduler with `ema_alpha` 1 that frames take `build`.
    fn observe_build(sched: &mut Scheduler, build: u64) {
        sched.observe(&PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(1_000_000 + build),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: None,
            pacing_overrun: None,
        });
    }

    fn cadence_scheduler() -> Scheduler {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.ema_alpha = 1.0;
        Scheduler::new(config)
    }

    fn animation_interval(sched: &mut Scheduler, now: u64) -> Duration {
        sched
            .plan(
                make_opportunity(PresentationTiming::Predictive, now, None, now + 15_000_000),
                FrameDemand::ANIMATION,
                0,
            )
            .frame_interval
    }

    #[test]
    fn cadence_holds_when_build_cost_hovers_at_a_boundary() {
        let mut sched = cadence_scheduler();
        // Animation needs its build cost and a quarter refresh: 12 ms fits
        // one refresh, 13 ms does not.
        observe_build(&mut sched, 13_000_000);
        assert_eq!(
            animation_interval(&mut sched, 1_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );
        for frame in 0..20 {
            let build = if frame % 2 == 0 {
                12_000_000
            } else {
                13_000_000
            };
            observe_build(&mut sched, build);
            assert_eq!(
                animation_interval(&mut sched, 1_000 + frame * 1_000),
                REFRESH_INTERVAL.saturating_mul(2),
                "frame {frame}"
            );
        }
    }

    #[test]
    fn continuous_input_slows_down_as_soon_as_its_work_needs_it() {
        let mut sched = cadence_scheduler();
        let scroll_interval = |sched: &mut Scheduler, now: u64| {
            sched
                .plan(
                    make_opportunity(PresentationTiming::Predictive, now, None, now + 15_000_000),
                    FrameDemand::CONTINUOUS_INPUT,
                    0,
                )
                .frame_interval
        };
        observe_build(&mut sched, 10_000_000);
        assert_eq!(scroll_interval(&mut sched, 1_000), REFRESH_INTERVAL);

        // Continuous input has no slack: 17 ms of work overruns a 16.7 ms
        // interval, however close to the boundary it is.
        observe_build(&mut sched, 17_000_000);
        assert_eq!(
            scroll_interval(&mut sched, 2_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );
    }

    #[test]
    fn work_just_past_a_boundary_keeps_the_faster_cadence() {
        let mut sched = cadence_scheduler();
        observe_build(&mut sched, 5_000_000);
        assert_eq!(animation_interval(&mut sched, 1_000), REFRESH_INTERVAL);

        // 12.8 ms needs 17 ms with its quarter refresh to spare, within a
        // sixteenth of the boundary: one slow frame does not halve the rate.
        observe_build(&mut sched, 12_800_000);
        assert_eq!(animation_interval(&mut sched, 2_000), REFRESH_INTERVAL);
        observe_build(&mut sched, 14_000_000);
        assert_eq!(
            animation_interval(&mut sched, 3_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );
    }

    #[test]
    fn cadence_speeds_up_once_the_work_fits_with_room_and_the_hold_has_passed() {
        let mut sched = cadence_scheduler();
        observe_build(&mut sched, 5_000_000);
        assert_eq!(animation_interval(&mut sched, 1_000), REFRESH_INTERVAL);
        observe_build(&mut sched, 15_000_000);
        assert_eq!(
            animation_interval(&mut sched, 2_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );

        // 8 ms fits one refresh with room, but the slower cadence is new.
        observe_build(&mut sched, 8_000_000);
        assert_eq!(
            animation_interval(&mut sched, 400_000_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );
        assert_eq!(
            animation_interval(&mut sched, 500_002_000),
            REFRESH_INTERVAL
        );

        // Slowing down again does not wait.
        observe_build(&mut sched, 15_000_000);
        assert_eq!(
            animation_interval(&mut sched, 500_003_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );
    }

    #[test]
    fn a_first_cadence_is_not_held() {
        let mut sched = cadence_scheduler();
        // The first frame is costly, as first frames are.
        observe_build(&mut sched, 14_000_000);
        assert_eq!(
            animation_interval(&mut sched, 1_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );

        // Nothing slowed down to that cadence, so it speeds up at once.
        observe_build(&mut sched, 8_000_000);
        assert_eq!(animation_interval(&mut sched, 2_000), REFRESH_INTERVAL);
    }

    #[test]
    fn held_cadence_follows_a_wandering_refresh_interval() {
        let mut sched = cadence_scheduler();
        observe_build(&mut sched, 13_000_000);
        assert_eq!(
            animation_interval(&mut sched, 1_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );

        // The display measures a little faster; the cadence keeps its two
        // refreshes, at the new interval.
        observe_build(&mut sched, 12_000_000);
        let measured = Duration(16_665_000);
        let mut opportunity =
            make_opportunity(PresentationTiming::Predictive, 2_000, None, 15_002_000);
        opportunity.tick.refresh_interval = Some(measured);
        opportunity.display_timing = DisplayTiming::fixed(measured);
        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 0);
        assert_eq!(plan.frame_interval, measured.saturating_mul(2));
    }

    #[test]
    fn a_new_display_rate_starts_the_cadence_afresh() {
        let mut sched = cadence_scheduler();
        observe_build(&mut sched, 13_000_000);
        assert_eq!(
            animation_interval(&mut sched, 1_000),
            REFRESH_INTERVAL.saturating_mul(2)
        );

        // At 120 Hz the same work fits two refreshes of 8.3 ms, so the frame
        // interval drops to 16.7 ms at once, without waiting out the hold.
        let fast = Duration(8_333_333);
        let mut opportunity =
            make_opportunity(PresentationTiming::Predictive, 2_000, None, 7_000_000);
        opportunity.tick.refresh_interval = Some(fast);
        opportunity.display_timing = DisplayTiming::fixed(fast);
        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 0);
        assert_eq!(plan.frame_interval, fast.saturating_mul(2));
    }

    /// A scheduler with nothing else adapting, so only a drain moves plans.
    fn drain_scheduler() -> Scheduler {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        config.present_latency_alpha = 0.0;
        Scheduler::new(config)
    }

    fn drain_plan(sched: &mut Scheduler) -> FramePlan {
        sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 1_800),
            FrameDemand::ANIMATION,
            0,
        )
    }

    /// Feedback for a frame planned for 2,000 and shown `refreshes` refreshes
    /// and `extra` ticks after it.
    fn shown(sched: &mut Scheduler, refreshes: u64, extra: u64) {
        sched.observe(&PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(1_500),
            commit_deadline: HostTime(1_800),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: Some(HostTime(2_000)),
            actual_present: Some(HostTime(
                2_000 + refreshes * REFRESH_INTERVAL.ticks() + extra,
            )),
            missed_deadline: None,
            pacing_overrun: None,
        });
    }

    #[test]
    fn frames_kept_a_refresh_late_drain_once() {
        let mut sched = drain_scheduler();
        assert_eq!(drain_plan(&mut sched).target_present, Some(HostTime(2_000)));
        for _ in 0..5 {
            shown(&mut sched, 0, 0);
        }
        // A present missed the cutoff, and every frame after it waits a
        // refresh behind it.
        for _ in 0..29 {
            shown(&mut sched, 1, 0);
        }
        assert_eq!(drain_plan(&mut sched).target_present, Some(HostTime(2_000)));
        shown(&mut sched, 1, 0);

        // The next frame is held back a refresh, once.
        let held = drain_plan(&mut sched);
        assert_eq!(
            held.target_present,
            Some(HostTime(2_000 + REFRESH_INTERVAL.ticks()))
        );
        assert_eq!(
            held.commit_deadline,
            HostTime(1_800 + REFRESH_INTERVAL.ticks())
        );
        assert_eq!(sched.state().queue_drains, 1);
        assert_eq!(drain_plan(&mut sched).target_present, Some(HostTime(2_000)));

        // It worked: frames are back on the floor, and a later queue drains
        // too.
        shown(&mut sched, 0, 0);
        for _ in 0..30 {
            shown(&mut sched, 1, 0);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 2);
    }

    #[test]
    fn a_drain_that_does_not_help_makes_the_latency_the_floor() {
        let mut sched = drain_scheduler();
        drain_plan(&mut sched);
        for _ in 0..3 {
            shown(&mut sched, 0, 0);
        }
        for _ in 0..30 {
            shown(&mut sched, 1, 0);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 1);

        // Still a refresh late through three held frames: the pipeline
        // changed. No more frames are held back for it.
        for _ in 0..240 {
            shown(&mut sched, 1, 0);
            drain_plan(&mut sched);
        }
        assert_eq!(sched.state().queue_drains, 3);
        // A refresh beyond the new floor drains again.
        for _ in 0..30 {
            shown(&mut sched, 2, 0);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 4);
    }

    #[test]
    fn one_fast_frame_does_not_lower_the_floor() {
        let mut sched = drain_scheduler();
        drain_plan(&mut sched);
        for _ in 0..3 {
            shown(&mut sched, 1, 0);
        }
        // One frame a refresh sooner than the rest, then the usual latency.
        shown(&mut sched, 0, 0);
        for _ in 0..100 {
            shown(&mut sched, 1, 0);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 0);

        // Two in a row lower it, and the usual latency is a queue again.
        for _ in 0..2 {
            shown(&mut sched, 0, 0);
        }
        for _ in 0..30 {
            shown(&mut sched, 1, 0);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 1);
    }

    #[test]
    fn less_than_half_a_refresh_late_does_not_drain() {
        let mut sched = drain_scheduler();
        drain_plan(&mut sched);
        for _ in 0..3 {
            shown(&mut sched, 0, 0);
        }
        for _ in 0..100 {
            shown(&mut sched, 0, REFRESH_INTERVAL.ticks() / 3);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 0);
    }

    /// Feedback for a frame with its own build start, deadline, and present.
    fn shown_at(
        sched: &mut Scheduler,
        starts_at_tick: bool,
        build_start: u64,
        commit_deadline: u64,
        actual_present: u64,
    ) {
        sched.observe(&PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(commit_deadline - 100),
            commit_deadline: HostTime(commit_deadline),
            build_start: HostTime(build_start),
            starts_at_tick,
            expected_present: Some(HostTime(actual_present)),
            actual_present: Some(HostTime(actual_present)),
            missed_deadline: None,
            pacing_overrun: None,
        });
    }

    #[test]
    fn a_margin_that_grows_does_not_drain_frames_started_when_planned() {
        let mut sched = drain_scheduler();
        drain_plan(&mut sched);
        let (deadline, shown) = (20_000_000, 21_000_000);
        for _ in 0..5 {
            shown_at(&mut sched, false, deadline - 1_000_000, deadline, shown);
        }
        // The build cost grew, so frames start 10 ms before their deadline
        // instead of 1 ms, and are shown when they were.
        for _ in 0..60 {
            shown_at(&mut sched, false, deadline - 11_000_000, deadline, shown);
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 0);
    }

    #[test]
    fn a_deadline_that_moves_does_not_drain_frames_started_at_a_tick() {
        let mut sched = drain_scheduler();
        drain_plan(&mut sched);
        let (tick, shown) = (1_000, 30_000_000);
        for _ in 0..5 {
            shown_at(&mut sched, true, tick, 2_000_000, shown);
        }
        // A deeper pipeline moved the deadline a refresh later; frames
        // still start at the tick and are shown when they were.
        for _ in 0..60 {
            shown_at(
                &mut sched,
                true,
                tick,
                2_000_000 + REFRESH_INTERVAL.ticks(),
                shown,
            );
        }
        drain_plan(&mut sched);
        assert_eq!(sched.state().queue_drains, 0);
    }

    #[test]
    fn an_input_frame_is_not_held_back_to_drain() {
        let mut sched = drain_scheduler();
        drain_plan(&mut sched);
        for _ in 0..3 {
            shown(&mut sched, 0, 0);
        }
        for _ in 0..30 {
            shown(&mut sched, 1, 0);
        }
        let input = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 1_800),
            FrameDemand::INPUT,
            0,
        );
        assert_eq!(input.held_back, Duration::ZERO);
        assert_eq!(input.target_present, Some(HostTime(2_000)));
        assert_eq!(sched.state().queue_drains, 0);

        // The next animation frame is held instead.
        let held = drain_plan(&mut sched);
        assert_eq!(held.held_back, REFRESH_INTERVAL);
        assert_eq!(sched.state().queue_drains, 1);
    }

    #[test]
    fn draining_works_while_present_latency_is_learned() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        let mut sched = Scheduler::new(config);
        let refresh = REFRESH_INTERVAL.ticks();
        // The platform shows a frame at its prediction, a refresh later for
        // each frame queued ahead of it.
        let mut queued = 0;
        for frame in 0..200_u64 {
            let plan = drain_plan(&mut sched);
            if frame == 5 {
                // A present missed the cutoff.
                queued = 1;
            }
            if !plan.held_back.is_zero() {
                // Held back, it leaves the queue a refresh to empty.
                queued = 0;
            }
            let predicted = 2_000 + plan.held_back.ticks();
            sched.observe(&PresentFeedback::new(
                &plan,
                HostTime(1_000 + plan.held_back.ticks()),
                HostTime(1_500 + plan.held_back.ticks()),
                Some(HostTime(predicted + queued * refresh)),
            ));
        }
        assert_eq!(sched.state().queue_drains, 1);
        assert!(
            sched.present_latency_ticks() < refresh / 2,
            "learned latency comes back down: {}",
            sched.present_latency_ticks()
        );
    }

    #[test]
    fn queue_drain_after_zero_turns_draining_off() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        config.present_latency_alpha = 0.0;
        config.queue_drain_after = 0;
        let mut sched = Scheduler::new(config);
        drain_plan(&mut sched);
        for _ in 0..3 {
            shown(&mut sched, 0, 0);
        }
        for _ in 0..100 {
            shown(&mut sched, 1, 0);
        }
        assert_eq!(drain_plan(&mut sched).target_present, Some(HostTime(2_000)));
        assert_eq!(sched.state().queue_drains, 0);
        assert_eq!(
            SchedulerConfig::pacing_only(Timebase::NANOS).queue_drain_after,
            0
        );
    }

    #[test]
    fn variable_refresh_without_granularity_uses_stable_divisor() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.ema_alpha = 1.0;
        config.safety_multiplier = 1.0;
        let mut sched = Scheduler::new(config);
        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(13_000_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        sched.observe(&feedback);
        let tick = FrameTick {
            now: HostTime(1_000_000),
            predicted_present: Some(HostTime(9_333_333)),
            refresh_interval: Some(Duration(8_333_333)),
            output: OutputId(0),
            prev_actual_present: None,
        };
        let opportunity = FrameOpportunity {
            tick,
            hints: PresentHints::predictive(HostTime(9_333_333), HostTime(8_333_333)),
            display_timing: DisplayTiming::variable(
                Duration(8_333_333),
                Duration(33_333_333),
                None,
            ),
        };

        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 0);

        assert_eq!(plan.frame_interval, Duration(16_666_666));
        assert_eq!(plan.target_present, Some(HostTime(17_666_666)));
        assert_eq!(plan.commit_deadline, HostTime(16_666_666));
        assert_eq!(plan.frame_start, HostTime(4_666_666));
    }

    #[test]
    fn variable_refresh_with_granularity_uses_direct_step() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.ema_alpha = 1.0;
        config.safety_multiplier = 1.0;
        let mut sched = Scheduler::new(config);
        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(13_000_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        sched.observe(&feedback);
        let tick = FrameTick {
            now: HostTime(1_000_000),
            predicted_present: Some(HostTime(9_333_333)),
            refresh_interval: Some(Duration(8_333_333)),
            output: OutputId(0),
            prev_actual_present: None,
        };
        let opportunity = FrameOpportunity {
            tick,
            hints: PresentHints::predictive(HostTime(9_333_333), HostTime(8_333_333)),
            display_timing: DisplayTiming::variable(
                Duration(8_333_333),
                Duration(33_333_333),
                Some(Duration(1_000_000)),
            ),
        };

        let plan = sched.plan(opportunity, FrameDemand::ANIMATION, 0);

        assert_eq!(plan.frame_interval, Duration(15_000_000));
        assert_eq!(plan.target_present, Some(HostTime(16_000_000)));
        assert_eq!(plan.commit_deadline, HostTime(15_000_000));
        assert_eq!(plan.frame_start, HostTime(3_000_000));
    }

    #[test]
    fn pipeline_depth_increases_after_misses() {
        let config = SchedulerConfig::predictive(Timebase::NANOS);
        let mut sched = Scheduler::new(config);
        assert_eq!(sched.pipeline_depth(), 1);

        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };

        sched.observe(&feedback);
        assert_eq!(sched.pipeline_depth(), 1); // 1 miss
        sched.observe(&feedback);
        assert_eq!(sched.pipeline_depth(), 1); // 2 misses
        sched.observe(&feedback);
        assert_eq!(sched.pipeline_depth(), 2); // 3 misses → increase
    }

    #[test]
    fn pipeline_depth_shifts_non_input_plan_by_whole_intervals() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 3;
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 1_800),
            FrameDemand::ANIMATION,
            0,
        );

        let lookahead = REFRESH_INTERVAL.saturating_mul(2);
        assert_eq!(plan.pipeline_depth, 3);
        assert_eq!(plan.frame_interval, REFRESH_INTERVAL);
        assert_eq!(plan.target_present, Some(HostTime(2_000) + lookahead));
        assert_eq!(plan.sample_time, HostTime(2_000) + lookahead);
        assert_eq!(plan.commit_deadline, HostTime(1_800) + lookahead);
        assert_eq!(
            plan.frame_start,
            HostTime(1_800) + lookahead - Duration(250)
        );
    }

    #[test]
    fn pipeline_depth_does_not_shift_one_shot_input() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 3;
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 1_800),
            FrameDemand::INPUT,
            0,
        );

        assert_eq!(plan.pipeline_depth, 3);
        assert_eq!(plan.target_present, Some(HostTime(2_000)));
        assert_eq!(plan.sample_time, HostTime(2_000));
        assert_eq!(plan.commit_deadline, HostTime(1_800));
        assert_eq!(plan.frame_start, HostTime(1_000));
    }

    #[test]
    fn pipeline_depth_does_not_shift_continuous_input() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 3;
        config.minimum_frame_start_margin = Duration(250);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Predictive, 1_000, Some(2_000), 1_800),
            FrameDemand::CONTINUOUS_INPUT,
            0,
        );

        assert_eq!(plan.pipeline_depth, 3);
        assert_eq!(plan.target_present, Some(HostTime(2_000)));
        assert_eq!(plan.sample_time, HostTime(2_000));
        assert_eq!(plan.commit_deadline, HostTime(1_800));
        assert_eq!(plan.frame_start, HostTime(1_550));
    }

    #[test]
    fn misses_count_across_hits_within_the_window() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };
        let hit = PresentFeedback {
            missed_deadline: Some(false),
            ..miss
        };

        sched.observe(&miss);
        sched.observe(&miss);
        sched.observe(&hit);
        assert_eq!(sched.pipeline_depth(), 1);
        // The third miss in the window raises depth, hit or no hit between.
        sched.observe(&miss);
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn frames_built_in_time_and_shown_late_do_not_raise_depth() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        // Committed by the deadline, shown two refreshes after the target:
        // the compositor held them, and more depth would not help.
        let held = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(1500),
            commit_deadline: HostTime(1800),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: Some(HostTime(2000)),
            actual_present: Some(HostTime(2000 + 2 * REFRESH_INTERVAL.ticks())),
            missed_deadline: Some(true),
            pacing_overrun: None,
        };
        for _ in 0..30 {
            sched.observe(&held);
        }
        assert_eq!(sched.pipeline_depth(), 1);

        // Committed after the deadline, they are misses.
        let built_late = PresentFeedback {
            submitted_at: HostTime(1900),
            ..held
        };
        for _ in 0..3 {
            sched.observe(&built_late);
        }
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn alternating_misses_raise_depth() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };
        let hit = PresentFeedback {
            missed_deadline: Some(false),
            ..miss
        };

        // Every other frame misses: never two in a row, but half of them.
        for _ in 0..3 {
            sched.observe(&miss);
            sched.observe(&hit);
        }
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn misses_older_than_the_window_do_not_count() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };
        let hit = PresentFeedback {
            missed_deadline: Some(false),
            ..miss
        };

        // One miss in every 30 frames is within the default policy.
        for _ in 0..4 {
            sched.observe(&miss);
            for _ in 0..29 {
                sched.observe(&hit);
            }
        }
        assert_eq!(sched.pipeline_depth(), 1);
    }

    #[test]
    fn feedback_planned_at_another_depth_does_not_move_depth() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };
        for _ in 0..3 {
            sched.observe(&miss);
        }
        assert_eq!(sched.pipeline_depth(), 2);

        // Frames planned at depth 1 were still in flight when it changed.
        let in_flight = PresentFeedback {
            pipeline_depth: Some(1),
            ..miss
        };
        for _ in 0..3 {
            sched.observe(&in_flight);
        }
        assert_eq!(sched.pipeline_depth(), 2);

        // Frames planned at depth 2 count.
        let planned = PresentFeedback {
            pipeline_depth: Some(2),
            ..miss
        };
        for _ in 0..3 {
            sched.observe(&planned);
        }
        assert_eq!(sched.pipeline_depth(), 3);
    }

    #[test]
    fn pipeline_depth_decreases_after_sustained_hits() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 3;
        let mut sched = Scheduler::new(config);
        assert_eq!(sched.pipeline_depth(), 3);

        let hit = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };

        for _ in 0..59 {
            plan_roomy_and_observe(&mut sched, &hit);
        }
        assert_eq!(sched.pipeline_depth(), 3); // 59 hits, not enough
        plan_roomy_and_observe(&mut sched, &hit); // 60th hit → decrease
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn depth_holds_while_one_shallower_would_not_have_time() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 2;
        config.ema_alpha = 1.0;
        let mut sched = Scheduler::new(config);

        // Every frame is on time, but takes 5 ms, and its tick comes 2 ms
        // before the deadline: one depth shallower it would have to start
        // before its tick.
        let hit = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(5_001_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        let tight = make_opportunity(
            PresentationTiming::Predictive,
            1_000,
            Some(3_000_000),
            2_000_000,
        );
        for _ in 0..100 {
            _ = sched.plan(tight, FrameDemand::ANIMATION, 0);
            sched.observe(&hit);
        }
        assert_eq!(sched.pipeline_depth(), 2);

        // With room before the deadline, it comes back.
        for _ in 0..60 {
            plan_roomy_and_observe(&mut sched, &hit);
        }
        assert_eq!(sched.pipeline_depth(), 1);
    }

    #[test]
    fn depth_recovers_on_frames_it_does_not_shape() {
        let hit = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(5_001_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };
        // Too tight for one depth shallower, if depth could move it.
        let tight = make_opportunity(
            PresentationTiming::Predictive,
            1_000,
            Some(3_000_000),
            2_000_000,
        );
        let mut hard = tight;
        hard.hints = hard.hints.with_hard_deadline();
        for (opportunity, demand) in [
            (tight, FrameDemand::INPUT),
            (tight, FrameDemand::CONTINUOUS_INPUT),
            (hard, FrameDemand::ANIMATION),
        ] {
            let mut config = SchedulerConfig::predictive(Timebase::NANOS);
            config.initial_depth = 2;
            config.ema_alpha = 1.0;
            let mut sched = Scheduler::new(config);
            for _ in 0..60 {
                _ = sched.plan(opportunity, demand, 0);
                sched.observe(&hit);
            }
            assert_eq!(sched.pipeline_depth(), 1, "{demand:?}");
        }
    }

    #[test]
    fn fixed_policy_never_changes_depth() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Fixed;
        config.initial_depth = 2;
        let mut sched = Scheduler::new(config);
        assert_eq!(sched.pipeline_depth(), 2);

        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };

        for _ in 0..10 {
            sched.observe(&miss);
        }
        assert_eq!(sched.pipeline_depth(), 2); // unchanged

        // Safety margin should still be tracked.
        assert!(sched.safety_margin_ticks() > 0);
    }

    #[test]
    fn custom_miss_threshold() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.degradation_policy = DegradationPolicy::Adaptive {
            miss_threshold: 5,
            miss_window: 30,
            recovery_threshold: 10,
        };
        let mut sched = Scheduler::new(config);

        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };

        for _ in 0..4 {
            sched.observe(&miss);
        }
        assert_eq!(sched.pipeline_depth(), 1); // 4 misses, threshold is 5
        sched.observe(&miss); // 5th miss → increase
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn recovery_respects_min_depth() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.initial_depth = 2;
        config.degradation_policy = DegradationPolicy::Adaptive {
            miss_threshold: 3,
            miss_window: 30,
            recovery_threshold: 2,
        };
        let mut sched = Scheduler::new(config);
        assert_eq!(sched.pipeline_depth(), 2);

        let hit = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };

        // 2 hits → decrease to 1 (min_depth).
        plan_roomy_and_observe(&mut sched, &hit);
        plan_roomy_and_observe(&mut sched, &hit);
        assert_eq!(sched.pipeline_depth(), 1);

        // 2 more hits → should stay at 1, can't go below min_depth.
        plan_roomy_and_observe(&mut sched, &hit);
        plan_roomy_and_observe(&mut sched, &hit);
        assert_eq!(sched.pipeline_depth(), 1);
    }

    #[test]
    fn estimated_presentation_timing_uses_desired_present() {
        let config = SchedulerConfig::predictive(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let plan = sched.plan(
            make_opportunity(PresentationTiming::Estimated, 1000, Some(2000), 1800),
            FrameDemand::ANIMATION,
            0,
        );

        // Estimated behaves like Predictive for target selection; hosts choose
        // a more conservative SchedulerConfig separately.
        assert_eq!(plan.target_present, Some(HostTime(2000)));
        assert_eq!(plan.sample_time, HostTime(2000));
    }

    #[test]
    fn feedback_without_a_verdict_neither_counts_nor_clears_misses() {
        let mut sched = Scheduler::new(SchedulerConfig::predictive(Timebase::NANOS));
        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };
        let unknown = PresentFeedback {
            missed_deadline: None,
            pacing_overrun: None,
            ..miss
        };

        sched.observe(&miss);
        sched.observe(&miss);
        for _ in 0..40 {
            sched.observe(&unknown);
        }
        assert_eq!(sched.pipeline_depth(), 1);
        sched.observe(&miss);
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn pacing_only_unknown_feedback_does_not_raise_depth() {
        let config = SchedulerConfig::pacing_only(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let unknown = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: None,
            pacing_overrun: None,
        };

        for _ in 0..8 {
            sched.observe(&unknown);
        }

        assert_eq!(sched.pipeline_depth(), 1);
        assert!(
            sched.safety_margin_ticks() > 0,
            "unknown deadline feedback should still train build-cost EMA"
        );
    }

    #[test]
    fn depth_clamped_at_max() {
        let mut config = SchedulerConfig::predictive(Timebase::NANOS);
        config.max_depth = 3;
        config.degradation_policy = DegradationPolicy::Adaptive {
            miss_threshold: 1,
            miss_window: 30,
            recovery_threshold: 10,
        };
        let mut sched = Scheduler::new(config);
        assert_eq!(sched.pipeline_depth(), 1);

        let miss = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(1500),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(true),
            pacing_overrun: None,
        };

        // Many consecutive misses should not exceed max_depth.
        for _ in 0..20 {
            sched.observe(&miss);
        }
        assert_eq!(sched.pipeline_depth(), 3, "depth should be clamped at max");
    }

    #[test]
    fn build_cost_ema_updates() {
        let config = SchedulerConfig::predictive(Timebase::NANOS);
        let mut sched = Scheduler::new(config);
        assert_eq!(sched.safety_margin_ticks(), 0);

        let feedback = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: Some(false),
            pacing_overrun: None,
        };

        sched.observe(&feedback);
        assert!(
            sched.safety_margin_ticks() > 0,
            "safety margin should increase after observing build cost"
        );
    }

    #[test]
    fn pacing_overrun_raises_depth_more_conservatively() {
        let config = SchedulerConfig::pacing_only(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let overrun = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: None,
            pacing_overrun: Some(true),
        };

        for _ in 0..5 {
            sched.observe(&overrun);
        }
        assert_eq!(sched.pipeline_depth(), 1);

        sched.observe(&overrun);
        assert_eq!(sched.pipeline_depth(), 2);
    }

    #[test]
    fn pacing_overruns_count_across_clear_ticks() {
        let config = SchedulerConfig::pacing_only(Timebase::NANOS);
        let mut sched = Scheduler::new(config);

        let overrun = PresentFeedback {
            present_latency: Duration::ZERO,
            pipeline_depth: None,
            work: None,
            submitted_at: HostTime(2_000),
            commit_deadline: HostTime(u64::MAX),
            build_start: HostTime(1_000),
            starts_at_tick: false,
            expected_present: None,
            actual_present: None,
            missed_deadline: None,
            pacing_overrun: Some(true),
        };
        let clear = PresentFeedback {
            pacing_overrun: Some(false),
            ..overrun
        };

        for _ in 0..3 {
            sched.observe(&overrun);
        }
        sched.observe(&clear);
        for _ in 0..2 {
            sched.observe(&overrun);
        }
        assert_eq!(sched.pipeline_depth(), 1);
        sched.observe(&overrun);
        assert_eq!(sched.pipeline_depth(), 2);
    }
}
