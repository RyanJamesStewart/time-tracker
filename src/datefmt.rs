//! Pure date/duration parsing shared by the CSV layer (`csv_writer`,
//! `workstream`) and the localhost API (`live_view`). It lives in the lib crate
//! on purpose: `cargo test --lib --target x86_64-unknown-linux-gnu` covers it,
//! and the v0.3 "column 0 is a plain date, not an ISO timestamp" change bit us
//! *twice* when this logic was duplicated and one copy still parsed an
//! `parse_from_rfc3339` — having one tested implementation closes that class
//! of bug.

use chrono::{DateTime, Local, NaiveDate};

/// A single recorded entry can't be longer than a full day — multi-session
/// days are just multiple entries. (The running timer itself caps elapsed at
/// 12h elsewhere; this is the manual-entry / patch ceiling.)
pub const MAX_ENTRY_MINUTES: i64 = 24 * 60;

/// Parse column 0 of a CSV data row into the entry's calendar date.
///
/// v0.3 writes a plain `YYYY-MM-DD`. v0.1/v0.2 files wrote an ISO timestamp
/// (`2026-05-09T14:03:00-07:00`, sometimes with sub-second precision) — accept
/// those too and take the date part, so a never-rewritten legacy monthly file
/// keeps reading. `None` only on genuine garbage.
pub fn parse_entry_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Local).date_naive())
}

/// Parse a duration the user typed in a Duration cell / the "+ New entry" row:
///   - `"H:MM"` / `"H:M"` — `"1:30"` → 90, `"1:5"` → 65
///   - a bare integer — `"90"` → 90 *minutes*
///   - a decimal — `"1.5"` → 90 (hours)
///   - an `h` suffix — `"2h"` → 120, `"1.5h"` → 90 (always hours)
///
/// Returns whole minutes, clamped to `[0, MAX_ENTRY_MINUTES]`. `None` on
/// anything that isn't one of those shapes ("1.5 hrs", "lunch", …).
pub fn parse_hm_dur(s: &str) -> Option<i64> {
    let raw = s.trim();
    if raw.is_empty() {
        return None;
    }
    let had_h = raw.ends_with(['h', 'H']);
    let s = raw.trim_end_matches(['h', 'H']).trim();
    if s.is_empty() {
        return None;
    }
    let mins: i64 = if let Some((h_str, m_str)) = s.split_once(':') {
        let h: i64 = h_str.trim().parse().ok()?;
        let m: i64 = m_str.trim().parse().ok()?;
        if !(0..=59).contains(&m) || h < 0 {
            return None;
        }
        h * 60 + m
    } else if had_h {
        // "2h" / "1.5h" → interpret the number as hours
        let hrs: f64 = s.parse().ok()?;
        if !hrs.is_finite() || hrs < 0.0 {
            return None;
        }
        (hrs * 60.0).round() as i64
    } else if let Ok(m) = s.parse::<i64>() {
        m // bare integer → minutes
    } else if let Ok(hrs) = s.parse::<f64>() {
        if !hrs.is_finite() || hrs < 0.0 {
            return None;
        }
        (hrs * 60.0).round() as i64 // bare decimal → hours
    } else {
        return None;
    };
    if mins < 0 {
        return None;
    }
    Some(mins.min(MAX_ENTRY_MINUTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn parse_entry_date_accepts_v03_plain_date() {
        let d = parse_entry_date("2026-05-09").unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2026, 5, 9));
        assert_eq!(parse_entry_date("  2026-12-31 ").unwrap().day(), 31);
    }

    #[test]
    fn parse_entry_date_accepts_legacy_iso_timestamp() {
        // v0.1/v0.2 column-0 form — must still read, taking the date part.
        let d = parse_entry_date("2026-05-09T14:03:00-07:00").unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2026, 5, 9));
        // csv_writer used `to_rfc3339()`, which can emit sub-second precision.
        let d = parse_entry_date("2026-01-02T09:00:00.123456-08:00").unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2026, 1, 2));
    }

    #[test]
    fn parse_entry_date_rejects_garbage() {
        assert_eq!(parse_entry_date(""), None);
        assert_eq!(parse_entry_date("not a date"), None);
        assert_eq!(parse_entry_date("2026-13-40"), None);
        assert_eq!(parse_entry_date("05/09/2026"), None);
    }

    #[test]
    fn parse_hm_dur_forms() {
        assert_eq!(parse_hm_dur("1:30"), Some(90));
        assert_eq!(parse_hm_dur("1:5"), Some(65));
        assert_eq!(parse_hm_dur("0:45"), Some(45));
        assert_eq!(parse_hm_dur("90"), Some(90));
        assert_eq!(parse_hm_dur("1.5"), Some(90));
        assert_eq!(parse_hm_dur("1.5h"), Some(90));
        assert_eq!(parse_hm_dur("2h"), Some(120));
        assert_eq!(parse_hm_dur(" 2 H "), Some(120));
        assert_eq!(parse_hm_dur("0"), Some(0));
        assert_eq!(parse_hm_dur("0:00"), Some(0));
    }

    #[test]
    fn parse_hm_dur_rejects_and_clamps() {
        assert_eq!(parse_hm_dur("99:99"), None); // minutes out of range
        assert_eq!(parse_hm_dur("-5"), None);
        assert_eq!(parse_hm_dur("-1:30"), None);
        assert_eq!(parse_hm_dur("bananas"), None);
        assert_eq!(parse_hm_dur("1.5 hrs"), None);
        assert_eq!(parse_hm_dur(""), None);
        assert_eq!(parse_hm_dur("h"), None);
        assert_eq!(parse_hm_dur("100:00"), Some(MAX_ENTRY_MINUTES)); // clamp
        assert_eq!(parse_hm_dur("3000"), Some(MAX_ENTRY_MINUTES));
        assert_eq!(parse_hm_dur("50h"), Some(MAX_ENTRY_MINUTES));
    }
}
