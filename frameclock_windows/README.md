<div align="center">

# Frameclock Windows

**Windows timing adapters for `frameclock`.**

</div>

`frameclock_windows` connects Windows frame timing to `frameclock`. It reads
the QPC (`QueryPerformanceCounter`) host clock as `HostTime` / `Timebase`,
builds `FrameTick` values for `VSync`-paced hosts, and builds
`FrameOpportunity` values for hosts that own `FrameDriver`.

The crate intentionally does not own `HWND`s, message loops, `DwmFlush`
pacing threads, or swapchains. Those belong to hosts and backend crates such as
`subduction_backend_windows`; this crate owns the clock reads and tick
bookkeeping those hosts feed and poll.

## Core Flow

```text
QueryPerformanceCounter / QueryPerformanceFrequency -> now / timebase -> HostTime
VSync-paced tick                                    -> make_tick      -> FrameTick
                                     -> frameclock_windows::frame_opportunity()
                                     -> FrameDriver::begin_frame()
                                     -> host render and present
                                     -> FrameDriver::submit_frame()
```

Call `make_tick` from a `VSync`-paced tick handler, for example one driven by
`DwmFlush` or a swapchain frame-latency waitable. It takes the output, the
refresh interval and the previous present time, all in QPC ticks.
`DWM_TIMING_INFO::qpcRefreshPeriod` is already in QPC ticks; convert a
nanosecond interval with `Duration::from_nanos(nanos, timebase())`. The
previous present anchors the prediction on the display's refresh grid; the QPC
time `DwmFlush` returned, or `DXGI_FRAME_STATISTICS::SyncQPCTime`, will do.

```rust,ignore
use frameclock::{
    DisplayTiming, Duration, FrameBeginResult, FrameDemand, FrameDriver, FrameSubmission,
    OutputId,
};

let mut driver = FrameDriver::new(frameclock_windows::scheduler_config());
// Every time and duration is in QPC ticks.
let refresh = Duration::from_nanos(16_666_667, frameclock_windows::timebase());
let display_timing = DisplayTiming::fixed(refresh);
let output = OutputId(0);
let mut last_present = None;

loop {
    driver.request(FrameDemand::ANIMATION);

    let tick = frameclock_windows::make_tick(output, Some(refresh), last_present);
    let begin = driver.begin_frame(frameclock_windows::frame_opportunity(tick, display_timing));
    match begin.result {
        FrameBeginResult::Ready(frame) => {
            // Render for frame.sample_time() and present the swapchain.
            let submit = driver.submit_frame(
                frame,
                FrameSubmission::new(frameclock_windows::now(), None),
            );
            _ = submit.summary;
        }
        FrameBeginResult::Wait(_) => {
            // The cadence starts this frame at a later vblank.
        }
        FrameBeginResult::Expired(summary) => {
            // Record the dropped-frame summary and request fresh demand if needed.
            _ = summary;
        }
        FrameBeginResult::Idle => {}
    }

    // Wait for the next vblank, for example with DwmFlush.
    last_present = Some(frameclock_windows::now());
}
```

Use `scheduler_config` for the `FrameDriver`: it is
`SchedulerConfig::estimated` in QPC ticks, matching the estimated hints that
`frame_opportunity` builds from `make_tick`'s grid predictions. Every duration
is in QPC ticks too: a raw `Duration(16_666_667)` would mean about 1.7 s, and
debug builds of the scheduler panic on display timing that slow. When the host
knows nothing about the output, `default_display_timing` is a fixed 60 Hz in
QPC ticks.

The hints start each frame at a tick, so the loop is handed ready frames and
never has to wake itself between vblanks; a frame the cadence holds back
returns `Wait(FrameWait::NextTick)` and is ready at the later vblank it waits
for. A host with its own timer can start frames nearer their deadline instead:
mark the hints `starting_when_planned`, wake at the time
`Wait(FrameWait::Until(time))` gives, and call
`FrameDriver::begin_pending_frame`.

## Variable Refresh

The `DisplayTiming` passed to `frame_opportunity` describes the output the
tick was made for; rebuild it when the window moves to another display or the
display mode changes. On a G-Sync, FreeSync or Windows dynamic-refresh output,
pass `DisplayTiming::variable` with the output's range. DXGI does not report
the range, so read it from the monitor's EDID (for example through
`DisplayMonitor::GetDescriptor`) or from host configuration. Variable timing
is kept as is, since on such an output the tick's refresh interval is only
the last observed gap; fixed timing is refined from the tick.

## Submitting Frames

A swapchain present tells the host nothing about when the frame was shown, so
submit with `FrameSubmission::new(submitted_at, None)`.

A flip-model swapchain's `IDXGISwapChain::GetFrameStatistics` says when its
last presented image reached the screen. Submit with
`FrameSubmission::reported(submitted_at)` and keep each frame's present count,
from `GetLastPresentCount` right after its `Present`, with its
`FramePlan::frame_index`. The statistics' `PresentCount` names the frame they
describe. Its present time is not `SyncQPCTime`, which is a sample taken at
v-blank `SyncRefreshCount`; the image reached the screen at v-blank
`PresentRefreshCount`, which can be an earlier one. Convert with
`present_time_from_statistics(PresentRefreshCount, SyncRefreshCount,
HostTime(SyncQPCTime), refresh)` and report
`PresentOutcome::Presented(present_time)` through `FrameDriver::report_frame`.
The conversion assumes every v-blank between the two counts lasted `refresh`.
On a variable-refresh output that does not hold, so report a present time only
when `PresentRefreshCount` equals `SyncRefreshCount`, and
`PresentOutcome::Unknown` otherwise. The statistics describe only the latest
image, so report any earlier frames they skipped over as
`PresentOutcome::Unknown` too. The `frameclock` README's "Submitting Frames"
table summarizes the choice.

## Timing Model

QPC frequency is read once and cached for the lifetime of the process via
`QueryPerformanceFrequency`. `now` reads `QueryPerformanceCounter` directly,
so all `HostTime` values are raw QPC ticks, not nanoseconds; convert with
`timebase` when nanosecond values are required
(`nanos = ticks * timebase.numer() / timebase.denom()`).

`frame_opportunity` commits a quarter refresh ahead of the predicted present by
default; use `frame_opportunity_with_commit_lead` when the host has a better
estimate.

## Minimum Supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license,

at your option.
