// SPEC §5.3 config.toml schema (subset for v1).
//
// On first launch, write a default config.toml so the user can find
// and edit it. On subsequent launches, parse + apply. Parse errors
// log a warning and fall back to defaults rather than crashing - the
// app must always boot.
//
// v1 reads `identity.staff`, `defaults.billable`, AND (R2, 2026-05-11)
// `[hotkeys]` (rebind — overrides the hardcoded set when present) and
// `[startup].enabled` (false → the app won't autostart-arm itself; the
// Windows Settings → Startup Apps toggle is the other half). A full
// Settings *GUI* is still v1.1, but the config *wiring* is real now —
// previously these sections were silently ignored, which is worse than
// not having them (a hand-edit looked like it should work and didn't).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub identity: Identity,
    #[serde(default)]
    pub defaults: Defaults,
    /// Optional hotkey rebinds. Absent fields fall back to the hardcoded
    /// defaults. Combo syntax: `"Ctrl+Shift+H"`, `"Ctrl+Alt+T"`, etc.
    #[serde(default)]
    pub hotkeys: Hotkeys,
    #[serde(default)]
    pub startup: Startup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    #[serde(default = "default_staff")]
    pub staff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_billable")]
    pub billable: bool,
}

/// `[hotkeys]` — each is an optional combo string. `None` ⇒ use the
/// hardcoded default for that action. (The default `config.toml` we write
/// includes these as commented-out lines so the rebind path is discoverable.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hotkeys {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_entry: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popover_toggle: Option<String>,
}

/// `[startup]` — `enabled = false` tells the app not to arm autostart for
/// itself. (The MSIX manifest's `uap5:StartupTask` is a *request* Windows
/// gates behind a user consent in Settings → Startup Apps; the app can't
/// un-declare it without the WinRT `StartupTask` API, which is a v1.1 item.
/// So `enabled = false` is honoured to the extent the runtime allows: the
/// app logs it and skips any future "arm autostart" step.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Startup {
    #[serde(default = "default_startup_enabled")]
    pub enabled: bool,
}

fn default_staff() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string())
}

fn default_billable() -> bool {
    true
}

fn default_startup_enabled() -> bool {
    true
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            staff: default_staff(),
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            billable: default_billable(),
        }
    }
}

impl Default for Startup {
    fn default() -> Self {
        Self {
            enabled: default_startup_enabled(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            identity: Identity::default(),
            defaults: Defaults::default(),
            hotkeys: Hotkeys::default(),
            startup: Startup::default(),
        }
    }
}

/// A parsed hotkey combo: `(ctrl, alt, shift, win, key_code_name)`. The
/// `key` is the `global_hotkey::hotkey::Code` *variant name* (e.g. `"KeyH"`,
/// `"Semicolon"`, `"Quote"`, `"Slash"`) — the caller maps it to a `Code`.
/// Lives here (not in `windows_main.rs`) so it's unit-testable on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyCombo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// `global_hotkey::hotkey::Code` variant name.
    pub key: String,
}

/// Parse `"Ctrl+Shift+H"` / `"Ctrl+Alt+T"` / `"Win+;"` etc. into a
/// `HotkeyCombo`. Case-insensitive on modifier names; the final token is the
/// key. Single letters map to `KeyX`; digits to `DigitN`; a handful of
/// punctuation names are recognised. Returns `None` on anything unparseable.
pub fn parse_hotkey_combo(s: &str) -> Option<HotkeyCombo> {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut win = false;
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let (mods, key_tok) = parts.split_at(parts.len() - 1);
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "win" | "super" | "meta" | "cmd" => win = true,
            _ => return None,
        }
    }
    let key = key_tok[0];
    let code = key_token_to_code(key)?;
    if !(ctrl || alt || shift || win) {
        // refuse modifier-less hotkeys — they'd intercept a bare key globally
        return None;
    }
    Some(HotkeyCombo { ctrl, alt, shift, win, key: code })
}

fn key_token_to_code(tok: &str) -> Option<String> {
    let t = tok.trim();
    if t.len() == 1 {
        let c = t.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Some(format!("Key{}", c.to_ascii_uppercase()));
        }
        if c.is_ascii_digit() {
            return Some(format!("Digit{c}"));
        }
        return Some(match c {
            ';' => "Semicolon",
            '\'' => "Quote",
            '/' => "Slash",
            '\\' => "Backslash",
            ',' => "Comma",
            '.' => "Period",
            '-' => "Minus",
            '=' => "Equal",
            '[' => "BracketLeft",
            ']' => "BracketRight",
            '`' => "Backquote",
            _ => return None,
        }
        .to_string());
    }
    // multi-char names: a few common ones, plus pass-through of FN keys / etc.
    let upper = t.to_ascii_uppercase();
    Some(match upper.as_str() {
        "SEMICOLON" => "Semicolon",
        "QUOTE" | "APOSTROPHE" => "Quote",
        "SLASH" => "Slash",
        "BACKSLASH" => "Backslash",
        "COMMA" => "Comma",
        "PERIOD" | "DOT" => "Period",
        "SPACE" => "Space",
        "ENTER" | "RETURN" => "Enter",
        "TAB" => "Tab",
        "BACKQUOTE" | "GRAVE" => "Backquote",
        "MINUS" => "Minus",
        "EQUAL" | "EQUALS" => "Equal",
        // F1..F24
        s if s.starts_with('F') && s[1..].chars().all(|c| c.is_ascii_digit()) && !s[1..].is_empty() => {
            return Some(s.to_string()); // global_hotkey Code variants are F1..F24
        }
        _ => return None,
    }
    .to_string())
}

/// The default `config.toml` body written on first launch. Hand-written
/// (not `toml::to_string_pretty`) so the `[hotkeys]` rebind path and the
/// `[startup]` toggle are *discoverable* as commented-out lines — the
/// hotkey-conflict MessageBox points the user here, so the section it
/// points at has to actually exist.
fn default_config_body(staff: &str, billable: bool) -> String {
    format!(
        "# Time Tracker — settings. Quit and relaunch after editing.\n\
         \n\
         [identity]\n\
         staff = \"{staff}\"\n\
         \n\
         [defaults]\n\
         billable = {billable}\n\
         \n\
         # [hotkeys] — uncomment + edit to rebind. Combo syntax: \"Ctrl+Shift+H\".\n\
         # Use this if another app already claimed a combo (the app shows a\n\
         # warning box on launch listing any that failed to register).\n\
         # [hotkeys]\n\
         # quick_entry    = \"Ctrl+Shift+H\"   # opens the popover's add-workstream form\n\
         # timer_start    = \"Ctrl+Shift+;\"   # stops the running timer and writes the entry (no popup) — starting happens via the popover\n\
         # timer_stop     = \"Ctrl+Shift+'\"   # opens the popover's workstream filter (type to filter, ↓ to pick) — was: stop timer\n\
         # popover_toggle = \"Ctrl+Shift+/\"   # show / hide the popover\n\
         \n\
         # [startup] — set enabled = false to stop the app arming autostart for\n\
         # itself. (Windows also has its own toggle: Settings → Apps → Startup.)\n\
         # [startup]\n\
         # enabled = true\n",
        staff = staff,
        billable = billable,
    )
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        if !path.exists() {
            // First launch: write the default file so the user can
            // discover + edit it.
            let cfg = Self::default();
            let body = default_config_body(&cfg.identity.staff, cfg.defaults.billable);
            let write = (|| -> std::io::Result<()> {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let tmp = path.with_extension("toml.tmp");
                std::fs::write(&tmp, body)?;
                std::fs::rename(&tmp, &path)?;
                Ok(())
            })();
            match write {
                Ok(()) => tracing::info!(path = %path.display(), "wrote default config.toml"),
                Err(e) => tracing::warn!(error = %e, "could not write default config.toml"),
            }
            return cfg;
        }
        match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "config.toml parse failed; using defaults"
                    );
                    Self::default()
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "config.toml read failed; using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml_str)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn config_path() -> PathBuf {
    crate::paths::data_dir().join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_combo_basic() {
        assert_eq!(
            parse_hotkey_combo("Ctrl+Shift+H"),
            Some(HotkeyCombo { ctrl: true, alt: false, shift: true, win: false, key: "KeyH".into() })
        );
        assert_eq!(
            parse_hotkey_combo("ctrl+alt+t"),
            Some(HotkeyCombo { ctrl: true, alt: true, shift: false, win: false, key: "KeyT".into() })
        );
        assert_eq!(
            parse_hotkey_combo("Ctrl+Shift+;").map(|c| c.key),
            Some("Semicolon".into())
        );
        assert_eq!(parse_hotkey_combo("Ctrl+Shift+'").map(|c| c.key), Some("Quote".into()));
        assert_eq!(parse_hotkey_combo("Ctrl+Shift+/").map(|c| c.key), Some("Slash".into()));
        assert_eq!(parse_hotkey_combo("Win+F2").map(|c| (c.win, c.key)), Some((true, "F2".into())));
        assert_eq!(parse_hotkey_combo("Ctrl+1").map(|c| c.key), Some("Digit1".into()));
    }

    #[test]
    fn parse_combo_rejects_bad() {
        assert_eq!(parse_hotkey_combo(""), None);
        assert_eq!(parse_hotkey_combo("H"), None, "modifier-less is refused");
        assert_eq!(parse_hotkey_combo("Hyper+H"), None, "unknown modifier");
        assert_eq!(parse_hotkey_combo("Ctrl+"), None, "no key");
        assert_eq!(parse_hotkey_combo("Ctrl+€"), None, "non-ascii key");
    }

    #[test]
    fn config_parses_hotkeys_and_startup_sections() {
        let s = r#"
            [identity]
            staff = "ryan"
            [defaults]
            billable = false
            [hotkeys]
            quick_entry = "Ctrl+Alt+H"
            timer_stop = "Ctrl+Shift+'"
            [startup]
            enabled = false
        "#;
        let c: Config = toml::from_str(s).unwrap();
        assert_eq!(c.identity.staff, "ryan");
        assert!(!c.defaults.billable);
        assert_eq!(c.hotkeys.quick_entry.as_deref(), Some("Ctrl+Alt+H"));
        assert_eq!(c.hotkeys.timer_start, None);
        assert_eq!(c.hotkeys.timer_stop.as_deref(), Some("Ctrl+Shift+'"));
        assert!(!c.startup.enabled);
    }

    #[test]
    fn config_defaults_when_sections_absent() {
        let c: Config = toml::from_str("[identity]\nstaff = \"x\"\n").unwrap();
        assert!(c.startup.enabled, "[startup] absent -> enabled true");
        assert_eq!(c.hotkeys.quick_entry, None);
    }
}
