<div align="center">

# Frameclock Apple

**Apple display-link timing adapters for `frameclock`.**

</div>

`frameclock_apple` connects Apple display-link callbacks to `frameclock`. It
converts `CADisplayLink` and `CVDisplayLink` timing into `FrameTick` values,
exposes Mach absolute time as `HostTime`, computes predictive present hints, and
builds `FrameOpportunity` values for hosts that own `FrameDriver`.

The crate intentionally does not own `CALayer` trees, `CAMetalLayer`
presentation, renderers, windows, app lifecycle, or event-loop policy.

## Core Flow

```text
CADisplayLink / CVDisplayLink -> FrameTick
                              -> frameclock_apple::frame_opportunity()
                              -> FrameDriver::begin_frame()
                              -> FrameBegin { result: FrameBeginResult::Ready(ActiveFrame), ... }
                              -> host render
                              -> FrameDriver::submit_frame() or FrameDriver::discard_frame()
                              -> FrameTimingSummary
```

Use `CaDisplayLink` or `CvDisplayLink` when an application wants this crate
to create an Apple display-link tick source. The default `ca-display-link`
feature exposes `CaDisplayLink`, a `CADisplayLink` wrapper that ticks on the
run loop of the thread that starts it: the main thread, or a render thread
of the host's. The `cv-display-link` feature exposes `CvDisplayLink`, a
legacy macOS `CVDisplayLink` wrapper, whose callback runs on the main thread
(`CvDisplayLink::new`) or on Core Video's own thread
(`CvDisplayLink::on_link_thread`). The callback can own the host's scheduler
state on whichever thread it runs; see
[Rendering Off the Main Thread](#rendering-off-the-main-thread).

Use `FrameDriver` when an application wants retained frame lifecycle state:
pending demand, queued frame-start plans, stronger-demand preemption, submission
summaries, and dropped-frame summaries. This crate supplies only Apple timing
facts. Hosts still decide when a frame is needed, what to render, when to
acquire native presentation resources, and how to submit to Core Animation or
Metal.

```rust,ignore
use frameclock::{FrameBeginResult, FrameDemand, FrameDriver, OutputId};
use frameclock_apple::CaDisplayLink;

// The display-link callback runs on this thread and may own the driver.
let mut driver = FrameDriver::new(frameclock_apple::scheduler_config());
// 60 Hz in Mach ticks, refined from each tick's refresh interval.
let display_timing = frameclock_apple::default_display_timing();
let output = OutputId(0);

let display_link = CaDisplayLink::new(
    move |tick| {
        driver.request(FrameDemand::ANIMATION);

        let opportunity = frameclock_apple::frame_opportunity(tick, display_timing);
        let begin = driver.begin_frame(opportunity);
        if let Some(summary) = begin.resolved_feedback {
            // Previous deferred submit resolved with this tick's actual-present fact.
            _ = summary;
        }

        match begin.result {
            FrameBeginResult::Ready(frame) => {
                let sample_time = frame.sample_time();
                // Prepare and submit Apple rendering work for sample_time.
                let submit = driver.submit_frame(
                    frame,
                    CaDisplayLink::FEEDBACK_MODE.submission(frameclock_apple::now()),
                );
                _ = (sample_time, submit.awaiting_actual_present);
            }
            FrameBeginResult::Wait(_) => {
                // The cadence starts this frame at a later tick.
            }
            FrameBeginResult::Expired(summary) => {
                // Record the dropped-frame summary and request fresh demand if needed.
                _ = summary;
            }
            FrameBeginResult::Idle => {}
        }
    },
    output,
);

display_link.start();
```

## API Surfaces

The root module exposes the Apple integration surface:

- `CaDisplayLink`, `CvDisplayLink`, and `CaMetalDisplayLink` with its
  `MetalTick` for the enabled display-link sources.
- `now` and `timebase` for Mach host-time conversion.
- `wake_at` for a main-thread wake at a host time, for hosts that start
  frames when planned.
- `scheduler_config` and `default_display_timing` for the `SchedulerConfig`
  and fallback `DisplayTiming` that match these ticks.
- `present_hints`, `display_timing`, and `frame_opportunity` for hosts that own
  `FrameDriver` directly.
- `AppleFeedbackMode`, and each display link's `FEEDBACK_MODE`, for turning
  that source's presentation-feedback facts into `FrameSubmission`.
- `preferred_frame_rate_range`, `preferred_frame_rate_range_for_frame`, and
  `PreferredFrameRateRange` for translating a selected frame interval into a
  Core Animation-style ProMotion cadence request.
- `CvDisplayLinkError` with the `cv-display-link` feature on macOS, and
  `CaMetalDisplayLinkError` with the `metal-display-link` feature.
- `presented_outcome` for turning a Metal drawable's `presentedTime` into a
  `PresentOutcome`.

`frameclock_apple` keeps Apple FFI and thread-model details out of
`frameclock` proper. Core scheduling policy, frame demand ordering, frame
summaries, and diagnostics stay in `frameclock`.

## Timing Model

`now` and emitted `FrameTick` values use Mach absolute time ticks. Use
`timebase` when a host needs to convert those ticks to nanoseconds for logs,
tracing, or external diagnostics.

`CADisplayLink` ticks carry `targetTimestamp` as `predicted_present`,
`duration` as `refresh_interval`, and the previous callback's `timestamp` as
`prev_actual_present`, except on the first tick after the link is created,
paused, or stopped. `CVDisplayLink` ticks carry the output host time as
`predicted_present` and the display's refresh period as `refresh_interval`.
For `CvDisplayLink::new`, `now` is read when the tick reaches the main
thread, so time spent waiting for a busy main thread is not counted as time
the host still has, and a tick overtaken by a newer one while it waits is
dropped. For `CvDisplayLink::on_link_thread`, it is read on Core Video's
thread as the tick arrives.

Every `Duration` and `DisplayTiming` passed alongside these ticks is in Mach
ticks too. Convert nanosecond values with `Duration::from_nanos(nanos,
frameclock_apple::timebase())`; on Apple silicon one Mach tick is about 41.7
ns, so a raw `Duration(16_666_667)` would mean about 694 ms. Debug builds of
the scheduler panic on display timing that slow.

`frame_opportunity` computes predictive `PresentHints` from the display-link
prediction when it is fresh. The adapter subtracts a small platform commit lead
from the predicted present time to form `latest_commit`; by default this is one
quarter of the tick's refresh interval. Use
`frame_opportunity_with_commit_lead` when the host has a better commit lead
estimate. If a display-link callback arrives after its predicted present time,
the stale prediction is ignored and the frame is planned with pacing-only hints
from the callback time. Scheduler safety margin remains inside `frameclock`
planning; it is not baked into Apple platform hints.

The hints start each frame at a display-link tick, so the callback is handed
ready frames and never has to wake itself. A frame the cadence holds back, such
as background work every other refresh, returns `Wait(FrameWait::NextTick)` and
is ready at the later tick it waits for. Pause the display link while
`FrameDriver::needs_tick` is false.

A host can start frames nearer their deadline instead, so they sample fresher
input, by waking itself. Mark the opportunity's hints `starting_when_planned`,
and wake with `wake_at` at the time `Wait(FrameWait::Until(time))` gives:

```rust,ignore
use std::{cell::RefCell, rc::Rc};

use frameclock::{FrameBeginResult, FrameWait};
use frameclock_apple::CaDisplayLink;

// `driver` is an `Rc<RefCell<FrameDriver>>` shared with the wake. Take each
// result out of the borrow before matching on it, so the arms can borrow the
// driver again.
let mut opportunity = frameclock_apple::frame_opportunity(tick, display_timing);
opportunity.hints = opportunity.hints.starting_when_planned();
let result = driver.borrow_mut().begin_frame(opportunity).result;
if let FrameBeginResult::Wait(FrameWait::Until(at)) = result {
    let driver = Rc::clone(&driver);
    frameclock_apple::wake_at(at, mtm, move || {
        let now = frameclock_apple::now();
        let result = driver.borrow_mut().begin_pending_frame(now);
        if let FrameBeginResult::Ready(frame) = result {
            // Render for frame.sample_time(), then submit it.
            let submission = CaDisplayLink::FEEDBACK_MODE.submission(frameclock_apple::now());
            let _ = driver.borrow_mut().submit_frame(frame, submission);
        }
    });
}
```

`wake_at` runs its closure on the main thread, so the closure can own
main-thread state, which `DispatchQueue::after` does not allow. A host
rendering on a thread of its own keeps the default hints, which start frames
at its link's ticks. `wake_at` uses a strict dispatch timer with no leeway: on a 120 Hz Mac it fired a median of
under 0.1 ms late, where `DispatchQueue::after` fired about a millisecond late
over a few milliseconds. The driver learns how late wakes come and starts
frames that much earlier; run live, frames started about 5 ms after each 120 Hz
tick and none of 120 expired.

Presentation feedback lifecycle is host-owned, while feedback capability comes
from the Apple display-link source. Use the source's `FEEDBACK_MODE`, such as
`CaDisplayLink::FEEDBACK_MODE.submission(frameclock_apple::now())`, when
submitting a frame. With `CADisplayLink`, the submission waits for the next
callback because `CADisplayLink.timestamp` reports the previous callback's
actual display time; the next `FrameDriver::begin_frame` returns the completed
`FrameTimingSummary` in `FrameBegin::resolved_feedback` when the tick carries
`prev_actual_present`. With `CVDisplayLink`, the submission completes at commit
time because this adapter does not synthesize actual-present timestamps from
`CVDisplayLink` ticks.

That `CADisplayLink` feedback is right for Core Animation transactions, which
reach the screen at the next refresh. A Metal renderer's frame is shown after
its GPU work, often several refreshes later: submit it with
`AppleFeedbackMode::Reported`, which is `FrameSubmission::reported`, and report
each drawable's `presentedTime`, converted with `presented_outcome`, through
`FrameDriver::report_frame`. The `frameclock` README's "Submitting Frames"
table summarizes the choice.

Display timing belongs to the output that produced the tick. Hosts should
refresh output identity and target-output `DisplayTiming` when a window moves
between displays or the platform reports a different display mode. For
ProMotion/VRR displays, pass `DisplayTiming::variable` for the current output.
On macOS, `NSScreen`'s `minimumRefreshInterval`, `maximumRefreshInterval` and
`displayUpdateGranularity` give the range and, when it is non-zero, its step,
in seconds; convert them to Mach ticks with `Duration::from_nanos`. Then use
`preferred_frame_rate_range_for_frame`, `preferred_frame_rate_range`, or
`CaDisplayLink::set_preferred_frame_interval` to translate a planned
`FramePlan::frame_interval` into a Core Animation frame-rate range.

## Choosing a Display Link

On a 120 Hz Mac, rendering 1 ms of work into a small window, the links
reached the screen this long after their callbacks:

| Link | Callback to on screen | Why |
|---|---|---|
| `CaDisplayLink`, presenting `nextDrawable` at once | 25 ms, 3 refreshes | the lowest |
| `CvDisplayLink`, the same way | 20 ms, or 28 or 37 ms with frames queued | see below |
| `CaMetalDisplayLink` | 41.5 ms, 5 refreshes | planned by macOS |

For an interactive window, use `CaDisplayLink` or `CvDisplayLink` and present
as soon as the frame is rendered. `CaMetalDisplayLink` hands each update its
drawable, a hard deadline, and a target, but in a window macOS plans that
target five refreshes ahead whatever `preferred_frame_latency` asks for. The
layer's setup, layer-hosting or layer-backed and at backing scale or not, made
no difference.

`CvDisplayLink`'s frames reach the screen soonest, 20 ms after its callbacks,
but its first frames are often queued behind one another, at 28 or 37 ms, and
stay there. Reported as below, two of three runs drained to 20 ms. A run
queued from its first frame never sees the lower latency, so nothing tells
the scheduler there is a queue to drain.

On the `CaDisplayLink` path, a present that misses the compositor's cutoff
leaves a frame queued, and every later frame is shown a refresh later until
one is skipped. Report each drawable's `presentedTime` with
`AppleFeedbackMode::Reported`, as the Metal display link's loop below does,
and the scheduler sees this and holds one frame back to drain it
(`SchedulerConfig::queue_drain_after`). With a 9 ms spike every 200 frames,
69 to 84% of frames were shown at 25 ms with draining and 14 to 27% without.
`CaDisplayLink::FEEDBACK_MODE` resolves frames from the link's own
timestamps instead, which miss the queue.

Presenting faster than the display refreshes, such as from a timer, fills the
drawable queue the same way: frames took 33 to 40 ms.

`nextDrawable` blocks when every drawable is in use. Read the time before and
after it and pass both with `FrameSubmission::with_buffer_wait`, so the
scheduler does not count the wait as the frame's work.

## Rendering Off the Main Thread

A display link on the main thread ticks between the application's event
handling, layout, and anything else the main thread does, and a tick that
arrives while the main thread is busy waits for it or is dropped. A host that
renders on a thread of its own avoids that:

- Create and start a `CaDisplayLink` on the render thread, and run that
  thread's run loop, such as with `NSRunLoop::currentRunLoop().run()`. The
  link ticks there. It is not `Send`, so it and its callback stay on that
  thread.
- Or make a `CvDisplayLink` with `on_link_thread`: its callback, which must be
  `Send`, runs on Core Video's thread as each tick arrives.

Either way, the callback owns the `FrameDriver` and the renderer, and presents
from that thread. Give a render thread of the host's user-interactive quality
of service, with `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE,
0)`. With the main thread busy as below, two of five runs on a render thread
without it lost frames or stayed a refresh late, and none of eight with it.

On a 120 Hz Mac, rendering 1 ms of work into a small window for four seconds,
with each drawable's `presentedTime` reported and the main thread busy for 12
ms of every 50:

| Link | Refreshes skipped on screen | Callback to on screen |
|---|---|---|
| `CaDisplayLink` on the main thread | 80 to 151 of 480 | 25 ms |
| `CaDisplayLink` on a render thread | 0 to 2 | 25 ms |
| `CvDisplayLink::new` | 1 to 64 | 20 to 28 ms |
| `CvDisplayLink::on_link_thread` | 1 to 9 | 20 ms |

With the main thread idle, every link skipped 9 refreshes or fewer. A render
thread does not help a frame that blocks it, such as in `nextDrawable`: report
that wait with `FrameSubmission::with_buffer_wait`.

## Metal Display Link

With the `metal-display-link` feature, `CaMetalDisplayLink` drives a
`CAMetalLayer` (macOS 14, iOS 17, or later; `new` returns
`CaMetalDisplayLinkError::Unavailable` before that). Each update arrives as a
`MetalTick` with the drawable to render into, and builds its own
`FrameOpportunity`: the update's `targetPresentationTimestamp` is the frame's
predicted present, its `targetTimestamp` is the deadline to call `present` by,
and frames start at the update, since the drawable arrives with it.

The link calls back on the run loop of the thread that starts it, so a render
thread can own the driver and the renderer. Its frames are shown after their
GPU work, so each one is reported: submit with `CaMetalDisplayLink::FEEDBACK_MODE`
and report the drawable's `presentedTime` through `FrameDriver::report_frame`,
converted with `presented_outcome`. Metal calls presented handlers on its own
thread, so send each frame's presented time back to the thread that owns the
driver:

```rust,ignore
use std::ptr::NonNull;
use std::sync::mpsc;

use block2::RcBlock;
use frameclock::{FrameBeginResult, FrameDemand, FrameDriver, FrameReport, OutputId};
use frameclock_apple::CaMetalDisplayLink;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCommandBuffer, MTLCommandQueue, MTLDrawable};

// `layer` is the view's `CAMetalLayer`, and `queue` a command queue on its
// device.
let mut driver = FrameDriver::new(frameclock_apple::scheduler_config());
let display_timing = frameclock_apple::default_display_timing();
let (presented_tx, presented_rx) = mpsc::channel::<(u64, f64)>();

let link = CaMetalDisplayLink::new(
    &layer,
    move |update| {
        // Report the frames whose presented handlers have run.
        for (frame_index, presented_time) in presented_rx.try_iter() {
            let outcome = frameclock_apple::presented_outcome(presented_time);
            let _ = driver.report_frame(frame_index, FrameReport { work: None, outcome });
        }

        driver.request(FrameDemand::ANIMATION);
        let begin = driver.begin_frame(update.frame_opportunity(display_timing));
        if let FrameBeginResult::Ready(frame) = begin.result {
            let frame_index = frame.plan().frame_index;
            let drawable = update.drawable();
            let presented_tx = presented_tx.clone();
            let presented = RcBlock::new(move |drawable: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                let time = unsafe { drawable.as_ref() }.presentedTime();
                let _ = presented_tx.send((frame_index, time));
            });
            unsafe { drawable.addPresentedHandler(RcBlock::as_ptr(&presented)) };

            // Encode rendering into drawable.texture() for frame.sample_time().
            let commands = queue.commandBuffer().unwrap();
            commands.presentDrawable(ProtocolObject::from_ref(&*drawable));
            commands.commit();
            let submission = CaMetalDisplayLink::FEEDBACK_MODE.submission(CaMetalDisplayLink::now());
            let _ = driver.submit_frame(frame, submission);
        }
    },
    OutputId(0),
)?;
link.start();
```

Present with plain `present` or `presentDrawable`: Core Animation asserts on
the timed variants, such as `presentDrawable:atTime:`, for this link's
drawables. The update's deadline is hard: pipeline depth never extends it, it
holds even when the update's target is stale, and an update that arrives after
its deadline returns `Expired`. Leave that update's
drawable unpresented, as for a frame the cadence holds back, which returns
`Wait(FrameWait::NextTick)`. Run live on a 120 Hz Mac for two
seconds, this loop drew every update with none expired, every frame was
committed before its deadline, and frames were shown within a fraction of a
millisecond of their target; background demand drew every other update, and
the link kept ticking past the drawables it skipped.

`preferred_frame_latency` asks for how many frames ahead each update comes,
but macOS may use more. On that Mac it made no difference at 1 or 2: in a
window the target came about 41 ms, five refreshes, after each update, and in
full screen about 16 ms, two refreshes. Either way frames were shown within
0.01 ms of the link's target, so frameclock learned no extra present latency.

## Feature Flags

- `ca-display-link`: enables `CADisplayLink` support and is enabled by
  default.
- `cv-display-link`: enables legacy `CVDisplayLink` support on macOS, for
  hosts that need a Core Video display link. Its ticks are forwarded from the
  Core Video thread to the main thread, or run on Core Video's thread.
- `metal-display-link`: enables `CAMetalDisplayLink` support, for Metal
  renderers that want each frame's drawable, deadline, and target together.

The features are independent, so enabling one never removes another's types.

This crate keeps its own implementation `no_std`, but the selected Objective-C
framework bindings currently require `std`. It is validated on supported Apple
targets instead of the workspace's generic `x86_64-unknown-none` no-std target.

## Minimum Supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license,

at your option.
