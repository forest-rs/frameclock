<div align="center">

# Frameclock Wayland

**Wayland timing adapters for `frameclock`.**

</div>

`frameclock_wayland` connects Wayland frame timing to `frameclock`. It selects
and reads the compositor-aligned presentation clock as `HostTime`, converts
`wl_surface.frame` callback completions into `FrameTick` values, and carries
`wp_presentation` feedback facts as `PresentEvent` values for scheduler
feedback.

The crate intentionally does not own `wl_surface` objects, event queues,
buffers, registries, or protocol dispatch. Protocol I/O belongs to hosts and
backend crates such as `subduction_backend_wayland`; this crate owns the
timing bookkeeping those hosts feed and poll, plus the pure timing conversion
from `FrameTick` to `FrameOpportunity`.

## Core Flow

```text
wl_surface.frame done           -> TickerState -> FrameTick
wp_presentation_feedback events -> PresentEvent -> PresentEventQueue
wp_presentation.clock_id        -> Clock -> HostTime reads
```

Use `TickerState` as the frame-callback bookkeeping for one surface: call
`mark_callback_requested` to claim the single in-flight slot before sending a
`wl_surface.frame` request (it returns `false` if a callback is already in
flight), call `on_callback_done` when the matching `wl_callback.done` event
arrives, and drain resulting ticks with `poll_tick`. A `TickerState` models a single
paced surface/output stream — create one per `wl_surface`, pass it a stable
`OutputId`, and feed it only that surface's presentation feedback. Hosts that
multiplex several surfaces on one queue should keep a `TickerState` per stream
and correlate feedback to the right stream by `SubmissionId` themselves.

Use `presentation_time_to_host_time` to convert
`wp_presentation_feedback.presented` timestamps, store the most recent value
via `TickerState::set_last_observed_actual_present` so the next tick carries
`FrameTick::prev_actual_present` once, and queue per-commit `PresentEvent`s in a
`PresentEventQueue` correlated by `SubmissionId`.

Use `Clock::from_presentation_clock_id` to map the `wp_presentation.clock_id`
event to a `Clock`, and read all timing facts from that clock so feedback
timestamps and tick times stay in one time domain.

## A Host Loop

This shape uses `wp_presentation` feedback for each commit. Protocol requests
and event decoding are the host's; they appear here as comments.

```rust,ignore
use frameclock::{
    FrameBeginResult, FrameDemand, FrameDriver, FrameReport, FrameSubmission, OutputId,
};
use frameclock_wayland::{Clock, PresentEvent, PresentEventQueue, SubmissionId, TickerState};

let mut driver = FrameDriver::new(frameclock_wayland::scheduler_config());
let mut ticker = TickerState::new();
let mut feedback = PresentEventQueue::default();
// Switch to `Clock::from_presentation_clock_id` once `wp_presentation`
// reports its clock.
let clock = Clock::Monotonic;
// 60 Hz, refined from the ticks once feedback establishes the cadence.
let display_timing = frameclock_wayland::default_display_timing();
// Which frame each commit carried, so its feedback can be reported.
let mut in_flight: Vec<(SubmissionId, u64)> = Vec::new();
let mut next_commit = 0;

// When the app has visual work, ask for a frame callback.
driver.request(FrameDemand::ANIMATION);
if ticker.mark_callback_requested() {
    // Send wl_surface.frame.
}

// When that callback's wl_callback.done arrives:
ticker.on_callback_done(clock, OutputId(0));

// After dispatch, plan frames for the queued ticks.
while let Some(tick) = ticker.poll_tick() {
    let opportunity = frameclock_wayland::frame_opportunity(tick, display_timing);
    if let FrameBeginResult::Ready(frame) = driver.begin_frame(opportunity).result {
        // Render for frame.sample_time(), request wp_presentation feedback,
        // attach the buffer, and commit.
        let id = SubmissionId(next_commit);
        next_commit += 1;
        in_flight.push((id, frame.plan().frame_index));
        _ = driver.submit_frame(frame, FrameSubmission::reported(clock.now()));
    }
}

// Push each decoded wp_presentation_feedback event into `feedback`, then:
while let Some(event) = feedback.pop() {
    if let PresentEvent::Presented { actual_present, refresh_interval, .. } = event {
        ticker.set_last_observed_actual_present(actual_present);
        if let Some(interval) = refresh_interval {
            ticker.set_last_observed_refresh_interval(interval);
        }
    }
    if let Some(at) = in_flight.iter().position(|(id, _)| *id == event.id()) {
        let (_, frame_index) = in_flight.remove(at);
        let report = FrameReport { work: None, outcome: event.outcome() };
        if let Some(summary) = driver.report_frame(frame_index, report) {
            _ = summary;
        }
    }
}
```

Use `scheduler_config` for the `FrameDriver`: it is
`SchedulerConfig::estimated` in nanoseconds, matching the estimated hints
`frame_opportunity` builds once feedback has established a cadence.

## Submitting Frames

A host that requests `wp_presentation` feedback for each commit learns each
frame's fate later, possibly after later commits, so it submits with
`FrameSubmission::reported` and reports `PresentEvent::outcome` through
`FrameDriver::report_frame`, as above. Without `wp_presentation`, submit with
`FrameSubmission::new(submitted_at, None)`. The `frameclock` README's
"Submitting Frames" table summarizes the choice.

## Timing Model

`now`, `Clock::now`, and all converted timestamps are nanosecond ticks;
`timebase` returns the identity nanosecond `Timebase`.

Wayland frame callbacks carry no predicted present time or refresh interval,
so emitted `FrameTick`s are pacing-only facts. Actual presentation evidence
arrives separately through `wp_presentation` feedback: the previous frame's
actual present time is surfaced as `FrameTick::prev_actual_present`, and the
full per-commit event stream is available as `PresentEvent` values for hosts
that resolve feedback by `SubmissionId`.

Use `frame_opportunity` to pair a tick with Wayland presentation hints and
display timing before passing it to `FrameDriver` or `Scheduler`. Before
presentation feedback establishes a cadence this produces pacing-only timing;
after feedback supplies actual-present and refresh-interval facts through
`TickerState`, the opportunity carries estimated target-present timing.

The `DisplayTiming` passed to `frame_opportunity` describes the output the
tick was made for; rebuild it when the surface moves to another output or the
output's mode changes. When the compositor drives the output with adaptive
sync, pass `DisplayTiming::variable` with the output's range.
`wp_presentation` reports at most one rate for such an output, one the
compositor picks (such as the fastest) or none, and no standard protocol
reports the range, so read it from the output's EDID or the compositor's
configuration. Variable timing is kept as is; fixed timing is refined from the
tick.

When the compositor advertises `wp_presentation`, its `clock_id` event names
the clock domain of all feedback timestamps. Hosts should switch their reads
to that clock (`Clock::Presentation`) so `HostTime` comparisons remain valid.
`Clock::Monotonic` is the fallback when `wp_presentation` is missing or the
advertised clock is unknown.

Compositors stop delivering frame callbacks while a surface is occluded or
minimised, so the tick stream can stall. Hosts should treat tick starvation
as normal Wayland behaviour: idle, apply a timeout, or fall back to a
timer-based tick source.

## no_std

This crate keeps its implementation `no_std` (with `alloc`), but reading
clocks requires an operating system. It is validated on Linux targets instead
of the workspace's generic `x86_64-unknown-none` no-std target.

## Minimum Supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license,

at your option.
