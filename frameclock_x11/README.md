<div align="center">

# Frameclock X11

**X11 timing adapters for `frameclock`.**

</div>

`frameclock_x11` connects X11 frame timing to `frameclock`, built on the
Present extension with no pre-Present fallbacks. A `PresentCompleteNotify`
event carries both a vblank tick and its `ust`/`msc` (unadjusted system time
in microseconds, and the media stream counter); this crate turns those events
into `frameclock::FrameTick` values and carries per-pixmap Present feedback as
`PresentEvent` values.

The crate owns only the timing bookkeeping. It references no `x11rb` type and
performs no protocol I/O: a host or backend decodes Present events and feeds
the `ust`/`msc` values in.

## Core Flow

```text
PresentNotifyMSC request       -> PresentTicker (claims the in-flight slot)
PresentCompleteNotify (msc)    -> PresentTicker -> FrameTick
PresentCompleteNotify (pixmap) -> PresentEvent  -> PresentEventQueue
```

See the crate-level documentation for the `ust` clock domain, the
`PresentNotifyMSC` arming loop, and how pacing behaves while a swapchain owner
(for example wgpu/Vulkan) presents.

Vblank notifications provide pacing and cadence, and leave
`FrameTick::prev_actual_present` unset. They do not prove that an application
frame was displayed. Pixmap completion events preserve their raw completion
mode; `PresentEvent::actual_present()` returns a timestamp only for known
presentation modes. Skipped and unknown completions require separate handling.

UST conversion assumes a local Linux X server using the same
`CLOCK_MONOTONIC` domain as the client. Remote servers and other clock domains
need external clock synchronization.

## A Host Loop

This shape paces a window whose frames a swapchain owner, such as
wgpu/Vulkan, presents. Protocol requests and event decoding are the host's;
they appear here as comments.

```rust,ignore
use frameclock::{FrameBeginResult, FrameDemand, FrameDriver, FrameSubmission};
use frameclock_x11::{Clock, PresentTicker};

let mut driver = FrameDriver::new(frameclock_x11::scheduler_config());
let mut ticker = PresentTicker::new();
// 60 Hz, refined from the ticks once two completions establish the cadence.
let display_timing = frameclock_x11::default_display_timing();

// When the app has visual work, arm a notify for the next vblank.
driver.request(FrameDemand::ANIMATION);
if ticker.mark_notify_requested() {
    // Send PresentNotifyMSC(window, serial, 0, 1, 0).
}

// When the matching PresentCompleteNotify of kind NotifyMSC arrives with its
// `ust` and `msc`:
ticker.on_complete_notify(Clock::Monotonic, ust, msc);

// After dispatch, plan frames for the queued ticks.
while let Some(tick) = ticker.poll_tick() {
    let opportunity = frameclock_x11::frame_opportunity(tick, display_timing);
    match driver.begin_frame(opportunity).result {
        FrameBeginResult::Ready(frame) => {
            // Render for frame.sample_time() and present through the swapchain.
            let submit = driver.submit_frame(
                frame,
                FrameSubmission::new(frameclock_x11::now(), None),
            );
            _ = submit.summary;
        }
        // The cadence starts this frame at a later vblank.
        FrameBeginResult::Wait(_) => {}
        FrameBeginResult::Expired(summary) => {
            // Record the dropped-frame summary.
            _ = summary;
        }
        FrameBeginResult::Idle => {}
    }
}

// Keep notifies coming while frame work waits for one.
if driver.needs_tick() && ticker.mark_notify_requested() {
    // Send PresentNotifyMSC(window, serial, 0, 1, 0).
}
```

Use `scheduler_config` for the `FrameDriver`: it is
`SchedulerConfig::estimated` in nanoseconds, matching the estimated hints
`frame_opportunity` builds once two completions have established a cadence.

The hints start each frame at a vblank notify, so the host is handed ready
frames there and never has to wake itself. A frame the cadence holds back
returns `Wait(FrameWait::NextTick)` and is ready at the later vblank it waits
for, so keep arming notifies while `FrameDriver::needs_tick` is true. A host
with its own timer can start frames nearer their deadline instead: mark the
hints `starting_when_planned`, wake at the time `Wait(FrameWait::Until(time))`
gives, and call `FrameDriver::begin_pending_frame`.

## Variable Refresh

The `DisplayTiming` passed to `frame_opportunity` describes the output the
tick was made for; rebuild it when the window moves to another output or the
mode changes. When the output is variable-refresh capable (RandR's
`vrr_capable` property) and the window has opted in with `_VARIABLE_REFRESH`,
pass `DisplayTiming::variable` with the output's range. Present does not
report the range, so read it from RandR's `EDID` output property or host
configuration. Variable timing is kept as is, since on such an output the
tick's refresh interval is only the last measured `msc` gap; fixed timing is
refined from the tick.

## Submitting Frames

When a swapchain owner presents, the host gets no per-frame feedback, so it
submits with `FrameSubmission::new(submitted_at, None)`, as above. A host that
presents its own pixmaps with `PresentPixmap` gets a completion for each one:
it submits with `FrameSubmission::reported`, keeps each pixmap's
`SubmissionId` with the frame's `FramePlan::frame_index`, and reports
`PresentEvent::outcome`, which also covers skipped pixmaps, through
`FrameDriver::report_frame`. The `frameclock` README's "Submitting Frames"
table summarizes the choice.

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
