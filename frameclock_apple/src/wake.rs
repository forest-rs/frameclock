// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Main-thread wakes at a host time.

use alloc::boxed::Box;
use core::ffi::c_void;

use dispatch2::{
    _dispatch_source_type_timer, DispatchObject, DispatchQueue, DispatchRetained, DispatchSource,
    DispatchTime,
};
use frameclock::HostTime;
use objc2::MainThreadMarker;

use crate::mach_time;

/// `DISPATCH_TIMER_STRICT`: fire on time instead of when convenient.
const TIMER_STRICT: usize = 0x1;

/// What a pending wake owns until it fires.
struct Wake {
    source: DispatchRetained<DispatchSource>,
    wake: Box<dyn FnOnce()>,
}

/// Runs `wake` on the main thread at host time `at`, or as soon as possible
/// if `at` has passed.
///
/// This is the wake a [`FrameWait::Until`](frameclock::FrameWait::Until)
/// asks for: call
/// [`FrameDriver::begin_pending_frame`](frameclock::FrameDriver::begin_pending_frame)
/// from `wake`. The closure runs on the main thread, so it can own
/// main-thread state such as an `Rc<RefCell<FrameDriver>>`, which
/// `DispatchQueue::after` cannot take.
///
/// It uses a strict Grand Central Dispatch timer with no leeway on the main
/// queue. On a 120 Hz Mac it fired a median of under 0.1 ms late, against
/// about a millisecond for `DispatchQueue::after` over a few milliseconds,
/// whose leeway grows with the delay. The driver learns whatever lateness
/// remains.
pub fn wake_at<F>(at: HostTime, mtm: MainThreadMarker, wake: F)
where
    F: FnOnce() + 'static,
{
    let _ = mtm;
    let timebase = mach_time::timebase();
    let delay = timebase.ticks_to_nanos(at.saturating_duration_since(mach_time::now()).ticks());
    let delay = i64::try_from(delay).unwrap_or(i64::MAX);

    // SAFETY: A timer source takes no handle, its mask is a valid timer
    // flag, and the main queue outlives it.
    let source = unsafe {
        DispatchSource::new(
            (&raw const _dispatch_source_type_timer).cast_mut(),
            0,
            TIMER_STRICT,
            Some(DispatchQueue::main()),
        )
    };
    let context = Box::into_raw(Box::new(Wake {
        source: source.clone(),
        wake: Box::new(wake),
    }));
    // SAFETY: `fire` takes the context back exactly once, on the main
    // queue, which is this thread, so the closure need not be `Send`.
    unsafe { source.set_context(context.cast::<c_void>()) };
    source.set_event_handler_f(fire);
    source.set_timer(DispatchTime::NOW.time(delay), u64::MAX, 0);
    source.activate();
}

extern "C" fn fire(context: *mut c_void) {
    // SAFETY: `wake_at` set this context to a boxed `Wake`, and a one-shot
    // timer cancelled here fires only once.
    let Wake { source, wake } = *unsafe { Box::from_raw(context.cast::<Wake>()) };
    source.cancel();
    wake();
}
