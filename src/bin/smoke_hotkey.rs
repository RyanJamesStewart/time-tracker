// SPEC.md §7 item 0 - foundational smoke test.
//
// Verifies that `global-hotkey` registers Ctrl+Shift+H against the Win32
// hotkey subsystem and fires events via the `tao` event loop. This is the
// architectural assumption the rest of Day 1 depends on; if it doesn't
// work, we find out before any UI / MSIX / packaging work.
//
// Hotkey: Ctrl+Shift+H (matches SPEC §3.1 quick-entry default)
//
// Run (Windows side):
//     cargo run --bin smoke-hotkey --release
// or directly:
//     target\x86_64-pc-windows-msvc\release\smoke-hotkey.exe
//
// Then press Ctrl+Shift+H. Expect: a printed line + Windows asterisk beep
// per press. Ctrl+C in the terminal to exit cleanly (handled below).
//
// Item 0 graduates to PASS only after this also works inside an MSIX-
// packaged shell with `runFullTrust` (item 0b - pending manifest work).

// Windows-only — global-hotkey + winit live in [target.'cfg(windows)']
// dependencies. On Linux we get a stub `main` so `cargo build --bins` still
// works for cross-cutting tooling.
#[cfg(not(windows))]
fn main() {
    eprintln!("smoke-hotkey is Windows-only.");
    std::process::exit(1);
}

#[cfg(windows)]
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
#[cfg(windows)]
use std::time::Instant;
#[cfg(windows)]
use winit::event_loop::{ControlFlow, EventLoop};

#[cfg(windows)]
fn main() {
    // ASCII-only output: default Windows console (cp1252) garbles UTF-8.
    // For real UI we use egui (Unicode-clean); console messages stay ASCII.
    println!("Time Tracker - smoke test (SPEC s7 item 0)");
    println!("Hotkey under test: Ctrl+Shift+H (quick-entry default per SPEC s3.1)");

    install_ctrlc_handler();

    let event_loop = EventLoop::new().expect("event loop");

    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("FATAL: GlobalHotKeyManager::new failed: {e}");
            eprintln!("Win32 hotkey subsystem unavailable. Cannot proceed.");
            std::process::exit(1);
        }
    };

    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyH);
    if let Err(e) = manager.register(hotkey) {
        eprintln!("FATAL: failed to register Ctrl+Shift+H: {e}");
        eprintln!("Likely cause: another app already owns this combo.");
        eprintln!("Try closing AutoHotkey / browser DevTools / etc and rerun.");
        std::process::exit(1);
    }

    println!("OK. Press Ctrl+Shift+H to fire. Ctrl+C to exit.");
    println!();

    let receiver = GlobalHotKeyEvent::receiver();
    let mut fire_count: u32 = 0;
    let started = Instant::now();

    let _ = event_loop.run(move |_event, elwt| {
        // Poll keeps the loop spinning so we can drain the hotkey channel.
        elwt.set_control_flow(ControlFlow::Poll);

        while let Ok(hk_event) = receiver.try_recv() {
            fire_count += 1;
            let elapsed = started.elapsed();
            println!(
                "[{:>7.2}s] HOTKEY #{}  state={:?}  id={}",
                elapsed.as_secs_f32(),
                fire_count,
                hk_event.state,
                hk_event.id,
            );
            beep();
        }
    });
}

#[cfg(windows)]
fn beep() {
    // MessageBeep lives in Win32::System::Diagnostics::Debug per windows-sys
    // module layout (Microsoft categorized it as a debug-audio API). The
    // MB_ICONASTERISK style constant stays in WindowsAndMessaging.
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
        use windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;
        MessageBeep(MB_ICONASTERISK);
    }
}


#[cfg(windows)]
fn install_ctrlc_handler() {
    // tao's event loop on Windows does not auto-handle SIGINT; without a
    // console control handler, Ctrl+C in the parent shell does nothing
    // (or kills the shell, depending on host). Register a Win32 console
    // control handler that exits cleanly on CTRL_C_EVENT or CTRL_BREAK_EVENT.
    use windows_sys::Win32::Foundation::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };

    unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
        if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
            println!();
            println!("[Ctrl+C - exiting smoke test cleanly]");
            std::process::exit(0);
        }
        0 // FALSE: not handled, let other handlers process (close, logoff, shutdown)
    }

    // Add (not remove). Returns BOOL; non-zero on success.
    let ok = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    if ok == 0 {
        eprintln!("WARNING: SetConsoleCtrlHandler failed; Ctrl+C may not exit cleanly.");
    }
}

