// SPEC.md §3.6 autocomplete.
//
// Source: distinct `client` and `engagement` values seen in the last
// 90 days across all monthly CSV files. Cached as JSON to
// %LOCALAPPDATA%\TimeTracker\autocomplete.json, rebuilt on app start and
// after each entry written.
//
// Ranking: case-insensitive prefix matches first, then case-insensitive
// substring matches. Top 5 returned.

use chrono::{DateTime, Duration, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

const WINDOW_DAYS: i64 = 90;
const MAX_SUGGESTIONS: usize = 5;
const CACHE_FILE: &str = "autocomplete.json";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Cache {
    pub clients: Vec<String>,
    pub engagements: Vec<String>,
}

impl Cache {
    /// Load the cache from disk, or return Default if missing/unparseable.
    pub fn load() -> Self {
        let path = crate::paths::data_dir().join(CACHE_FILE);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist the cache atomically (temp + rename).
    pub fn save(&self) -> std::io::Result<()> {
        let path = crate::paths::data_dir().join(CACHE_FILE);
        let parent = path.parent().expect("data_dir always has parent");
        std::fs::create_dir_all(parent)?;
        let temp = parent.join(".autocomplete.json.tmp");
        let json = serde_json::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&temp, json)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    /// Rank candidates by case-insensitive prefix-then-substring.
    /// Top 5 returned. Empty query returns the first 5 known values.
    pub fn rank_clients(&self, query: &str) -> Vec<String> {
        rank(&self.clients, query)
    }
    pub fn rank_engagements(&self, query: &str) -> Vec<String> {
        rank(&self.engagements, query)
    }

    /// Add observed values (typically called after a CSV write) and
    /// persist. Idempotent: duplicates collapse via the BTreeSet.
    pub fn observe(&mut self, client: &str, engagement: &str) -> std::io::Result<()> {
        observe_one(&mut self.clients, client);
        if !engagement.is_empty() {
            observe_one(&mut self.engagements, engagement);
        }
        self.save()
    }
}

fn observe_one(values: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return;
    }
    if values.iter().any(|v| v.eq_ignore_ascii_case(trimmed)) {
        return;
    }
    values.push(trimmed.to_string());
    values.sort_by_key(|s| s.to_lowercase());
}

fn rank(items: &[String], query: &str) -> Vec<String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return items.iter().take(MAX_SUGGESTIONS).cloned().collect();
    }

    let mut prefix_matches: Vec<&String> = items
        .iter()
        .filter(|s| s.to_lowercase().starts_with(&q))
        .collect();
    let prefix_set: std::collections::HashSet<&String> = prefix_matches.iter().copied().collect();
    let substring_matches: Vec<&String> = items
        .iter()
        .filter(|s| {
            !prefix_set.contains(s) && s.to_lowercase().contains(&q)
        })
        .collect();

    prefix_matches.extend(substring_matches);
    prefix_matches
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .cloned()
        .collect()
}

/// Full rebuild from CSV files in the csv_dir. Scans every file matching
/// `YYYY-MM.csv`, parses each row, and collects distinct values from
/// rows whose `timestamp_iso` falls inside the 90-day window.
pub fn rebuild_from_csvs() -> std::io::Result<Cache> {
    let mut clients: BTreeSet<String> = BTreeSet::new();
    let mut engagements: BTreeSet<String> = BTreeSet::new();
    let cutoff = Local::now() - Duration::days(WINDOW_DAYS);

    let csv_dir = crate::paths::csv_dir();
    if !csv_dir.exists() {
        let cache = Cache::default();
        let _ = cache.save();
        return Ok(cache);
    }

    for entry in std::fs::read_dir(&csv_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "csv") {
            continue;
        }
        let _ = scan_csv(&path, cutoff, &mut clients, &mut engagements);
    }

    let cache = Cache {
        clients: clients.into_iter().collect(),
        engagements: engagements.into_iter().collect(),
    };
    let _ = cache.save();
    Ok(cache)
}

fn scan_csv(
    path: &Path,
    cutoff: DateTime<Local>,
    clients: &mut BTreeSet<String>,
    engagements: &mut BTreeSet<String>,
) -> std::io::Result<()> {
    // The CSV files are small and we can read them whole. Skip BOM if
    // present, skip the header row, then parse each remaining line.
    let mut content = std::fs::read(path)?;
    if content.starts_with(&[0xEF, 0xBB, 0xBF]) {
        content.drain(0..3);
    }
    let s = String::from_utf8_lossy(&content);
    let mut lines = s.lines();
    // Skip header.
    let _ = lines.next();
    for line in lines {
        if let Some((ts, client, engagement)) = parse_minimal_row(line) {
            if let Ok(parsed_ts) = DateTime::parse_from_rfc3339(ts) {
                if parsed_ts.with_timezone(&Local) < cutoff {
                    continue;
                }
                let c = client.trim();
                if !c.is_empty() {
                    clients.insert(c.to_string());
                }
                let e = engagement.trim();
                if !e.is_empty() {
                    engagements.insert(e.to_string());
                }
            }
        }
    }
    Ok(())
}

/// Minimal CSV row parser scoped to extracting `timestamp_iso`,
/// `client`, `engagement` from columns 0/2/3. Handles RFC 4180
/// quoting (double-quote escape, embedded commas inside quotes).
/// Returns None if the row has fewer than 4 fields.
fn parse_minimal_row(line: &str) -> Option<(&str, &str, &str)> {
    let bytes = line.as_bytes();
    let mut fields: Vec<(usize, usize)> = Vec::with_capacity(9);
    let mut start = 0usize;
    let mut in_quotes = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' => {
                if in_quotes && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_quotes = !in_quotes;
            }
            b',' if !in_quotes => {
                fields.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    fields.push((start, bytes.len()));
    if fields.len() < 4 {
        return None;
    }
    let ts = std::str::from_utf8(&bytes[fields[0].0..fields[0].1]).ok()?;
    let client = std::str::from_utf8(&bytes[fields[2].0..fields[2].1]).ok()?;
    let engagement = std::str::from_utf8(&bytes[fields[3].0..fields[3].1]).ok()?;
    // Strip surrounding quotes if present (and unescape doubled quotes).
    Some((ts, strip_quotes(client), strip_quotes(engagement)))
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_empty_returns_first_5() {
        let items = vec!["A", "B", "C", "D", "E", "F"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        assert_eq!(rank(&items, ""), vec!["A", "B", "C", "D", "E"]);
    }

    #[test]
    fn rank_prefix_before_substring() {
        let items: Vec<String> = vec!["Acme Client", "ClientCo", "MyClient"]
            .into_iter()
            .map(String::from)
            .collect();
        let r = rank(&items, "Cl");
        assert_eq!(r[0], "ClientCo", "prefix match ranks first");
        // Then the substring matches (Acme Client, MyClient)
        assert!(r.contains(&"Acme Client".to_string()));
        assert!(r.contains(&"MyClient".to_string()));
    }

    #[test]
    fn rank_case_insensitive() {
        let items: Vec<String> = vec!["ClientCo", "DEFCorp"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(rank(&items, "client"), vec!["ClientCo".to_string()]);
        assert_eq!(rank(&items, "DEF"), vec!["DEFCorp".to_string()]);
        assert_eq!(rank(&items, "def"), vec!["DEFCorp".to_string()]);
    }

    #[test]
    fn rank_caps_at_5() {
        let items: Vec<String> = (0..20).map(|i| format!("Client{i:02}")).collect();
        let r = rank(&items, "Client");
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn observe_dedupes_case_insensitive() {
        let mut v: Vec<String> = vec!["ClientCo".to_string()];
        observe_one(&mut v, "clientco"); // dup
        observe_one(&mut v, "Acme");
        observe_one(&mut v, "ClientCo"); // dup
        assert_eq!(v.len(), 2);
        assert!(v.contains(&"ClientCo".to_string()));
        assert!(v.contains(&"Acme".to_string()));
    }

    #[test]
    fn parse_minimal_unquoted() {
        let line = "2026-05-08T15:23:11-07:00,Ryan,ClientCo,K-1,phone,18,0.30,true,quick";
        let (ts, client, eng) = parse_minimal_row(line).unwrap();
        assert_eq!(ts, "2026-05-08T15:23:11-07:00");
        assert_eq!(client, "ClientCo");
        assert_eq!(eng, "K-1");
    }

    #[test]
    fn parse_minimal_quoted_with_comma() {
        let line = "2026-05-08T15:23:11-07:00,Ryan,\"ClientCo, LLC\",\"K-1, complex\",notes,18,0.30,true,quick";
        let (_ts, client, eng) = parse_minimal_row(line).unwrap();
        assert_eq!(client, "ClientCo, LLC");
        assert_eq!(eng, "K-1, complex");
    }

    #[test]
    fn parse_minimal_too_few_fields() {
        let line = "ts,staff,client";
        assert!(parse_minimal_row(line).is_none());
    }
}
