# Frameclock and Mediaclock

Display-frame scheduling and media timeline mapping for Rust applications.

| Crate | Responsibility |
| --- | --- |
| [`frameclock`](frameclock/) | Frame demand, scheduling, presentation feedback, and diagnostics |
| [`mediaclock`](mediaclock/) | Mapping host time into media timelines |
| [`frameclock_apple`](frameclock_apple/) | Apple display-link timing |
| [`frameclock_wayland`](frameclock_wayland/) | Wayland clock and presentation timing |
| [`frameclock_windows`](frameclock_windows/) | Windows performance-counter timing |
| [`frameclock_web`](frameclock_web/) | Browser animation-frame timing |
| [`frameclock_footfall`](frameclock_footfall/) | Frame timing observations for Footfall |

The core clocks work without `std`. Platform adapters provide timing facts;
applications own their event loops, rendering, and presentation surfaces.

Run the deterministic example with `cargo run -p frameclock_simulated`, or the
winit event-loop example with `cargo run -p winit_paced_redraw`.

The minimum supported Rust version is **1.88**. CI checks feature combinations,
native platforms, WebAssembly, `no_std`, documentation, spelling, and formatting.
On macOS, run:

```sh
cargo fmt --all --check
taplo fmt --check --diff
typos
bash .github/copyright.sh
cargo clippy --workspace --exclude frameclock_wayland --exclude frameclock_windows --all-targets --all-features -- -D warnings
cargo test --workspace --exclude frameclock_wayland --exclude frameclock_windows --all-features
cargo doc --workspace --exclude frameclock_wayland --exclude frameclock_windows --all-features --no-deps
```

On Linux, exclude `frameclock_apple` and `frameclock_windows`; on Windows, exclude
`frameclock_apple` and `frameclock_wayland`.

Licensed under Apache-2.0 OR MIT.
