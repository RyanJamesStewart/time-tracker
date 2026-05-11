// SPEC.md §7 item 4 - single-instance enforcement + named-pipe IPC.
//
// Pattern (per SPEC §4.1):
//   - First instance creates a named mutex `Local\TimeTracker` and a
//     named pipe server at `\\.\pipe\TimeTracker`.
//   - Second instance: CreateMutexW returns ERROR_ALREADY_EXISTS, so it
//     opens the named pipe as a client, sends a one-shot message
//     ("show_quick_entry" etc.), and exits.
//   - First instance's pipe server thread receives the message and invokes
//     the supplied callback (which posts a UserEvent into the tao loop).
//
// Mutex naming: spec says `Global\TimeTracker-<user-sid>`. v1 uses
// `Local\TimeTracker` (Local prefix scopes to the current Terminal
// Services session, which equals per-user on single-user desktops).
// Skips SID derivation work; revisit if multi-user-per-machine becomes
// real (uncommon for the target accountant deployment).

#![cfg(windows)]

use std::ffi::OsStr;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

const MUTEX_NAME: &str = r"Local\TimeTracker";
const PIPE_NAME: &str = r"\\.\pipe\TimeTracker";

pub enum InstanceCheck {
    /// We are the first instance. Hold the guard for the lifetime of
    /// the process; dropping it releases the OS handle.
    First(MutexGuard),
    /// Another instance is already running. We attempted to notify it
    /// (best-effort) and the caller should exit cleanly.
    Second,
}

pub struct MutexGuard {
    handle: HANDLE,
}

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

// Marker: HANDLE is *mut c_void which is !Send by default. CloseHandle
// is thread-safe per Win32 docs and we only ever drop from the owning
// thread anyway. Tagging Send so MutexGuard can live in main()'s scope
// without needing thread-local plumbing.
unsafe impl Send for MutexGuard {}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}

/// Try to acquire the single-instance mutex. If another instance owns
/// it, attempt to notify that instance via the named pipe and return
/// `Second` so the caller can exit cleanly.
pub fn acquire_or_notify(notify_msg: &str) -> InstanceCheck {
    let name_w = wide(MUTEX_NAME);
    let handle = unsafe { CreateMutexW(ptr::null_mut(), 1, name_w.as_ptr()) };
    let last_err = unsafe { GetLastError() };

    if handle.is_null() {
        // CreateMutexW failed entirely. Fail open (proceed without
        // single-instance) rather than blocking the user from launching.
        eprintln!(
            "WARNING: CreateMutexW returned NULL (err={last_err}); single-instance disabled"
        );
        return InstanceCheck::First(MutexGuard {
            handle: ptr::null_mut(),
        });
    }

    if last_err == ERROR_ALREADY_EXISTS {
        // Another instance owns the mutex. Close our duplicate handle,
        // try to notify the existing instance, then signal Second.
        unsafe { CloseHandle(handle) };
        if let Err(e) = send_pipe_message(notify_msg) {
            eprintln!("(could not notify existing instance: {e})");
        }
        InstanceCheck::Second
    } else {
        InstanceCheck::First(MutexGuard { handle })
    }
}

fn send_pipe_message(msg: &str) -> std::io::Result<()> {
    let name_w = wide(PIPE_NAME);
    let h = unsafe {
        CreateFileW(
            name_w.as_ptr(),
            FILE_GENERIC_WRITE,
            0,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let bytes = msg.as_bytes();
    let mut written: u32 = 0;
    let ok = unsafe {
        WriteFile(
            h,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Start a background thread that hosts the named-pipe server.
/// Each accepted client gets one message read; messages are passed to
/// `on_message`. Loop runs for the process lifetime.
pub fn start_pipe_server<F>(on_message: F)
where
    F: Fn(String) + Send + 'static,
{
    thread::Builder::new()
        .name("tt-pipe-server".to_string())
        .spawn(move || pipe_server_loop(on_message))
        .expect("failed to spawn pipe server thread");
}

fn pipe_server_loop<F>(on_message: F)
where
    F: Fn(String),
{
    // ERROR_PIPE_CONNECTED (535) means the client connected before we
    // called ConnectNamedPipe - that's still success, not failure.
    const ERROR_PIPE_CONNECTED: u32 = 535;
    let name_w = wide(PIPE_NAME);

    loop {
        let h = unsafe {
            CreateNamedPipeW(
                name_w.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                512,  // outbound buffer hint
                512,  // inbound buffer hint
                0,    // default timeout
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            eprintln!(
                "WARNING: CreateNamedPipeW failed: {}",
                std::io::Error::last_os_error()
            );
            // Back off briefly to avoid spinning on persistent failure.
            thread::sleep(Duration::from_secs(1));
            continue;
        }

        let connected = unsafe { ConnectNamedPipe(h, ptr::null_mut()) };
        if connected == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                eprintln!("WARNING: ConnectNamedPipe failed: err={err}");
                unsafe { CloseHandle(h) };
                continue;
            }
        }

        // Read a single message. PIPE_TYPE_MESSAGE means ReadFile
        // returns one full message per call.
        let mut buf = [0u8; 512];
        let mut read_n: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read_n,
                ptr::null_mut(),
            )
        };
        if ok != 0 && read_n > 0 {
            let msg = String::from_utf8_lossy(&buf[..read_n as usize]).to_string();
            on_message(msg);
        }

        unsafe { DisconnectNamedPipe(h) };
        unsafe { CloseHandle(h) };
    }
}
