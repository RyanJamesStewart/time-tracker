// Tiny shim. The real entry point is in src/windows_main.rs, gated to
// Windows only so non-Windows targets can compile the lib for `cargo
// test --lib --target x86_64-unknown-linux-gnu` without dragging in the
// winit/glutin/glow/egui_glow/tray-icon/global-hotkey stack.

// Release builds use the Windows GUI subsystem: no console window. This is a
// tray app - launching it (from the Start menu, autostart, or a terminal)
// must not pop a console that has to stay open. Debug builds keep the console
// so `cargo run` shows the eprintln! tracing during dev.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod windows_main;

fn main() {
    #[cfg(windows)]
    windows_main::run();

    #[cfg(not(windows))]
    {
        eprintln!("time-tracker is Windows-only.");
        eprintln!("Build with: cargo xwin build --release --target x86_64-pc-windows-msvc --bin time-tracker");
        std::process::exit(1);
    }
}
