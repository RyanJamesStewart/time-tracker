// SPEC.md §3.3 duration parser.
//
// Accepts user-typed duration strings and returns minutes.
//
// Accepted forms:
//   "0.3"          -> 18 minutes  (decimal hours)
//   "0.25", "1.5"  -> ditto
//   "18m", "90m"   -> minutes
//   "1h", "2h"     -> hours
//   "1h30m"        -> 90 minutes
//   "1h 30m"       -> 90 minutes  (whitespace tolerated)
//   "1:15"         -> 75 minutes  (h:mm)
//
// Explicit rejects (per SPEC §3.3):
//   "1:15:30"      -> too granular
//   ""             -> empty
//   "abc"          -> garbage
//   "0", "0.0"     -> zero (almost always a typo)
//   "00:00"        -> zero
//   "-0.5"         -> negative
//   ">24h"         -> sanity cap; a single entry > 24h is almost always wrong

use std::fmt;

const MAX_MINUTES: u32 = 24 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a duration string. Returns total minutes.
pub fn parse(input: &str) -> Result<u32, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(err("empty"));
    }
    if s.starts_with('-') {
        return Err(err("negative not allowed"));
    }

    let minutes = parse_inner(s).ok_or_else(|| err(&format!("could not parse {s:?}")))?;

    if minutes == 0 {
        return Err(err("must be > 0"));
    }
    if minutes > MAX_MINUTES {
        return Err(err("must be <= 24h"));
    }
    Ok(minutes)
}

fn parse_inner(s: &str) -> Option<u32> {
    // h:mm form
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 2 {
            return None;
        }
        let h: u32 = parts[0].parse().ok()?;
        let m: u32 = parts[1].parse().ok()?;
        if m >= 60 {
            return None;
        }
        return Some(h * 60 + m);
    }

    // Hh / Mm / HhMm forms (case-insensitive, whitespace tolerated)
    let lower = s.to_ascii_lowercase();
    if lower.contains('h') || lower.contains('m') {
        return parse_hm(&lower);
    }

    // Pure number = decimal hours
    let hours: f64 = s.parse().ok()?;
    if !hours.is_finite() || hours < 0.0 {
        return None;
    }
    Some((hours * 60.0).round() as u32)
}

fn parse_hm(s: &str) -> Option<u32> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut hours = 0u32;
    let mut minutes = 0u32;
    let mut buf = String::new();
    let mut saw_h = false;
    let mut saw_m = false;
    for c in cleaned.chars() {
        if c.is_ascii_digit() {
            buf.push(c);
        } else if c == 'h' {
            if buf.is_empty() || saw_h {
                return None;
            }
            hours = buf.parse().ok()?;
            buf.clear();
            saw_h = true;
        } else if c == 'm' {
            if buf.is_empty() || saw_m {
                return None;
            }
            minutes = buf.parse().ok()?;
            buf.clear();
            saw_m = true;
        } else {
            return None;
        }
    }
    if !buf.is_empty() {
        return None;
    }
    if !saw_h && !saw_m {
        return None;
    }
    Some(hours * 60 + minutes)
}

fn err(msg: &str) -> ParseError {
    ParseError(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> u32 {
        parse(s).unwrap_or_else(|e| panic!("expected ok for {s:?}, got {e}"))
    }
    fn err_case(s: &str) {
        assert!(parse(s).is_err(), "expected error for {s:?}");
    }

    // ----- accepted -----
    #[test] fn decimal_03() { assert_eq!(ok("0.3"), 18); }
    #[test] fn decimal_025() { assert_eq!(ok("0.25"), 15); }
    #[test] fn decimal_15() { assert_eq!(ok("1.5"), 90); }
    #[test] fn decimal_175() { assert_eq!(ok("1.75"), 105); }
    #[test] fn minutes_18m() { assert_eq!(ok("18m"), 18); }
    #[test] fn minutes_90m() { assert_eq!(ok("90m"), 90); }
    #[test] fn hours_1h() { assert_eq!(ok("1h"), 60); }
    #[test] fn hours_2h() { assert_eq!(ok("2h"), 120); }
    #[test] fn hours_1h30m() { assert_eq!(ok("1h30m"), 90); }
    #[test] fn hours_1h_space_30m() { assert_eq!(ok("1h 30m"), 90); }
    #[test] fn hmm_1_15() { assert_eq!(ok("1:15"), 75); }
    #[test] fn hmm_2_30() { assert_eq!(ok("2:30"), 150); }

    // case-insensitive
    #[test] fn upper_h() { assert_eq!(ok("1H"), 60); }
    #[test] fn upper_hm() { assert_eq!(ok("1H30M"), 90); }

    // trim whitespace
    #[test] fn leading_trailing_ws() { assert_eq!(ok("  1h  "), 60); }

    // ----- rejected -----
    #[test] fn reject_hms() { err_case("1:15:30"); }
    #[test] fn reject_empty() { err_case(""); }
    #[test] fn reject_whitespace_only() { err_case("   "); }
    #[test] fn reject_alpha() { err_case("abc"); }
    #[test] fn reject_zero_int() { err_case("0"); }
    #[test] fn reject_zero_decimal() { err_case("0.0"); }
    #[test] fn reject_zero_hmm() { err_case("00:00"); }
    #[test] fn reject_negative() { err_case("-0.5"); }
    #[test] fn reject_negative_int() { err_case("-1"); }
    #[test] fn reject_over_24h_decimal() { err_case("25"); }
    #[test] fn reject_over_24h_h() { err_case("25h"); }
    #[test] fn reject_minute_overflow_in_hmm() { err_case("1:60"); }
    #[test] fn reject_minute_overflow_99() { err_case("1:99"); }
    #[test] fn reject_no_unit_with_h_letter() { err_case("h"); }
    #[test] fn reject_trailing_garbage() { err_case("1h30"); }
    #[test] fn reject_double_h() { err_case("1h2h"); }
    #[test] fn reject_strange_separator() { err_case("1.30h"); /* would be 78 mins, but fmt is ambiguous */ }
}
