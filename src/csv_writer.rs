// SPEC.md §3.7 (CSV format) + §4.1 (atomic write, self-verify, retry, queue,
// single-writer model).
//
// Design (per SPEC §4.1):
//   - All CSV writes (live submit + queue drain) flow through ONE
//     dedicated writer thread fed by an mpsc channel. No locks in app
//     code; the channel serializes everything.
//   - Each write: build row -> atomic temp+rename -> post-write
//     self-verify (re-read last line, confirm match).
//   - On Excel-lock failure (ERROR_SHARING_VIOLATION 32,
//     ERROR_LOCK_VIOLATION 33): exponential backoff 100ms..2000ms for up
//     to 10 seconds total. Then queue the entry to disk and return
//     Queued. Drain runs after each successful write.
//   - ERROR_ACCESS_DENIED (5) is NOT treated as a lock error (R4,
//     2026-05-11): a real ACCESS_DENIED means the CSV folder isn't
//     writable (moved to a synced/locked location, an EDR policy on the
//     path, …) — retrying then queueing forever just silently piles up
//     entries that will never drain, with a misleading "close Excel" hint.
//     We fail loudly instead with a permissions-flavoured message.
//   - File creation: writes UTF-8 BOM + header row before the first
//     data row. Matches SPEC §3.7 column order exactly.
//   - Line endings: CRLF. Encoding: UTF-8 with BOM.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::usage::Usage;

// File layout: a version banner line, then the column header, then CRLF
// rows. Lines starting with `#` are comments, skipped by all parsers.
//
// v0.3 (2026-05-11): column 0 is a plain `date` (YYYY-MM-DD), not an ISO
// timestamp. Entries are date + duration, full stop — no begin/end clock
// time anywhere (a timer still runs against wall time internally, but only
// the date it landed on and the minutes worked are recorded). Parsers also
// accept the old `timestamp_iso` form (anything with a `T`) and use its
// date part, so v0.1/v0.2 files keep reading; an existing file keeps its
// existing column shape on append (10 cols if it had `workstream_id`, 9 if
// not) — only files *created* now get the v0.3 banner + header.
const VERSION_COMMENT: &[u8] = b"# time-tracker v0.3\r\n";
const HEADER: &[u8] = b"date,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method,workstream_id\r\n";
#[cfg_attr(not(test), allow(dead_code))]
const HEADER_V01: &[u8] = b"timestamp_iso,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method\r\n";
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

const RETRY_TOTAL: Duration = Duration::from_secs(10);
const RETRY_INITIAL_MS: u64 = 100;
const RETRY_MAX_MS: u64 = 2000;

// Windows lock-error codes treated as "Excel (or another holder) has
// the file open exclusively; retry". ERROR_ACCESS_DENIED (5) is
// deliberately NOT here — see the module docstring (R4).
#[cfg(windows)]
const LOCK_ERRORS: &[i32] = &[32, 33];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryMethod {
    Quick,
    Timer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub timestamp: DateTime<Local>,
    pub staff: String,
    pub client: String,
    pub engagement: String,
    pub narrative: String,
    pub minutes: u32,
    pub billable: bool,
    pub entry_method: EntryMethod,
    /// v0.2: reference into the workstream registry. `None` only for
    /// rows replayed from a v0.1 queue file, or when appending to a
    /// pre-existing v0.1 monthly CSV (which keeps the 9-column shape).
    #[serde(default)]
    pub workstream_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum WriteResult {
    /// Wrote successfully and self-verify confirmed the row landed.
    Written,
    /// File was locked beyond the retry window; entry queued to disk
    /// and will drain on the next successful write.
    Queued,
    /// Hard error - entry not written and not queued.
    Error(String),
}

/// Result of a `Request::Rewrite` (the read-modify-rewrite-whole-file path
/// used by the Recorded-Time API). There is no queue for rewrites — the
/// transform is a one-shot closure that can't be persisted — so a prolonged
/// lock is just an error the caller surfaces (503).
#[derive(Debug, Clone)]
pub enum RewriteResult {
    /// File written; the read-modify-rewrite landed atomically.
    Written,
    /// The CSV stayed locked past the retry window. The caller should
    /// surface this as "close the other program (Excel?) and try again".
    Locked(String),
    /// The transform closure rejected (e.g. "no such row") or a hard IO
    /// error (not lock-related, e.g. ACCESS_DENIED — see R4).
    Error(String),
}

/// The transform a `Request::Rewrite` runs *on the writer thread*. It
/// receives the current file bytes (`Some`) or `None` if the file does not
/// exist (`ErrorKind::NotFound` specifically — a *failed read of an existing
/// file* never reaches the closure as `None`; the writer thread retries
/// lock errors and returns `RewriteResult::Error` on anything else). It
/// returns the new whole-file bytes, or an error string the caller surfaces.
pub type RewriteFn = Box<dyn FnOnce(Option<&[u8]>) -> Result<Vec<u8>, String> + Send>;

/// Cheap clone-able handle for posting writes to the writer thread.
#[derive(Clone)]
pub struct WriterHandle {
    tx: Sender<Request>,
}

enum Request {
    Write {
        entry: Entry,
        ack: Sender<WriteResult>,
    },
    /// Read-modify-rewrite the whole monthly CSV for `stem` (e.g. an edit /
    /// create / delete from the Recorded-Time API). Runs on the same writer
    /// thread as `Write`, so there is exactly one serialized writer per
    /// monthly file (the single-writer invariant). Same atomic temp+rename +
    /// Excel-lock-retry-up-to-10s discipline as the append path; no queue.
    Rewrite {
        stem: String,
        transform: RewriteFn,
        ack: Sender<RewriteResult>,
    },
}

/// Spawn the writer thread. Returns a handle that can be cheaply cloned
/// across the app. Each handle posts a Write request and awaits the
/// WriteResult on a private one-shot channel.
///
/// `usage` is wired through so the writer thread can emit `queue_drain`
/// events directly when it drains queued entries — the wedge thesis
/// (SPEC §5.4) needs `count` per drain event so we can answer
/// "how often did Excel-lock recovery succeed?" without cross-referencing
/// the tracing log.
pub fn spawn(usage: Arc<Usage>) -> WriterHandle {
    let (tx, rx) = channel();
    thread::Builder::new()
        .name("tt-csv-writer".to_string())
        .spawn(move || writer_loop(rx, usage))
        .expect("failed to spawn csv writer thread");
    WriterHandle { tx }
}

impl WriterHandle {
    /// Block until the entry is written, queued, or errored.
    pub fn write_blocking(&self, entry: Entry) -> WriteResult {
        let (ack_tx, ack_rx) = channel();
        if self
            .tx
            .send(Request::Write {
                entry,
                ack: ack_tx,
            })
            .is_err()
        {
            return WriteResult::Error("writer thread is dead".to_string());
        }
        ack_rx
            .recv()
            .unwrap_or_else(|_| WriteResult::Error("writer dropped ack".to_string()))
    }

    /// Block until a read-modify-rewrite of `<stem>.csv` lands (or fails).
    /// `transform` runs *on the writer thread* — see [`RewriteFn`] — so the
    /// read-modify-write is serialized against the append path; this closes
    /// the lost-append / torn-file race that two independent writers create.
    pub fn rewrite_blocking(&self, stem: impl Into<String>, transform: RewriteFn) -> RewriteResult {
        let (ack_tx, ack_rx) = channel();
        if self
            .tx
            .send(Request::Rewrite {
                stem: stem.into(),
                transform,
                ack: ack_tx,
            })
            .is_err()
        {
            return RewriteResult::Error("writer thread is dead".to_string());
        }
        ack_rx
            .recv()
            .unwrap_or_else(|_| RewriteResult::Error("writer dropped ack".to_string()))
    }
}

fn writer_loop(rx: Receiver<Request>, usage: Arc<Usage>) {
    // Best-effort drain at startup: anything queued from a prior
    // session (or before the writer was up) gets a chance to land.
    if let Ok(n) = drain_queue() {
        if n > 0 {
            usage.queue_drain(n);
        }
    }

    while let Ok(req) = rx.recv() {
        match req {
            Request::Write { entry, ack } => {
                let result = write_with_retry(&entry);
                if matches!(result, WriteResult::Written) {
                    if let Ok(n) = drain_queue() {
                        if n > 0 {
                            usage.queue_drain(n);
                        }
                    }
                }
                let _ = ack.send(result);
            }
            Request::Rewrite { stem, transform, ack } => {
                let result = rewrite_with_retry(&stem, transform);
                if matches!(result, RewriteResult::Written) {
                    // A rewrite that landed means the file is no longer
                    // locked — same opportunistic drain the append path does.
                    if let Ok(n) = drain_queue() {
                        if n > 0 {
                            usage.queue_drain(n);
                        }
                    }
                }
                let _ = ack.send(result);
            }
        }
    }
}

/// Read-modify-rewrite `<stem>.csv` on the writer thread. Resolves the path
/// off `paths::csv_dir()` and delegates to [`rewrite_path`].
fn rewrite_with_retry(stem: &str, transform: RewriteFn) -> RewriteResult {
    rewrite_path(&monthly_csv_path_for_stem(stem), transform)
}

fn monthly_csv_path_for_stem(stem: &str) -> PathBuf {
    crate::paths::csv_dir().join(format!("{stem}.csv"))
}

/// Read-modify-rewrite the file at `path`. The read AND the write retry on
/// Excel-lock errors (codes 32/33) up to `RETRY_TOTAL`; a real
/// `ACCESS_DENIED` (5) is NOT a lock error (R4) — it fails loudly with a
/// permissions-flavoured message instead of retrying forever. A failed read
/// of an existing file is never collapsed to "create fresh" — only a genuine
/// `NotFound` reaches the transform as `None` (B4). The transform's returned
/// bytes are written atomically (temp `+ sync_all + rename`).
fn rewrite_path(path: &Path, transform: RewriteFn) -> RewriteResult {
    let parent = match path.parent() {
        Some(p) => p,
        None => return RewriteResult::Error("csv path has no parent".to_string()),
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        return RewriteResult::Error(format!("mkdir: {e}"));
    }

    // --- read (with lock retry) ---
    let started = Instant::now();
    let mut backoff_ms = RETRY_INITIAL_MS;
    let current: Option<Vec<u8>> = loop {
        match std::fs::read(path) {
            Ok(bytes) => break Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break None,
            Err(e) if is_lock_error(&e) => {
                if started.elapsed() >= RETRY_TOTAL {
                    return RewriteResult::Locked(format!(
                        "the CSV file is open in another program (Excel?) — close it and try again ({})",
                        path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_MS);
            }
            Err(e) => {
                // ACCESS_DENIED and friends: fail loudly, do NOT retry-forever.
                return RewriteResult::Error(format!(
                    "can't read {} — check permissions ({e})",
                    path.display()
                ));
            }
        }
    };

    // --- transform (on this thread) ---
    let new_bytes = match transform(current.as_deref()) {
        Ok(b) => b,
        Err(msg) => return RewriteResult::Error(msg),
    };

    // --- write atomically (with lock retry) ---
    let file_name = match path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => return RewriteResult::Error("csv path has no file name".to_string()),
    };
    let temp_path = parent.join(format!(".{file_name}.tmp"));
    let started = Instant::now();
    let mut backoff_ms = RETRY_INITIAL_MS;
    loop {
        let attempt = (|| -> std::io::Result<()> {
            {
                let mut f = std::fs::File::create(&temp_path)?;
                f.write_all(&new_bytes)?;
                f.sync_all()?;
            }
            std::fs::rename(&temp_path, path)?;
            Ok(())
        })();
        match attempt {
            Ok(()) => return RewriteResult::Written,
            Err(e) if is_lock_error(&e) => {
                if started.elapsed() >= RETRY_TOTAL {
                    let _ = std::fs::remove_file(&temp_path);
                    return RewriteResult::Locked(format!(
                        "the CSV file is open in another program (Excel?) — close it and try again ({})",
                        path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_MS);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                return RewriteResult::Error(format!(
                    "can't write {} — check permissions ({e})",
                    path.display()
                ));
            }
        }
    }
}

fn write_with_retry(entry: &Entry) -> WriteResult {
    let path = monthly_csv_path(&entry.timestamp);

    let started = Instant::now();
    let mut backoff_ms = RETRY_INITIAL_MS;

    loop {
        match append_atomic(&path, entry) {
            Ok(row) => match verify_last_line(&path, &row) {
                Ok(true) => return WriteResult::Written,
                Ok(false) => {
                    tracing::warn!(
                        path = %path.display(),
                        "CSV self-verify mismatch; possible AV rollback or concurrent writer",
                    );
                    return WriteResult::Error(
                        "post-write self-verify failed".to_string(),
                    );
                }
                Err(e) => {
                    return WriteResult::Error(format!("verify read failed: {e}"));
                }
            },
            Err(e) if is_lock_error(&e) => {
                if started.elapsed() >= RETRY_TOTAL {
                    tracing::warn!(
                        path = %path.display(),
                        "CSV locked > 10s; queueing entry for later drain"
                    );
                    return match write_queue(entry) {
                        Ok(()) => WriteResult::Queued,
                        Err(qe) => WriteResult::Error(format!(
                            "locked + queue write failed: {qe}"
                        )),
                    };
                }
                tracing::debug!(
                    path = %path.display(),
                    backoff_ms,
                    "CSV write locked, sleeping and retrying"
                );
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_MS);
            }
            Err(e) => {
                // Not a lock — e.g. ERROR_ACCESS_DENIED (5): the CSV folder
                // isn't writable. Fail loudly with a permissions-flavoured
                // message (NOT "close Excel" — there's no Excel here) so the
                // user fixes the folder instead of watching entries pile up
                // in the queue forever (R4).
                return WriteResult::Error(format!(
                    "can't write {} — check the folder's permissions ({e})",
                    path.display()
                ));
            }
        }
    }
}

/// Append `entry` as a CSV row: read the file, decide v0.1-vs-v0.2 shape
/// (new files are v0.2; an existing v0.1 file — no `#` banner — keeps the
/// 9-column shape so it stays homogeneous), append, write the whole thing
/// to a sibling `.tmp`, fsync, rename atomically. Returns the exact row
/// bytes written so the caller can self-verify the tail. Per SPEC §4.1.
fn append_atomic(path: &Path, entry: &Entry) -> std::io::Result<Vec<u8>> {
    let parent = path.parent().expect("CSV path always has a parent");
    std::fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .expect("CSV path always has a file name")
        .to_string_lossy()
        .into_owned();
    let temp_path = parent.join(format!(".{file_name}.tmp"));

    let (mut content, v02): (Vec<u8>, bool) = if path.exists() {
        let existing = std::fs::read(path)?;
        let after_bom = existing.strip_prefix(&UTF8_BOM[..]).unwrap_or(&existing);
        // v0.2 files open with the `# time-tracker …` banner; v0.1
        // files open straight into the column header.
        let is_v02 = after_bom.starts_with(b"#");
        (existing, is_v02)
    } else {
        let mut buf =
            Vec::with_capacity(UTF8_BOM.len() + VERSION_COMMENT.len() + HEADER.len());
        buf.extend_from_slice(&UTF8_BOM);
        buf.extend_from_slice(VERSION_COMMENT);
        buf.extend_from_slice(HEADER);
        (buf, true)
    };

    let row = if v02 { format_row(entry) } else { format_row_legacy(entry) };
    content.extend_from_slice(&row);

    {
        let mut f = std::fs::File::create(&temp_path)?;
        f.write_all(&content)?;
        f.sync_all()?;
    }
    std::fs::rename(&temp_path, path)?;
    Ok(row)
}

fn verify_last_line(path: &Path, expected_row: &[u8]) -> std::io::Result<bool> {
    let content = std::fs::read(path)?;
    Ok(content.ends_with(expected_row))
}

#[cfg(windows)]
fn is_lock_error(e: &std::io::Error) -> bool {
    e.raw_os_error()
        .map(|c| LOCK_ERRORS.contains(&c))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_lock_error(_e: &std::io::Error) -> bool {
    false
}

fn monthly_csv_path(ts: &DateTime<Local>) -> PathBuf {
    crate::paths::csv_dir().join(format!("{}.csv", ts.format("%Y-%m")))
}

// ----- queue (fallback for prolonged locks) -----

fn queue_path_for(entry: &Entry) -> PathBuf {
    // Filename is timestamp-based + microseconds for uniqueness so
    // simultaneous queue writes don't collide.
    crate::paths::data_dir()
        .join("queue")
        .join(format!("{}.json", entry.timestamp.format("%Y%m%d-%H%M%S-%6f")))
}

fn write_queue(entry: &Entry) -> std::io::Result<()> {
    let path = queue_path_for(entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&path, json)?;
    tracing::info!(path = %path.display(), "queued entry to disk");
    Ok(())
}

/// Drain queued entries (best-effort). Each successful write removes the
/// queue file. Stops as soon as a write fails (assume the file is
/// re-locked) and leaves the rest for the next drain.
fn drain_queue() -> std::io::Result<usize> {
    let queue_dir = crate::paths::data_dir().join("queue");
    if !queue_dir.exists() {
        return Ok(0);
    }
    let mut entries: Vec<_> = std::fs::read_dir(&queue_dir)?
        .filter_map(|r| r.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    // Process in chronological order (filename starts with timestamp).
    entries.sort_by_key(|e| e.file_name());

    let mut drained = 0usize;
    for dir_entry in entries {
        let queue_file = dir_entry.path();
        let json = match std::fs::read_to_string(&queue_file) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %queue_file.display(), error = %e, "queue file unreadable; skipping");
                continue;
            }
        };
        let entry: Entry = match serde_json::from_str(&json) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(path = %queue_file.display(), error = %e, "queue file unparseable; skipping");
                continue;
            }
        };
        match write_with_retry_no_queue(&entry) {
            DrainOutcome::Written => {
                let _ = std::fs::remove_file(&queue_file);
                drained += 1;
            }
            DrainOutcome::StillLocked => {
                // Stop draining; the file is still locked. Leave the
                // remaining queue files in place for the next drain.
                tracing::info!(drained, "queue drain paused (CSV still locked)");
                break;
            }
            DrainOutcome::Error(msg) => {
                tracing::warn!(path = %queue_file.display(), error = %msg, "queue drain error; leaving file");
                break;
            }
        }
    }
    if drained > 0 {
        tracing::info!(drained, "queue drained");
    }
    Ok(drained)
}

enum DrainOutcome {
    Written,
    StillLocked,
    Error(String),
}

/// Like write_with_retry but never re-queues on failure. Used by the
/// drain path so we don't get a queue-from-queue infinite loop.
fn write_with_retry_no_queue(entry: &Entry) -> DrainOutcome {
    let path = monthly_csv_path(&entry.timestamp);

    let started = Instant::now();
    let mut backoff_ms = RETRY_INITIAL_MS;
    loop {
        match append_atomic(&path, entry) {
            Ok(row) => match verify_last_line(&path, &row) {
                Ok(true) => return DrainOutcome::Written,
                Ok(false) => return DrainOutcome::Error("self-verify failed".to_string()),
                Err(e) => return DrainOutcome::Error(format!("verify read: {e}")),
            },
            Err(e) if is_lock_error(&e) => {
                if started.elapsed() >= RETRY_TOTAL {
                    return DrainOutcome::StillLocked;
                }
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_MS);
            }
            Err(e) => return DrainOutcome::Error(format!("io: {e}")),
        }
    }
}

// ----- row formatting -----

pub fn format_row(e: &Entry) -> Vec<u8> {
    // v0.3: column 0 is the *date* the work landed on (the local calendar
    // date of `e.timestamp`), formatted `YYYY-MM-DD`. We deliberately drop
    // the wall-clock time: entries are date + duration. Excel reads
    // `2026-05-09` as a date with no formula, and there's no DST/offset
    // ambiguity to preserve once the clock time is gone. The monthly file an
    // entry lands in is still chosen by `e.timestamp`'s month, so an entry
    // logged near midnight stays on the day the timer (or the user) said.
    // 10 columns, trailing `workstream_id` (empty if unknown).
    let hours_decimal = (e.minutes as f64) / 60.0;
    let row = format!(
        "{ts},{staff},{client},{engagement},{narrative},{minutes},{hours:.2},{billable},{method},{ws}\r\n",
        ts = e.timestamp.format("%Y-%m-%d"),
        staff = csv_escape(&e.staff),
        client = csv_escape(&e.client),
        engagement = csv_escape(&e.engagement),
        narrative = csv_escape(&e.narrative),
        minutes = e.minutes,
        hours = hours_decimal,
        billable = e.billable,
        method = match e.entry_method {
            EntryMethod::Quick => "quick",
            EntryMethod::Timer => "timer",
        },
        ws = csv_escape(e.workstream_id.as_deref().unwrap_or("")),
    );
    row.into_bytes()
}

/// v0.1 row shape (9 columns, no `workstream_id`). Used only when
/// appending to a pre-existing v0.1 monthly CSV so it stays homogeneous.
fn format_row_legacy(e: &Entry) -> Vec<u8> {
    let hours_decimal = (e.minutes as f64) / 60.0;
    let row = format!(
        "{ts},{staff},{client},{engagement},{narrative},{minutes},{hours:.2},{billable},{method}\r\n",
        ts = e.timestamp.format("%Y-%m-%d"),
        staff = csv_escape(&e.staff),
        client = csv_escape(&e.client),
        engagement = csv_escape(&e.engagement),
        narrative = csv_escape(&e.narrative),
        minutes = e.minutes,
        hours = hours_decimal,
        billable = e.billable,
        method = match e.entry_method {
            EntryMethod::Quick => "quick",
            EntryMethod::Timer => "timer",
        },
    );
    row.into_bytes()
}

/// True if a CSV field, written verbatim, would be interpreted as a *formula*
/// when the file is opened in Excel / Google Sheets — i.e. it begins with
/// `=`, `+`, `-`, `@`, or a tab / CR / LF. Such fields get a leading `'`
/// (invisible in Excel) so they're read as literal text. This matters because
/// the export CSV gets emailed out — a client name like `=cmd|'…'!A1` or a
/// narrative `=WEBSERVICE("http://…")` must not execute on the recipient's box.
fn starts_dangerous(s: &str) -> bool {
    matches!(
        s.as_bytes().first(),
        Some(b'=' | b'+' | b'-' | b'@' | b'\t' | b'\r' | b'\n')
    )
}

/// Escape a value for one CSV field: spreadsheet formula-injection guard
/// (`starts_dangerous` → prefix `'`), then RFC-4180 quoting (wrap in `"…"` and
/// double inner `"`) if it contains a comma / quote / newline. Shared by the
/// append path (`format_row`) and the read-modify-rewrite path in `live_view`.
pub fn csv_escape(s: &str) -> String {
    let mut field = if starts_dangerous(s) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        field = format!("\"{}\"", field.replace('"', "\"\""));
    }
    field
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fake_entry() -> Entry {
        Entry {
            timestamp: Local.with_ymd_and_hms(2026, 5, 8, 15, 23, 11).unwrap(),
            staff: "Ryan".to_string(),
            client: "ClientCo".to_string(),
            engagement: "K-1 questions".to_string(),
            narrative: "phone call".to_string(),
            minutes: 18,
            billable: true,
            entry_method: EntryMethod::Quick,
            workstream_id: Some("ws-0123456789abcdef".to_string()),
        }
    }

    #[test]
    fn row_basic_shape() {
        let row = format_row(&fake_entry());
        let s = std::str::from_utf8(&row).unwrap();
        assert!(s.ends_with("\r\n"));
        // v0.2: 10 columns -> 9 commas (then CRLF).
        let comma_count = s[..s.len() - 2].chars().filter(|c| *c == ',').count();
        assert_eq!(comma_count, 9, "expected 9 commas, got {comma_count} in {s:?}");
        assert!(s.contains(",timer,ws-0123456789abcdef\r\n") || s.contains(",quick,ws-0123456789abcdef\r\n"),
            "workstream_id should be the trailing column in {s:?}");
    }

    #[test]
    fn legacy_row_has_nine_columns_no_workstream_id() {
        let row = format_row_legacy(&fake_entry());
        let s = std::str::from_utf8(&row).unwrap();
        let comma_count = s[..s.len() - 2].chars().filter(|c| *c == ',').count();
        assert_eq!(comma_count, 8, "v0.1 row is 9 columns");
        assert!(!s.contains("ws-0123456789abcdef"), "no workstream_id in legacy row");
    }

    #[test]
    fn row_decimal_rounding_to_two_places() {
        let mut e = fake_entry();
        e.minutes = 18; // 18/60 = 0.3
        let s = String::from_utf8(format_row(&e)).unwrap();
        assert!(s.contains(",18,0.30,"), "expected 0.30 in {s:?}");

        e.minutes = 17; // 17/60 = 0.2833... -> 0.28
        let s = String::from_utf8(format_row(&e)).unwrap();
        assert!(s.contains(",17,0.28,"), "expected 0.28 in {s:?}");

        e.minutes = 90; // 1.5
        let s = String::from_utf8(format_row(&e)).unwrap();
        assert!(s.contains(",90,1.50,"), "expected 1.50 in {s:?}");
    }

    #[test]
    fn row_ends_with_crlf() {
        let row = format_row(&fake_entry());
        assert_eq!(&row[row.len() - 2..], b"\r\n");
    }

    #[test]
    fn row_billable_true_false_strings() {
        let mut e = fake_entry();
        e.billable = true;
        assert!(String::from_utf8_lossy(&format_row(&e)).contains(",true,"));
        e.billable = false;
        assert!(String::from_utf8_lossy(&format_row(&e)).contains(",false,"));
    }

    #[test]
    fn row_entry_method_lowercase() {
        let mut e = fake_entry();
        e.entry_method = EntryMethod::Quick;
        assert!(String::from_utf8_lossy(&format_row(&e)).contains(",quick"));
        e.entry_method = EntryMethod::Timer;
        assert!(String::from_utf8_lossy(&format_row(&e)).contains(",timer"));
    }

    #[test]
    fn csv_escape_no_quoting_for_plain() {
        assert_eq!(csv_escape("hello"), "hello");
        assert_eq!(csv_escape(""), "");
    }

    #[test]
    fn csv_escape_quotes_when_comma() {
        assert_eq!(csv_escape("a, b"), "\"a, b\"");
    }

    #[test]
    fn csv_escape_doubles_quote_inside_quoted() {
        assert_eq!(csv_escape("she said \"hi\""), "\"she said \"\"hi\"\"\"");
    }

    #[test]
    fn csv_escape_quotes_when_newline() {
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
    }

    #[test]
    fn csv_escape_neutralizes_formula_leads() {
        // Excel/Sheets formula-injection guard: a leading =,+,-,@,tab,CR gets a ' prefix.
        assert_eq!(csv_escape("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(csv_escape("=1+1"), "'=1+1");
        assert_eq!(csv_escape("+SUM(A1)"), "'+SUM(A1)");
        assert_eq!(csv_escape("@cmd"), "'@cmd");
        assert_eq!(csv_escape("- bullet point"), "'- bullet point");
        assert_eq!(csv_escape("\tTabLed"), "'\tTabLed");
        // ...but normal text and ordinary leading digits/letters are untouched.
        assert_eq!(csv_escape("Acme Corp"), "Acme Corp");
        assert_eq!(csv_escape("2026-05-09"), "2026-05-09");
        assert_eq!(csv_escape("60"), "60");
        assert_eq!(csv_escape(""), "");
    }

    #[test]
    fn row_with_formula_client_is_neutralized() {
        let mut e = fake_entry();
        e.client = "=WEBSERVICE(\"http://evil\")".to_string();
        let s = String::from_utf8(format_row(&e)).unwrap();
        assert!(s.contains(",\"'=WEBSERVICE("), "client formula should be quoted + ' prefixed in {s:?}");
    }

    #[test]
    fn row_narrative_with_comma_is_quoted() {
        let mut e = fake_entry();
        e.narrative = "phone re K-1, asked re depreciation".to_string();
        let s = String::from_utf8(format_row(&e)).unwrap();
        assert!(
            s.contains("\"phone re K-1, asked re depreciation\""),
            "expected quoted narrative in {s:?}"
        );
    }

    // ----- atomic write + header tests (use tempdir to isolate) -----

    // Each test gets its own subdirectory so cargo's parallel runner can't
    // race on `gtt-test-{pid}/test.csv` (the previous shared layout caused
    // `remove_file` in one test to ENOENT a `rename` in another).
    fn with_temp_path<F: FnOnce(&Path)>(tag: &str, f: F) {
        let dir = std::env::temp_dir().join(format!("gtt-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.csv");
        let _ = std::fs::remove_file(&path);
        f(&path);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn append_atomic_writes_bom_banner_header_on_create() {
        with_temp_path("create", |p| {
            let row = append_atomic(p, &fake_entry()).unwrap();
            let content = std::fs::read(p).unwrap();
            assert_eq!(&content[..3], &UTF8_BOM, "BOM at file start");
            // v0.2 file: BOM, then `# time-tracker v0.2`, then the header.
            assert!(content[3..].starts_with(VERSION_COMMENT), "version banner after BOM");
            assert!(content[3 + VERSION_COMMENT.len()..].starts_with(HEADER), "header after banner");
            assert!(content.ends_with(&row), "row at the end");
            assert_eq!(&row, &format_row(&fake_entry()), "returned row is the v0.2 row");
        });
    }

    #[test]
    fn append_atomic_no_duplicate_header_on_existing() {
        with_temp_path("no_dup_header", |p| {
            let row1 = append_atomic(p, &fake_entry()).unwrap();
            let row2 = append_atomic(p, &fake_entry()).unwrap();
            let content = std::fs::read(p).unwrap();
            let count_of = |needle: &[u8]| {
                let (mut count, mut idx) = (0usize, 0usize);
                while let Some(pos) = content[idx..].windows(needle.len()).position(|w| w == needle) {
                    count += 1;
                    idx += pos + 1;
                }
                count
            };
            assert_eq!(count_of(HEADER), 1, "header exactly once");
            assert_eq!(count_of(VERSION_COMMENT), 1, "version banner exactly once");
            assert!(content.windows(row1.len()).any(|w| w == row1.as_slice()));
            assert!(content.ends_with(&row2));
        });
    }

    #[test]
    fn append_to_existing_v01_file_stays_nine_columns() {
        with_temp_path("v01_compat", |p| {
            // Simulate a v0.1 monthly CSV already on disk (BOM + 9-col header).
            let mut seed = Vec::new();
            seed.extend_from_slice(&UTF8_BOM);
            seed.extend_from_slice(HEADER_V01);
            std::fs::write(p, &seed).unwrap();
            let row = append_atomic(p, &fake_entry()).unwrap();
            assert_eq!(&row, &format_row_legacy(&fake_entry()), "appended a 9-col row to a v0.1 file");
            let content = std::fs::read(p).unwrap();
            assert!(!content.windows(VERSION_COMMENT.len()).any(|w| w == VERSION_COMMENT), "no banner injected into a v0.1 file");
            assert!(content.ends_with(&row));
        });
    }

    #[test]
    fn verify_last_line_round_trip() {
        with_temp_path("verify_last_line", |p| {
            let row = append_atomic(p, &fake_entry()).unwrap();
            assert!(verify_last_line(p, &row).unwrap());
            let other = b"different,row,bytes\r\n";
            assert!(!verify_last_line(p, other).unwrap());
        });
    }

    // ----- rewrite path (B2): read-modify-rewrite serializes with append --

    #[test]
    fn rewrite_path_creates_when_absent_and_round_trips() {
        with_temp_path("rewrite_create", |p| {
            // File absent -> transform gets None and can create fresh.
            let r = rewrite_path(
                p,
                Box::new(|cur| {
                    assert!(cur.is_none(), "absent file -> None");
                    Ok(b"\xEF\xBB\xBF# time-tracker v0.2\r\nheader\r\nrow1\r\n".to_vec())
                }),
            );
            assert!(matches!(r, RewriteResult::Written), "{r:?}");
            assert_eq!(std::fs::read(p).unwrap(), b"\xEF\xBB\xBF# time-tracker v0.2\r\nheader\r\nrow1\r\n");

            // Now the transform sees the existing bytes and rewrites.
            let r = rewrite_path(
                p,
                Box::new(|cur| {
                    let cur = cur.expect("present file -> Some");
                    let mut v = cur.to_vec();
                    v.extend_from_slice(b"row2\r\n");
                    Ok(v)
                }),
            );
            assert!(matches!(r, RewriteResult::Written), "{r:?}");
            assert!(std::fs::read(p).unwrap().ends_with(b"row2\r\n"));
        });
    }

    #[test]
    fn rewrite_path_propagates_transform_error() {
        with_temp_path("rewrite_err", |p| {
            std::fs::write(p, b"seed").unwrap();
            let r = rewrite_path(p, Box::new(|_| Err("no such row".to_string())));
            match r {
                RewriteResult::Error(m) => assert_eq!(m, "no such row"),
                other => panic!("expected Error, got {other:?}"),
            }
            // File untouched by a rejected transform.
            assert_eq!(std::fs::read(p).unwrap(), b"seed");
        });
    }

    #[test]
    fn append_and_two_rewrites_serialize_without_corruption() {
        // Two `Request::Rewrite`s + one `Request::Write` all go through the
        // single writer thread; the file stays well-formed and every
        // mutation lands. (B2 acceptance, in miniature — uses the env-var
        // override so the writer thread's `paths::csv_dir()` points at a
        // private temp dir for this test.)
        let dir = std::env::temp_dir().join(format!("gtt-rewrite-serial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Save/restore the override so concurrent tests aren't disturbed
        // (this test is the only one that touches these vars).
        let prev_csv = std::env::var("TIMETRACKER_CSV_DIR_OVERRIDE").ok();
        let prev_data = std::env::var("TIMETRACKER_DATA_DIR_OVERRIDE").ok();
        std::env::set_var("TIMETRACKER_CSV_DIR_OVERRIDE", &dir);
        std::env::set_var("TIMETRACKER_DATA_DIR_OVERRIDE", dir.join("data"));

        let writer = spawn(Arc::new(Usage::open()));
        let stem = "2099-01";
        let path = dir.join(format!("{stem}.csv"));

        // rewrite 1: create the file fresh.
        let r = writer.rewrite_blocking(
            stem,
            Box::new(|cur| {
                assert!(cur.is_none());
                Ok(b"\xEF\xBB\xBF# time-tracker v0.2\r\nh\r\n".to_vec())
            }),
        );
        assert!(matches!(r, RewriteResult::Written), "{r:?}");

        // rewrite 2: append a logical row.
        let r = writer.rewrite_blocking(
            stem,
            Box::new(|cur| {
                let mut v = cur.expect("file present").to_vec();
                v.extend_from_slice(b"a,b,c\r\n");
                Ok(v)
            }),
        );
        assert!(matches!(r, RewriteResult::Written), "{r:?}");

        // a Write to the same month appends via the append path.
        let mut e = fake_entry();
        e.timestamp = Local.with_ymd_and_hms(2099, 1, 15, 9, 0, 0).unwrap();
        let w = writer.write_blocking(e);
        assert!(matches!(w, WriteResult::Written), "{w:?}");

        // rewrite 3 (after the append): sees the appended row.
        let r = writer.rewrite_blocking(
            stem,
            Box::new(|cur| {
                let v = cur.expect("file present").to_vec();
                // Must contain the rewrite-added row AND the writer-thread row.
                let s = String::from_utf8_lossy(&v);
                assert!(s.contains("a,b,c"), "rewrite-2 row survived: {s:?}");
                assert!(s.contains("ClientCo"), "append row present: {s:?}");
                Ok(v)
            }),
        );
        assert!(matches!(r, RewriteResult::Written), "{r:?}");

        let final_bytes = std::fs::read(&path).unwrap();
        assert!(final_bytes.starts_with(&UTF8_BOM), "BOM intact");
        let s = String::from_utf8_lossy(&final_bytes);
        assert!(s.contains("# time-tracker v0.2"), "banner intact");

        // restore env
        match prev_csv {
            Some(v) => std::env::set_var("TIMETRACKER_CSV_DIR_OVERRIDE", v),
            None => std::env::remove_var("TIMETRACKER_CSV_DIR_OVERRIDE"),
        }
        match prev_data {
            Some(v) => std::env::set_var("TIMETRACKER_DATA_DIR_OVERRIDE", v),
            None => std::env::remove_var("TIMETRACKER_DATA_DIR_OVERRIDE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
