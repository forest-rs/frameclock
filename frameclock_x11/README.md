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
