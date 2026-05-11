// SPEC.md §5.2 path resolution + §4.1 pre-create placeholder files.
//
// Resolves the canonical data and CSV directories. With MSIX file
// virtualization disabled in the manifest (per SPEC §6.1
// `desktop6:FileSystemWriteVirtualization="disabled"`), packaged and
// non-packaged builds both write to the SAME physical paths.
//
// CSV path choice: %USERPROFILE%\TimeTracker\ (NOT under Documents).
// Reason: when a user has OneDrive Documents-folder backup enabled
// (default for many MS-account configs), %USERPROFILE%\Documents\
// silently redirects to %USERPROFILE%\OneDrive\Documents\. Files put
// there sync to Microsoft cloud automatically. For accountant client
// data we want the user to explicitly opt in to cloud sync, not
// inherit it. Putting the CSV folder directly under USERPROFILE (which
// is NOT a Known Folder and NOT redirected) gives a stable, predictable
// path. Users can always move/symlink later if they want OneDrive sync.
//
// MSIX-context detection (GetCurrentPackageFamilyName) is deferred
// until packaging work since it's only needed for diagnostic metadata,
// not file-path routing.

use std::fs;
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = "TimeTracker";
const CSV_DIR_NAME: &str = "TimeTracker";

// Env-var overrides for sandboxed test harnesses (e.g. the Excel-lock
// PowerShell smoke at scripts/test-excel-lock.ps1). Production code never
// sets these; if unset, the canonical %LOCALAPPDATA%\TimeTracker and
// %USERPROFILE%\TimeTracker paths are used.
const DATA_DIR_OVERRIDE_ENV: &str = "TIMETRACKER_DATA_DIR_OVERRIDE";
const CSV_DIR_OVERRIDE_ENV: &str = "TIMETRACKER_CSV_DIR_OVERRIDE";

/// `%LOCALAPPDATA%\TimeTracker` on Windows. Holds config, autocomplete cache,
/// timer state, queue, logs, crash dumps, and the usage instrumentation log.
pub fn data_dir() -> PathBuf {
    if let Ok(v) = std::env::var(DATA_DIR_OVERRIDE_ENV) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    let base = directories::BaseDirs::new()
        .expect("HOME / LOCALAPPDATA missing - cannot determine data dir");
    base.data_local_dir().join(APP_DIR_NAME)
}

/// `%USERPROFILE%\TimeTracker` on Windows. Holds the rolling monthly
/// CSV files. Lives directly under the user home (NOT under Documents)
/// to avoid OneDrive Known-Folder redirection - see module docstring.
pub fn csv_dir() -> PathBuf {
    if let Ok(v) = std::env::var(CSV_DIR_OVERRIDE_ENV) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    let base = directories::BaseDirs::new()
        .expect("HOME / USERPROFILE missing - cannot determine CSV dir");
    base.home_dir().join(CSV_DIR_NAME)
}

/// Create the full data layout (idempotent). Called once at startup
/// before any other subsystem touches disk.
///
/// Pre-creating placeholder files is the prompt-time MSIX-shadow
/// workaround: even with virtualization disabled, pre-creating the
/// real files means no first-write redirection can ever happen, and
/// downstream code can use simple `OpenOptions::append(true)` paths
/// without checking for missing-file errors.
pub fn ensure_layout() -> std::io::Result<()> {
    let data = data_dir();
    fs::create_dir_all(&data)?;
    fs::create_dir_all(data.join("queue"))?;
    fs::create_dir_all(data.join("logs"))?;
    fs::create_dir_all(data.join("crashes"))?;
    fs::create_dir_all(csv_dir())?;

    create_if_missing(&data.join("usage.log"), b"")?;
    create_if_missing(&data.join("autocomplete.json"), b"{}")?;
    create_if_missing(&data.join("queue").join(".keep"), b"")?;
    create_if_missing(&data.join("logs").join(".keep"), b"")?;
    create_if_missing(&data.join("crashes").join(".keep"), b"")?;
    Ok(())
}

fn create_if_missing(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::write(path, contents)
}
