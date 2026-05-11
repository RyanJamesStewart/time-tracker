// SPEC §5.4 usage instrumentation - JSONL events to %LOCALAPPDATA%\TimeTracker\usage.log.
//
// The Part 2 pitch ("your team logged N entries last week, here's the
// time you'd save with email automation") depends on this data. No
// content fields are recorded - just structural metadata (event type,
// timestamps, counts, lengths). Per SPEC §4.4 PII scrubbing.
//
// Events emitted by various subsystems:
//   app_start          {ts, version}
//   app_exit           {ts}
//   hotkey_registered  {ts, which, ok}      <- one per hotkey at startup
//                                              (a registration *outcome* — NOT
//                                              a keypress; kept separate so
//                                              `hotkey_fire` only ever counts
//                                              real presses; R5, 2026-05-11)
//   hotkey_fire        {ts, hotkey, success} <- emitted only on an actual press
//   popup_open         {ts, source: hotkey|menu|ipc}
//   popup_close        {ts, action: submit|cancel, duration_ms}
//   entry_written      {ts, method, minutes, billable}
//   entry_queued       {ts, reason: csv_locked}
//   queue_drain        {ts, count}
//   crash              {ts}

use chrono::Local;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Usage {
    file: Mutex<Option<File>>,
}

impl Usage {
    pub fn open() -> Self {
        let path = path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path).ok();
        if file.is_none() {
            tracing::warn!(path = %path.display(), "could not open usage.log; instrumentation disabled");
        }
        Self {
            file: Mutex::new(file),
        }
    }

    fn emit(&self, mut event: Value) {
        if let Some(obj) = event.as_object_mut() {
            obj.insert("ts".to_string(), json!(Local::now().to_rfc3339()));
        }
        let line = format!("{event}\n");
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    pub fn app_start(&self) {
        self.emit(json!({"event": "app_start", "version": env!("CARGO_PKG_VERSION")}));
    }

    pub fn app_exit(&self) {
        self.emit(json!({"event": "app_exit"}));
    }

    pub fn popup_open(&self, source: &str) {
        self.emit(json!({"event": "popup_open", "source": source}));
    }

    pub fn popup_close(&self, action: &str, duration_ms: u128) {
        self.emit(json!({"event": "popup_close", "action": action, "duration_ms": duration_ms}));
    }

    pub fn entry_written(&self, method: &str, minutes: u32, billable: bool) {
        self.emit(json!({
            "event": "entry_written",
            "method": method,
            "minutes": minutes,
            "billable": billable,
        }));
    }

    pub fn entry_queued(&self, reason: &str) {
        self.emit(json!({"event": "entry_queued", "reason": reason}));
    }

    pub fn hotkey_fire(&self, hotkey: &str, success: bool) {
        self.emit(json!({"event": "hotkey_fire", "hotkey": hotkey, "success": success}));
    }

    /// Startup-time registration *outcome* for one hotkey. Distinct from
    /// `hotkey_fire` (which is a real keypress) so the wedge data isn't
    /// polluted by ~4 phantom "fires" every launch (R5).
    pub fn hotkey_registered(&self, which: &str, ok: bool) {
        self.emit(json!({"event": "hotkey_registered", "which": which, "ok": ok}));
    }

    pub fn queue_drain(&self, count: usize) {
        self.emit(json!({"event": "queue_drain", "count": count}));
    }

    /// Best-effort emit from the panic hook. Process is dying, so we
    /// ignore any errors - if the file isn't already open we don't try
    /// to open it. The event line is short and the OS has likely already
    /// flushed the appender buffer by the time the hook runs.
    pub fn crash(&self) {
        self.emit(json!({"event": "crash"}));
    }
}

fn path() -> PathBuf {
    crate::paths::data_dir().join("usage.log")
}
