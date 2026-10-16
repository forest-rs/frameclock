// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Small API 33 binding probe; the host owns callback registration and lifetime.

#![expect(
    unsafe_code,
    reason = "this example copies a live NDK callback payload through its accessor functions"
)]

use frameclock::HostTime;
use frameclock_android::{FrameTimeline, TimelineError};

/// Copies the callback-owned NDK fields into owned, binding-independent values.
///
/// # Safety
/// `data` must be the non-null payload of the currently executing API 33
/// Choreographer vsync callback, and the OS must provide those API 33 symbols.
pub(crate) unsafe fn copy_callback(
    data: *const ndk_sys::AChoreographerFrameCallbackData,
) -> Result<(HostTime, usize, Vec<FrameTimeline>), TimelineError> {
    // SAFETY: Each accessor borrows the same live callback payload described
    // by this function's contract; no native pointer escapes the copy.
    let (frame_time, preferred, count) = unsafe {
        (
            ndk_sys::AChoreographerFrameCallbackData_getFrameTimeNanos(data),
            ndk_sys::AChoreographerFrameCallbackData_getPreferredFrameTimelineIndex(data),
            ndk_sys::AChoreographerFrameCallbackData_getFrameTimelinesLength(data),
        )
    };
    let frame_time = HostTime(u64::try_from(frame_time).map_err(|_| TimelineError::InvalidData)?);
    let mut timelines = Vec::with_capacity(count);
    for index in 0..count {
        // SAFETY: index is within the length reported by the live payload.
        let timeline = unsafe {
            FrameTimeline::new(
                ndk_sys::AChoreographerFrameCallbackData_getFrameTimelineVsyncId(data, index),
                ndk_sys::AChoreographerFrameCallbackData_getFrameTimelineExpectedPresentationTimeNanos(data, index),
                ndk_sys::AChoreographerFrameCallbackData_getFrameTimelineDeadlineNanos(data, index),
            )
        }?;
        timelines.push(timeline);
    }
    Ok((frame_time, preferred, timelines))
}
