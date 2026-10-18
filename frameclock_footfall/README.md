<div align="center">

# Frameclock Footfall

**Records `frameclock`'s frame timing in Footfall traces.**

</div>

`frameclock_footfall` turns what a `FrameDriver` learns about each frame into
[Footfall](https://github.com/forest-rs/footfall) observations. A trace can
then say which frames missed the display, by how much, and what their work did
differently.

The crate is `no_std` and holds no state beyond the platform's timebase.
Without an installed Footfall backend every call does nothing.

## Core Flow

```text
platform tick       -> Recorder::sync(now)
FrameOpportunity    -> Recorder::opportunity(&opportunity)
                    -> FrameDriver::begin_frame(opportunity)
FrameBeginResult::Ready(frame)
                    -> Recorder::begin_frame(&frame)
                    -> build frame
                    -> Recorder::end_frame(&frame)
                    -> FrameDriver::submit_frame() / report_frame() / ...
FrameTimingSummary  -> Recorder::record(&summary)
```

Record every opportunity the platform offers, including those at which no
frame starts. A frame whose hints start it at a tick is recorded as
scheduled to begin at the first opportunity from its `frame_start`, so
Footfall can tell a frame the program started late from one whose tick the
platform skipped, and count the ticks skipped.

Pass every summary the driver returns, including the ones that arrive frames
later from `report_frame` or `FrameBegin::resolved_feedback`. Frame points
carry the frame's number, so they find their frame whenever they arrive. The
summary is the only source of frame points: the plan's deadline and target are
known at begin, but recording them then would only duplicate it.

## What Is Recorded

| `FrameTimingSummary` | Footfall |
| --- | --- |
| `frame_index` | the number on the frame markers and every frame point, in the recorder's `FrameStream` |
| `frame_start`, `starts_at_tick` | `FramePointKind::Scheduled`, beginning at `BeginsAt::Opportunity` for a frame that starts at a tick, otherwise `BeginsAt::Time` |
| `predicted_present` | `FramePointKind::Predicted`, with a `presentation_timing` arg |
| `commit_deadline` | `FramePointKind::Deadline` |
| `target_present` | `FramePointKind::Target` |
| `sample_time` | `FramePointKind::Sampled` |
| `submitted_at` | `FramePointKind::Submitted`, observed, with a `work_ns` arg when reported |
| `expected_present` | `FramePointKind::Expected` |
| `actual_present` | `FramePointKind::Presented`, observed for `ActualPresent` and estimated otherwise |
| `drop_reason` | `FramePointKind::Dropped`, with the matching `DropReason` |
| `frame_interval`, `pipeline_depth`, `present_latency`, `held_back`, `synchronous` | a `FramePlan`: its interval, depth, latency allowance, a `HoldReason::DrainQueue` hold, and a synchronous kind, with `safety_margin_ns` as an arg |
| `work` | also the `frameclock_submission_work` histogram, in nanoseconds |

The plan carries the frame's number, rather than living in gauges set when the
summary arrives, which may be frames later, so it describes the frame it was
planned for. Footfall's analysis judges the frame by it: the cadence it meant,
a frame held back to drain the queue, a resize frame presented outside pacing,
and the depth or allowance behind a lasting change in latency.
`FramePacing::plan` returns it. A synchronous frame records no prediction,
deadline, or target, since it was not paced.

A program with several frame drivers, such as one per window, gives each
recorder its own stream with `Recorder::with_stream`, and a recreated driver,
whose frame indices start again, a new generation of it.

Every drop is recorded with its reason, including frames the host discarded
or superseded itself. Footfall's analysis keeps those out of its miss counts,
while still counting the work they wasted. `NotPresented` stays
`DropReason::NotPresented`, a miss: frameclock does not say whether a later
frame replaced the frame or it was lost.

Frame points stay in host ticks under `HOST_CLOCK`, with the platform
adapter's timebase as the tick period. Footfall's analysis converts them
through the clock syncs `Recorder::sync` records, and `Trace::pacing` judges
each frame on time, late, dropped, skipped, or submitted late from them.
`Pacing::prediction_error` compares the predicted, target, and expected times
with when frames were shown.

## Footfall Version

This crate uses Footfall from git, pinned to a commit with the changes that will
be Footfall 0.2, until that release is published.

## Minimum Supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.

## License

Licensed under either of

- Apache License, Version 2.0, or
- MIT license,

at your option.
