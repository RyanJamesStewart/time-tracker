// SPEC.md §5.1 (logging stack) + §4.1 (30-day retention enforced by app).
//
// `tracing` for structured logging; `tracing-appender` for daily rotation
// to `data_dir/logs/time-tracker.YYYY-MM-DD`. tracing-appender does NOT enforce
// retention - we delete files older than 30 days here at startup.
//
// Log level: defaults to INFO. Override with the TIMETRACKER_LOG environment
// variable using tracing-subscriber EnvFilter syntax (e.g.
// TIMETRACKER_LOG="time_tracker=debug,tao=warn").

use chrono::{Duration, Local, NaiveDate};
use std::fs;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

const FILE_PREFIX: &str = "time-tracker";
const RETENTION_DAYS: i64 = 30;

/// Initialize tracing. Returns the WorkerGuard for the non-blocking
/// file writer; caller MUST hold this for the lifetime of the process
/// or pending log lines may be dropped on exit.
pub fn init() -> WorkerGuard {
    let logs = crate::paths::data_dir().join("logs");
    let _ = fs::create_dir_all(&logs);

    enforce_retention(&logs, RETENTION_DAYS);

    let file_appender = tracing_appender::rolling::daily(&logs, FILE_PREFIX);
    let (writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("TIMETRACKER_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .init();

    guard
}

/// Delete log files older than `days_to_keep`. tracing-appender names
/// daily files like `time-tracker.YYYY-MM-DD` (no extension by default).
fn enforce_retention(logs_dir: &Path, days_to_keep: i64) {
    let cutoff = (Local::now() - Duration::days(days_to_keep)).date_naive();
    let prefix = format!("{FILE_PREFIX}.");

    let entries = match fs::read_dir(logs_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let date_str = match name.strip_prefix(&prefix) {
            Some(s) => s,
            None => continue,
        };
        if let Ok(d) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            if d < cutoff {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}
