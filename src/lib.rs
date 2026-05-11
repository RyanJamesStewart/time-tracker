// Pure-logic and Win32-cfg-gated modules live in the lib crate so
// `cargo test --lib --target x86_64-unknown-linux-gnu` runs the
// duration / autocomplete / csv_writer / timer suites from WSL,
// without dragging in the Windows-only GUI/IPC/tray dependency stack
// (winit + glutin + glow + egui_glow + tray-icon + global-hotkey).
//
// What stays in the binary (src/main.rs + src/popup.rs):
//   - the winit event loop owner
//   - the manual GL/egui rendering integration
//   - tray + hotkey wiring
// Those need a Windows target to compile and can't run unit tests on Linux.

pub mod autocomplete;
pub mod config;
pub mod crash;
pub mod csv_writer;
pub mod duration;
pub mod logging;
pub mod paths;
pub mod single_instance;
pub mod timer;
pub mod usage;
pub mod workstream;
