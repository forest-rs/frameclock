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
    Duration, FrameBeginResult, FrameDemand, FrameDriver, FrameSubmission, OutputId,
};

let mut driver = FrameDriver::new(frameclock_windows::scheduler_config());
// Every time and duration is in QPC ticks.
let refresh = Duration::from_nanos(16_666_667, frameclock_windows::timebase());
let output = OutputId(0);
let mut last_present = None;

loop {
    driver.request(FrameDemand::ANIMATION);

    let tick = frameclock_windows::make_tick(output, Some(refresh), last_present);
    let begin = driver.begin_frame(frameclock_windows::frame_opportunity(tick, refresh));
    match begin.result {
        FrameBeginResult::Ready(frame) => {
            // Render for frame.sample_time() and present the swapchain.
            let submit = driver.submit_frame(
                frame,
                FrameSubmission::new(frameclock_windows::now(), None),
            );
            _ = submit.summary;
        }
        FrameBeginResult::WaitUntil(frame_start) => {
            // Wait until frame_start, then call begin_frame again.
            _ = frame_start;
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
`frame_opportunity` builds from `make_tick`'s grid predictions.

## Submitting Frames

A swapchain present tells the host nothing about when the frame was shown, so
submit with `FrameSubmission::new(submitted_at, None)`. A host that reads
`IDXGISwapChain::GetFrameStatistics` can match each `PresentCount` to the frame
it presented: submit with `FrameSubmission::reported(submitted_at)` and report
`PresentOutcome::Presented(HostTime(SyncQPCTime))` through
`FrameDriver::report_frame`. The `frameclock` README's "Submitting Frames"
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
