<div align="center">

# Frameclock

**Display-frame timing, scheduling, feedback, and diagnostics.**

</div>

`frameclock` is a small `no_std` crate for turning platform display callbacks,
display timing, and frame demand into explicit frame plans. Platform adapters
provide timing facts; hosts provide demand and display constraints;
`frameclock` decides when frame work should begin, which time a frame should
sample, what deadline it has, and how scheduler policy should adapt after
presentation feedback.

The crate intentionally does not own windows, event loops, layer trees,
renderers, swapchains, or platform presentation resources.

## Core Flow

```text
platform tick -> FrameOpportunity
              -> FrameDriver::begin_frame()
              -> FrameBegin { result: FrameBeginResult::Ready(ActiveFrame), ... }
              -> build frame
              -> FrameDriver::submit_frame() or FrameDriver::discard_frame()
              -> FrameTimingSummary
```

## A Host Loop

Start from your platform's adapter: `frameclock_apple`, `frameclock_windows`,
`frameclock_wayland`, `frameclock_x11`, or `frameclock_web`. Each turns its
platform's frame callbacks into `FrameTick`s and offers the same functions, so
host code can pick one with `cfg` and stay the same everywhere:

```rust,ignore
#[cfg(target_vendor = "apple")]
use frameclock_apple as platform;
#[cfg(target_arch = "wasm32")]
use frameclock_web as platform;
// ...and likewise for Windows, Wayland, or X11.

use frameclock::{FrameBeginResult, FrameDemand, FrameDriver, FrameSubmission};

let mut driver = FrameDriver::new(platform::scheduler_config());
let display_timing = platform::default_display_timing();

// When something on screen changes:
driver.request(FrameDemand::ANIMATION);

// In each frame callback, with the adapter's `FrameTick`:
let opportunity = platform::frame_opportunity(tick, display_timing);
match driver.begin_frame(opportunity).result {
    FrameBeginResult::Ready(frame) => {
        // Update and render for frame.sample_time(), then:
        let _ = driver.submit_frame(frame, FrameSubmission::new(platform::now(), None));
    }
    // The cadence starts this frame at a later callback.
    FrameBeginResult::Wait(_) => {}
    FrameBeginResult::Expired(_) | FrameBeginResult::Idle => {}
}
// Keep the callbacks coming while `driver.needs_tick()`.
```

Each adapter's README shows the full loop for its platform, including which
`FrameSubmission` fits its presentation evidence.

Every `HostTime` and `Duration` is in the adapter's host ticks: nanoseconds on
Linux, Mach ticks on Apple, QPC ticks on Windows, and microseconds on the web.
Write a duration from nanoseconds with
`Duration::from_nanos(nanos, platform::timebase())`. Debug builds panic on
display timing slower than 4 Hz, which is what a nanosecond value passed as
ticks usually looks like.

The adapters' opportunities start each frame at a tick, so a host that renders
from its frame callback never has to wake itself. A frame the cadence holds
back returns `Wait(FrameWait::NextTick)` and starts at the later callback it
waits for. A host with its own timer can start frames later instead, nearer
their deadline, so they sample fresher input: mark the hints
`starting_when_planned`, and the driver returns `Wait(FrameWait::Until(time))`.
Wake at that time and call `FrameDriver::begin_pending_frame`. The driver
learns how late the host's wakes come and starts later frames that much
earlier.

## Concepts

Use `FrameDemand` as the host-owned reason a frame is needed. Request
`INPUT` for discrete user action, `CONTINUOUS_INPUT` while scrolling/resizing or
dragging, `ANIMATION` while a visual timeline is active, and `BACKGROUND` for
deferrable visual work. With `FrameDriver`, call `request(demand)` when those
causes arrive; with the low-level scheduler, pass the demand to
`Scheduler::plan(opportunity, demand, frame_index)`.

Demand also remains attached to the selected `FramePlan`. Once
`FrameBeginResult::Ready` returns, use `frame.plan().demand` to choose the app's
workload for that frame: interactive frames can skip optional refinement,
animation frames can use normal visual work, and background frames can batch or
defer. The scheduler owns demand ordering; the app owns quality policy.

Use `DisplayTiming` to describe the current target output's fixed or variable
refresh constraints. It should come from the backend/platform facts for the
same output or surface as the `FrameTick`; refresh it when a window moves
between displays or the platform reports a changed display mode.

Use `FramePlan::frame_start` to arm an event-loop wake or request redraw when
the start time is already due. Use `FramePlan::sample_time` for animation and
simulation sampling. Use `FramePlan::target_present` only when code
specifically needs the predicted display time.

`FrameDemand::NONE` means there is no app-visible frame work to schedule.
Ordinary render loops should stay idle instead of calling `Scheduler::plan`
with empty demand. Passing `NONE` is reserved for hosts that intentionally want
a passive pacing plan for diagnostics or backend bookkeeping.
`FrameDemandClass` is the derived ordering/classification used by
`FrameDemand::dominant_class` and `FrameDemand::preempts`; it is mainly for
diagnostics and adapter policy that needs to match frameclock's demand order.

`FrameDriver` is the retained lifecycle helper for hosts that need to queue
demand and a future frame-start plan between event-loop turns. It owns pending
demand, stronger-demand preemption, queued `PlannedFrame`s, feedback
observation, and `FrameTimingSummary` construction. Hosts still own timers,
redraw requests, renderer submission, and native presentation resources. Use
`FrameDriver::next_frame_start` as one wake source to merge with app timers.
After submitting or discarding an `ActiveFrame`, hosts should request another
redraw when `FrameDriver::has_pending_demand()` is still true.

`FrameDriver` also owns the retained content-frame counter. It advances that
counter only when a planned frame becomes ready or expires, so queued plans
that are preempted or cleared do not leave diagnostic gaps. The reported
`FramePlan::frame_index` is used for submit/drop summaries. Low-level
`Scheduler` integrations own their own content-frame id and pass it explicitly
to `Scheduler::plan` and `FrameTickEvent::new`.

The lower-level `Scheduler` remains available for custom integrations. Event
structs and `FrameTimingSummaryBuilder` live under `frameclock::diagnostics`
for telemetry adapters and tests, but normal host code should not need to
assemble summary events by hand.

## API Surfaces

The root module re-exports the common retained-host integration surface:

- `FrameDriver`, `FrameOpportunity`, `ActiveFrame`, `FrameBegin`, and
  `FrameSubmission`
- `FrameDemand`
- `SchedulerConfig`
- `FrameTick`, `PresentHints`, and `DisplayTiming`
- `HostTime`, `Duration`, and `OutputId`
- `FrameTimingSummary`

Specialized APIs live under their modules:

- `frameclock::diagnostics` for event structs, `DiagnosticsSink`, and
  `FrameTimingSummaryBuilder`.
- `frameclock::scheduler` for `Scheduler`, `DegradationPolicy`, and adaptation
  state used by custom low-level integrations.
- `frameclock::timing` for presentation feedback, pending feedback, frame
  plans, and lower-level timing details.
- `frameclock::time` for `Timebase` and clock conversion.
- `frameclock::driver` and `frameclock::demand` for lower-level lifecycle and
  demand policy types.

Media timeline mapping for video PTS or audio-master clocks lives in the
`mediaclock` crate. A media host usually asks `frameclock` for a frame's sample
or target-present host time, then asks `mediaclock` which media time should be
visible then.

Without a platform adapter, such as in a test, a simulation, or a custom
backend, build the `FrameOpportunity` directly:

```rust,ignore
use frameclock::time::Timebase;
use frameclock::{
    Duration, FrameBeginResult, FrameDemand, FrameDriver, FrameOpportunity,
    FrameSubmission, FrameWait, HostTime, OutputId, SchedulerConfig,
};

// Host times here are nanoseconds. Pass the platform adapter's timebase
// instead, such as `frameclock_apple::timebase()`.
let timebase = Timebase::NANOS;
let mut driver = FrameDriver::new(SchedulerConfig::pacing_only(timebase));
driver.request(FrameDemand::ANIMATION);

let opportunity = FrameOpportunity::pacing_only(
    HostTime(1_000_000),
    Duration::from_nanos(16_666_667, timebase),
    OutputId(0),
);

let begin = driver.begin_frame(opportunity);
if let Some(summary) = begin.resolved_feedback {
    // A previous deferred submission resolved on this tick.
    _ = summary;
}

match begin.result {
    FrameBeginResult::Ready(frame) => {
        let sample_time = frame.sample_time();
        // Prepare app/model/render state for sample_time, then submit renderer
        // work. If the frame cannot be submitted, call `discard_frame` instead.
        let submit = driver.submit_frame(
            frame,
            FrameSubmission::new(HostTime(2_000_000), None),
        );
        _ = (sample_time, submit.summary);
    }
    FrameBeginResult::Wait(FrameWait::Until(frame_start)) => {
        // Wake at frame_start, then call `begin_pending_frame`.
        _ = frame_start;
    }
    FrameBeginResult::Wait(FrameWait::NextTick) => {
        // A later tick starts the frame.
    }
    FrameBeginResult::Expired(summary) => {
        // Record the dropped-frame summary and request fresh demand if needed.
        _ = summary;
    }
    FrameBeginResult::Idle => {}
}
```

## Submitting Frames

Submit each frame with the `FrameSubmission` that matches the presentation
evidence the platform gives you:

| The platform tells you | Submit with | The frame's summary arrives |
| --- | --- | --- |
| Nothing beyond your own commit | `FrameSubmission::new(submitted_at, None)` | From `submit_frame`, judged by commit timing |
| The present time on the next tick, as `FrameTick::prev_actual_present` | `FrameSubmission::deferred(submitted_at)` | In `FrameBegin::resolved_feedback` from a later `begin_frame` |
| Each frame's fate later, in any order | `FrameSubmission::reported(submitted_at)` | From `FrameDriver::report_frame` |

`deferred` suits a host whose submission reaches the screen at the next
refresh, so the next tick's `prev_actual_present` is that frame's present. Each
platform adapter's README says which row it fits.

`reported` suits a host that learns what became of a frame only after later
frames were submitted: a GPU renderer's completion and presented handlers,
compositor presentation events, swapchain statistics. As each frame's facts
arrive, call `FrameDriver::report_frame(frame_index, FrameReport { work,
outcome })`, with `frame_index` from the frame's `FramePlan`. `outcome` is a
`PresentOutcome`: presented at a host time, never presented, or unknown.
`work` is what the frame cost beyond the host's own build, such as how long its
GPU work took, if the platform can say; the scheduler takes the longer of it
and the host's span, so a frame whose GPU work outlasts the refresh interval
makes it choose a slower, steady cadence instead of alternating intervals. The
driver holds up to `REPORTED_FRAME_CAPACITY` frames, resolves each from its own
report in any order, and returns its summary from that call.

Whichever row fits, a frame that waits for a buffer to render into, in
`nextDrawable`, `dequeueBuffer`, or `vkAcquireNextImageKHR`, should say so:
`FrameSubmission::with_buffer_wait(requested, acquired)`, with the times before
and after the call. The wait is backpressure from frames queued ahead, not the
frame's work, so the scheduler leaves it out of build cost; counted as work, a
full presentation pipeline would look like slow frames, and the scheduler
would drop the cadence or start frames earlier for it. Nor does a frame the wait
made miss its deadline deepen the pipeline, which would only queue more. The
frame's summary carries the wait as a `BufferWait`.

## Display Timing And VRR

`DisplayTiming::fixed(interval)` is the right model when a backend has only a
current refresh interval, or when the platform does not expose direct control
over variable presentation timing.

Display timing is not app-global state. It belongs to the current target output
for a frame opportunity. If a window is dragged from a 60 Hz display to a 120 Hz
display, or if a platform display link reports a new interval/range, the next
`FrameOpportunity` should carry timing for that new output.

`DisplayTiming::variable(min_interval, max_interval, granularity)` describes a
display range. The `granularity` argument is intentionally conservative:

- `Some(step)` means the backend knows direct display intervals can be selected
  at that step.
- `None` means the backend knows a VRR range exists, but does not know the
  direct interval granularity or cannot request arbitrary direct presentation
  durations.

When granularity is unknown, `frameclock` chooses stable multiples of
`min_interval`, like fixed-rate pacing, instead of inventing arbitrary intervals
inside the VRR range. Platform adapters should pass an explicit granularity only
when the presentation API can honestly honor it.

## Diagnostics

`frameclock` exposes a neutral `DiagnosticsSink` trait and event structs for
ticks, plans, submits, feedback, scheduler state, and compact per-frame timing
summaries. Adapter crates can map those events to Spoor, Tracy, or other
instrumentation systems without adding those dependencies to the core crate.
`FrameTimingSummary::timing_basis` classifies whether each summary is based on
actual present feedback, predicted present timing, submission timing, or
pacing-only timing.

## Migration Notes

`frameclock` owns the timing pieces that previously lived under
`subduction_core::{clock, scheduler, time, timing}`. `subduction_core` keeps
compatibility re-exports for now, but new code should import these types from
`frameclock` directly.

The split also tightens names around timing semantics:

- `FramePlan::semantic_time` is now `FramePlan::sample_time`.
- `FramePlan::present_time` is now `FramePlan::target_present`.
- `FramePlan::frame_start` is now the scheduler-selected time to wake or start
  app-side frame work before `FramePlan::commit_deadline`.
- `Scheduler::plan` now takes a `FrameOpportunity`, `FrameDemand`, and explicit
  frame index so display timing facts, demand, and lifecycle identity remain
  explicit policy inputs.
- `FrameDemand::dominant_class` and `FrameDemand::preempts` expose the demand
  ordering used by the scheduler.
- `FrameDriver` owns pending demand and queued frame-start plans for hosts that
  need retained frame scheduling state. `PlannedFrame` retains the originating
  `FrameTick`, selected `FramePlan`, and matching `PresentHints`.
- Platform-named scheduler presets are now capability-named:
  `SchedulerConfig::predictive`, `SchedulerConfig::estimated`, and
  `SchedulerConfig::pacing_only`. Each takes the host clock's `Timebase`, so
  its default durations are the same length on every platform.

## Feature Flags

- `std`: reserved for future standard-library integrations. The current API is
  `no_std`.

## Minimum Supported Rust Version

This crate has been verified to compile with **Rust 1.88** and later.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license,

at your option.
