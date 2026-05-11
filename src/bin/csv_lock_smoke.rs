// SPEC.md §4.1 retry-path smoke test.
//
// Drives `csv_writer::spawn()` directly (no UI, no IPC) and submits one
// entry. Used by `scripts/test-excel-lock.ps1` to validate the lock-retry
// + queue-fallback path against a REAL Windows ERROR_SHARING_VIOLATION
// from PowerShell holding the file open exclusively.
//
// Reads `TIMETRACKER_DATA_DIR_OVERRIDE` and `TIMETRACKER_CSV_DIR_OVERRIDE` so the
// test never touches the user's real %LOCALAPPDATA%\TimeTracker or
// %USERPROFILE%\TimeTracker. The PowerShell harness sets both to a
// temp sandbox.
//
// Expected outcomes (printed as the last line, machine-readable):
//   RESULT: WRITTEN     - file unlocked or unlocked within 10s; row landed
//   RESULT: QUEUED      - file stayed locked > 10s; entry persisted to queue
//   RESULT: ERROR <msg> - hard failure (not lock-related)
//
// Build:  cargo xwin build --release --target x86_64-pc-windows-msvc \
//                              --bin csv-lock-smoke
// Linux gets a stub main so `cargo build --bins` still works on WSL.

#[cfg(not(windows))]
fn main() {
    eprintln!("csv-lock-smoke is Windows-only.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    use chrono::Local;
    use std::sync::Arc;
    use time_tracker::{csv_writer, paths, usage};

    eprintln!("csv-lock-smoke: SPEC.md §4.1 retry-path validator");
    eprintln!(
        "  data_dir = {}\n  csv_dir  = {}",
        paths::data_dir().display(),
        paths::csv_dir().display()
    );

    if let Err(e) = paths::ensure_layout() {
        eprintln!("ensure_layout failed: {e}");
        println!("RESULT: ERROR ensure_layout: {e}");
        std::process::exit(1);
    }

    // `spawn` needs the usage sink (queue-drain events go through it); the
    // smoke doesn't read them back, but the signature requires it.
    let writer = csv_writer::spawn(Arc::new(usage::Usage::open()));

    let entry = csv_writer::Entry {
        timestamp: Local::now(),
        staff: "test-harness".to_string(),
        client: "LOCK-SMOKE".to_string(),
        engagement: "validate-retry".to_string(),
        narrative: "scripts/test-excel-lock.ps1 driving csv-lock-smoke".to_string(),
        minutes: 6,
        billable: false,
        entry_method: csv_writer::EntryMethod::Quick,
        workstream_id: None,
    };

    let started = std::time::Instant::now();
    let result = writer.write_blocking(entry);
    let elapsed = started.elapsed();

    eprintln!("write_blocking returned in {:.2}s", elapsed.as_secs_f32());

    match result {
        csv_writer::WriteResult::Written => println!("RESULT: WRITTEN"),
        csv_writer::WriteResult::Queued => println!("RESULT: QUEUED"),
        csv_writer::WriteResult::Error(msg) => {
            println!("RESULT: ERROR {msg}");
            std::process::exit(2);
        }
    }
}
