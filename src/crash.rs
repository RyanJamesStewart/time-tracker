// SPEC §4.1 crash handler - panic-hook fallback (per the scope-cut
// option in §4.1: "if Day 3 PM runs hot, fall back to simple
// std::panic::set_hook + log + show-on-next-launch").
//
// On panic: write a panic-{ts}.log file under
// %LOCALAPPDATA%\TimeTracker\crashes\ with the panic info + backtrace.
// On startup: scan for recent crash files and surface to tracing.
//
// minidumper-style native minidumps (the spec's "ideal" path) deferred
// to v1.1 - it requires a separate IPC server process and is overkill
// for a 1-customer hand-delivered v1 where a panic log + a phone call
// is the support path.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::usage::Usage;

static CRASH_DIR: OnceLock<PathBuf> = OnceLock::new();
static USAGE: OnceLock<Arc<Usage>> = OnceLock::new();

/// Wire the Usage handle so the panic hook can emit a `crash` event.
/// Call once at app start, after `Usage::open()`. If never called, the
/// panic hook still writes the panic-{ts}.log file but skips the
/// `usage.log` event.
pub fn set_usage(u: Arc<Usage>) {
    let _ = USAGE.set(u);
}

pub fn install_handler() {
    let dir = crate::paths::data_dir().join("crashes");
    let _ = std::fs::create_dir_all(&dir);
    let _ = CRASH_DIR.set(dir);

    std::panic::set_hook(Box::new(|info| {
        // First write to stderr so the developer sees something while
        // attached, then write to the crash log so the partner has a
        // file to send.
        eprintln!("FATAL: app panicked: {info}");

        // Best-effort: emit a structural `crash` event into usage.log so
        // the wedge-data analysis can compute reliability without
        // cross-referencing two corpora. Process is dying so we skip
        // emitting if Usage was never wired (test contexts, early-init
        // panics) - the panic-{ts}.log below is still authoritative.
        if let Some(u) = USAGE.get() {
            u.crash();
        }

        let dir = match CRASH_DIR.get() {
            Some(d) => d,
            None => return,
        };
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("panic-{ts}.log"));
        let backtrace = std::backtrace::Backtrace::force_capture();

        let content = format!(
            "Time Tracker panic\n\
             ===========================\n\
             timestamp:  {}\n\
             version:    {}\n\
             platform:   {}\n\
             \n\
             panic info:\n{info}\n\
             \n\
             backtrace:\n{backtrace}\n",
            chrono::Local::now().to_rfc3339(),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
        );
        let _ = std::fs::write(&path, content);
        eprintln!("Crash log: {}", path.display());
    }));
}

/// Find the most recent panic-*.log if any. Used at startup to surface
/// "the last run crashed" to the user.
pub fn find_recent_crash() -> Option<PathBuf> {
    let dir = crate::paths::data_dir().join("crashes");
    std::fs::read_dir(&dir).ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("panic-") && n.ends_with(".log"))
        })
        .max_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// SPEC §4.1 / §6.4: if a crash artifact from the previous run exists, put
/// it in front of the user — a Windows MessageBox: "Time Tracker crashed
/// last session. Send the diagnostic to the developer?" with a `mailto:`
/// link (no auto-upload), or discard. Either way the artifact is *renamed*
/// (`panic-*.log` → `panic-*.log.seen`) so it doesn't re-prompt next launch.
/// No-op on non-Windows. Best-effort: a failed MessageBox / rename is logged.
#[cfg(windows)]
pub fn surface_recent_crash() {
    let Some(crash_path) = find_recent_crash() else { return };
    let name = crash_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("panic.log")
        .to_string();
    let crashes_dir = crate::paths::data_dir().join("crashes");

    // mailto: with a short, PII-free body — the developer asks the user to
    // attach the panic-*.log; we don't auto-upload anything.
    let subject = "Time Tracker crash report";
    let body = format!(
        "Time Tracker crashed in the previous session.\\n\\nPlease attach this file:\\n{}\\n\\n(No data was uploaded automatically.)",
        crash_path.display()
    );
    let mailto = format!(
        "mailto:?subject={}&body={}",
        url_escape(subject),
        url_escape(&body)
    );

    let title = "Time Tracker — crash recovered";
    let text = format!(
        "Time Tracker closed unexpectedly during the previous session.\n\n\
         A diagnostic was saved to:\n{}\n\n\
         Send it to the developer? (Yes opens a pre-filled email — attach the file above. \
         No just dismisses; nothing is uploaded either way.)",
        crash_path.display()
    );

    let pressed_yes = message_box_yesno(title, &text);
    if pressed_yes {
        open_mailto(&mailto);
    }
    // Mark the artifact handled so we don't re-prompt. Keep it on disk
    // (renamed) so the user / developer can still find it.
    let seen_path = crashes_dir.join(format!("{name}.seen"));
    match std::fs::rename(&crash_path, &seen_path) {
        Ok(()) => tracing::info!(from = %crash_path.display(), to = %seen_path.display(),
            "crash artifact surfaced and archived"),
        Err(e) => tracing::warn!(error = %e, path = %crash_path.display(),
            "could not archive crash artifact after surfacing — may re-prompt next launch"),
    }
}
#[cfg(not(windows))]
pub fn surface_recent_crash() {}

/// SPEC §4.1 first-launch self-test: a quick write-and-readback of a test
/// row in the CSV dir. On first launch (no marker file) only. Shows a
/// one-line success/fail banner via MessageBox and drops the marker so it
/// runs once. No-op on non-Windows.
#[cfg(windows)]
pub fn first_launch_self_test() {
    let marker = crate::paths::data_dir().join(".self-test-done");
    if marker.exists() {
        return;
    }
    let probe = crate::paths::csv_dir().join(".self-test-probe.csv");
    let payload = b"timestamp_iso,staff,client\r\n2026-01-01T00:00:00-00:00,self-test,probe\r\n";
    let (ok, detail) = match std::fs::write(&probe, payload).and_then(|_| std::fs::read(&probe)) {
        Ok(read_back) if read_back == payload => (true, String::new()),
        Ok(_) => (false, "the file was written but read back wrong".to_string()),
        Err(e) => (false, format!("{e}")),
    };
    let _ = std::fs::remove_file(&probe);
    let title = "Time Tracker — first-launch check";
    let text = if ok {
        format!(
            "Setup check passed: the time-log folder is writable.\n\n{}",
            crate::paths::csv_dir().display()
        )
    } else {
        format!(
            "Setup check FAILED: couldn't write to the time-log folder.\n\n{}\n\n{}\n\n\
             Entries will queue to disk until this is fixed — check the folder's permissions.",
            crate::paths::csv_dir().display(),
            detail
        )
    };
    message_box_ok(title, &text);
    // Drop the marker regardless — re-prompting every launch would be worse
    // than missing a one-time banner. (If the folder is unwritable, the
    // marker write fails too, so on a still-broken next launch the banner
    // re-shows — which is the right behaviour.)
    let _ = std::fs::write(&marker, b"");
}
#[cfg(not(windows))]
pub fn first_launch_self_test() {}

// ---- tiny Win32 helpers (no extra deps; windows-sys already present) ----

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn message_box_ok(title: &str, text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
    let t = to_wide(title);
    let b = to_wide(text);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), b.as_ptr(), t.as_ptr(), MB_OK | MB_ICONINFORMATION);
    }
}

#[cfg(windows)]
fn message_box_yesno(title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_ICONWARNING, MB_YESNO,
    };
    let t = to_wide(title);
    let b = to_wide(text);
    let r = unsafe { MessageBoxW(std::ptr::null_mut(), b.as_ptr(), t.as_ptr(), MB_YESNO | MB_ICONWARNING) };
    r == IDYES as i32
}

#[cfg(windows)]
fn open_mailto(mailto: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Err(e) = std::process::Command::new("cmd")
        .args(["/C", "start", "", mailto])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        tracing::warn!(error = %e, "could not launch the mail client for the crash report");
    }
}

/// Minimal percent-encoding for the mailto query (RFC 3986 unreserved kept;
/// everything else → %XX). Good enough for `subject=`/`body=`.
#[cfg(windows)]
fn url_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
