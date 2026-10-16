// Copyright 2026 the Frameclock Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Live, visible `MoltenVK` GOOGLE presentation-timing probe.
//!
//! Run `cargo run -p platform_timing_probe --bin moltenvk -- /path/to/libMoltenVK.dylib`.
//! The default path is `/opt/homebrew/lib/libMoltenVK.dylib`. This opens a
//! fixed-size window, submits timed color clears, correlates real reports
//! through frameclock, prints results, and exits. No hardware is needed for
//! ordinary workspace tests; this binary must be run explicitly on macOS.

#[cfg(target_os = "macos")]
#[path = "../moltenvk.rs"]
mod macos;

fn main() {
    #[cfg(target_os = "macos")]
    macos::run();
    #[cfg(not(target_os = "macos"))]
    eprintln!("The live MoltenVK probe requires macOS.");
}
