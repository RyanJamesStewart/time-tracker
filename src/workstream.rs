// v0.2 workstream registry — the persistent set of (client, engagement)
// pairs the user works against. New in v0.2: the popover's switcher list,
// the Recorded Time "+ New entry" client/engagement pickers, and the
// `workstream_id` stamped on each CSV row all draw from here.
//
// Storage: `workstreams.json` in `paths::data_dir()` (alongside
// `timer-state.json`). The v0.2 spec wrote "%LOCALAPPDATA%\TimeTracker\…"
// but the established layout puts app state under `%LOCALAPPDATA%\TimeTracker`
// (CSV exports live under `%USERPROFILE%\TimeTracker`); we keep state
// where the rest of it already lives.
//
// First run: if the file is absent, synthesize the registry from
// existing monthly CSVs — every distinct (client, engagement) pair
// becomes a workstream, `last_used_at` set to the newest entry for that
// pair. So a v0.1 → v0.2 upgrade has a populated registry with no manual
// entry.
//
// Read once on app start, hold in memory. Write on every change (add,
// pin/unpin, touch). Writes are atomic (`.tmp` + rename), same as the
// timer-state and CSV writers. Single-writer (the tracker app); the
// localhost server reads a snapshot via `GET /workstreams`.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "workstreams.json";
const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workstream {
    pub id: String,
    pub client: String,
    pub engagement: String,
    pub billable_default: bool,
    pub pinned: bool,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

impl Workstream {
    fn new(client: &str, engagement: &str, billable_default: bool, when: DateTime<Utc>) -> Self {
        Self {
            id: derive_id(client, engagement),
            client: client.trim().to_string(),
            engagement: engagement.trim().to_string(),
            billable_default,
            pinned: false,
            created_at: when,
            last_used_at: when,
        }
    }
}

/// Deterministic id for a (client, engagement) pair — stable across
/// re-scans of the CSVs, so a freshly-synthesized registry produces the
/// same ids the previous one had.
pub fn derive_id(client: &str, engagement: &str) -> String {
    let mut h = DefaultHasher::new();
    client.trim().hash(&mut h);
    0u8.hash(&mut h); // separator so ("ab","c") != ("a","bc")
    engagement.trim().hash(&mut h);
    format!("ws-{:016x}", h.finish())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkstreamRegistry {
    pub version: u32,
    pub workstreams: Vec<Workstream>,
}

impl WorkstreamRegistry {
    /// Load from disk; if absent or unparseable, synthesize from existing
    /// monthly CSVs (and write the synthesized registry back so the next
    /// load is a plain read). Always returns *something* usable.
    pub fn load() -> Self {
        let path = state_path();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(mut reg) = serde_json::from_str::<WorkstreamRegistry>(&s) {
                if reg.version == 0 {
                    reg.version = REGISTRY_VERSION;
                }
                tracing::info!(path = %path.display(), count = reg.workstreams.len(), "workstream registry loaded");
                return reg;
            }
            tracing::warn!(path = %path.display(), "workstreams.json unparseable; rebuilding from CSVs");
        }
        let reg = Self::synthesize_from_csvs();
        tracing::info!(count = reg.workstreams.len(), "workstream registry synthesized from CSVs");
        let _ = reg.persist();
        reg
    }

    fn synthesize_from_csvs() -> Self {
        // (client, engagement) -> (newest_ts, billable_of_newest)
        let mut seen: BTreeMap<(String, String), (DateTime<Utc>, bool)> = BTreeMap::new();
        let dir = crate::paths::csv_dir();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let name = ent.file_name();
                let name = name.to_string_lossy();
                if !name.ends_with(".csv") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(ent.path()) else {
                    continue;
                };
                for fields in csv_data_rows(&content) {
                    // CSV layout: <date|timestamp_iso>,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method[,workstream_id]
                    if fields.len() < 9 {
                        continue;
                    }
                    // v0.3 column 0 is a plain date; v0.1/v0.2 was an ISO
                    // timestamp. `parse_entry_date` reads both; we only use it
                    // to order workstreams by recency, so midnight-of-the-date
                    // (or `now` on garbage) is plenty of precision.
                    let ts = crate::datefmt::parse_entry_date(&fields[0])
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                        .map(|ndt| Utc.from_utc_datetime(&ndt))
                        .unwrap_or_else(Utc::now);
                    let client = fields[2].trim().to_string();
                    let engagement = fields[3].trim().to_string();
                    if client.is_empty() {
                        continue;
                    }
                    let billable = fields[7].eq_ignore_ascii_case("true");
                    let e = seen.entry((client, engagement)).or_insert((ts, billable));
                    if ts >= e.0 {
                        *e = (ts, billable);
                    }
                }
            }
        }
        let mut workstreams: Vec<Workstream> = seen
            .into_iter()
            .map(|((client, engagement), (ts, billable))| Workstream {
                id: derive_id(&client, &engagement),
                client,
                engagement,
                billable_default: billable,
                pinned: false,
                created_at: ts,
                last_used_at: ts,
            })
            .collect();
        // Most-recently-used first — that's the order the popover wants.
        workstreams.sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
        Self {
            version: REGISTRY_VERSION,
            workstreams,
        }
    }

    pub fn find(&self, client: &str, engagement: &str) -> Option<&Workstream> {
        let id = derive_id(client, engagement);
        self.workstreams.iter().find(|w| w.id == id)
    }

    /// Return the id for (client, engagement), creating the workstream if
    /// it doesn't exist yet, and bumping `last_used_at` to now. Returns
    /// `true` in the second slot if a new workstream was created.
    pub fn touch_or_create(
        &mut self,
        client: &str,
        engagement: &str,
        billable_default: bool,
    ) -> (String, bool) {
        let id = derive_id(client, engagement);
        let now = Utc::now();
        if let Some(w) = self.workstreams.iter_mut().find(|w| w.id == id) {
            w.last_used_at = now;
            return (id, false);
        }
        self.workstreams
            .push(Workstream::new(client, engagement, billable_default, now));
        (id, true)
    }

    /// Add a workstream explicitly (the popover's add flow). If it already
    /// exists, this is a touch. Returns the workstream and whether it was
    /// newly created.
    pub fn add(&mut self, client: &str, engagement: &str, billable_default: bool) -> (Workstream, bool) {
        let (id, created) = self.touch_or_create(client, engagement, billable_default);
        let w = self.workstreams.iter().find(|w| w.id == id).cloned().expect("just inserted");
        (w, created)
    }

    pub fn set_pinned(&mut self, id: &str, pinned: bool) -> bool {
        if let Some(w) = self.workstreams.iter_mut().find(|w| w.id == id) {
            w.pinned = pinned;
            true
        } else {
            false
        }
    }

    /// Workstreams in display order: pinned first, then by recency.
    pub fn ordered(&self) -> Vec<&Workstream> {
        let mut v: Vec<&Workstream> = self.workstreams.iter().collect();
        v.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.last_used_at.cmp(&a.last_used_at))
        });
        v
    }

    pub fn persist(&self) -> std::io::Result<()> {
        let path = state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn state_path() -> PathBuf {
    crate::paths::data_dir().join(STATE_FILE)
}

/// Yield the data rows (parsed fields) of a CSV string, skipping the BOM,
/// `#`-prefixed comment lines (v0.2 version banner), and the header.
/// Minimal RFC-4180-ish reader: double-quoted fields with doubled inner
/// quotes. Mirrors `live_view::split_csv_line`.
fn csv_data_rows(content: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut header_skipped = false;
    for line in content.lines() {
        let line = line.trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !header_skipped {
            header_skipped = true; // first non-comment line is the header
            continue;
        }
        out.push(split_csv_line(line));
    }
    out
}

fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_id_is_stable_and_distinct() {
        assert_eq!(derive_id("Halden & Roe", "Q1 Review"), derive_id("Halden & Roe", "Q1 Review"));
        assert_eq!(derive_id(" Halden & Roe ", "Q1 Review"), derive_id("Halden & Roe", "Q1 Review"));
        assert_ne!(derive_id("ab", "c"), derive_id("a", "bc"));
        assert!(derive_id("X", "Y").starts_with("ws-"));
    }

    #[test]
    fn touch_or_create_inserts_then_touches() {
        let mut reg = WorkstreamRegistry { version: 1, workstreams: vec![] };
        let (id1, created1) = reg.touch_or_create("Acme", "Audit", true);
        assert!(created1);
        assert_eq!(reg.workstreams.len(), 1);
        let first_used = reg.workstreams[0].last_used_at;
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (id2, created2) = reg.touch_or_create("Acme", "Audit", true);
        assert!(!created2);
        assert_eq!(id1, id2);
        assert_eq!(reg.workstreams.len(), 1);
        assert!(reg.workstreams[0].last_used_at >= first_used);
    }

    #[test]
    fn pin_then_ordered_puts_pinned_first() {
        let mut reg = WorkstreamRegistry { version: 1, workstreams: vec![] };
        let (a, _) = reg.touch_or_create("A", "x", true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (_b, _) = reg.touch_or_create("B", "y", true); // newer
        assert!(reg.set_pinned(&a, true));
        let order = reg.ordered();
        assert_eq!(order[0].id, a, "pinned A should come before newer B");
    }

    #[test]
    fn csv_data_rows_skips_bom_comment_and_header() {
        let csv = "\u{feff}# time-tracker v0.2\r\ntimestamp_iso,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method,workstream_id\r\n2026-05-11T09:00:00-07:00,RyanJ,Acme,Audit,work,30,0.50,true,timer,ws-deadbeef\r\n";
        let rows = csv_data_rows(csv);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][2], "Acme");
        assert_eq!(rows[0][3], "Audit");
        assert_eq!(rows[0].len(), 10);
    }

    #[test]
    fn csv_data_rows_handles_v01_without_comment_line() {
        let csv = "\u{feff}timestamp_iso,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method\r\n2026-05-11T09:00:00-07:00,RyanJ,Acme,Audit,\"work, more\",30,0.50,true,timer\r\n";
        let rows = csv_data_rows(csv);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][4], "work, more");
        assert_eq!(rows[0].len(), 9);
    }
}
