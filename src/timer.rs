// SPEC §3.2 timer subsystem.
//
// State machine:
//   None  --start(client, engagement, narrative, billable)-->  Some(RunningTimer)
//   Some  --stop()--> Some(taken)  (returns the timer to the caller for entry write)
//
// Persistence (per SPEC §3.2):
//   - Atomic write of the current RunningTimer (or removal of file if None)
//     after every state mutation (start, stop) so crashes never lose
//     elapsed time.
//   - The 30s tick persistence requirement is satisfied by re-persisting
//     in `tick()` which the main loop calls periodically.
//
// Sanity cap (per SPEC §3.2):
//   elapsed > 12h returns the capped value AND a flag the popup uses to
//   render a "Timer running >12h - confirm or edit" hint. Guards against
//   laptop sleep, DST shifts, NTP clock jumps.

use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "timer-state.json";
const SANITY_CAP_MINUTES: u32 = 12 * 60;

/// Continuation-reminder thresholds (minutes since the current window's
/// base). At REMIND we surface a non-invasive toast asking the user to
/// confirm the timer is still legit; if they don't answer, at AUTOSTOP we
/// stop the timer and log the entry so a forgotten/overnight timer can't
/// run away. "Keep going" re-bases the window so the next prompt is
/// REMIND_AFTER_MIN later again.
pub const REMIND_AFTER_MIN: i64 = 4 * 60;
pub const AUTOSTOP_AFTER_MIN: i64 = 5 * 60;

/// What the 30s tick should do about the running timer's continuation
/// window. `Ok` covers both "too early to ask" and "already asked, still
/// inside the 1h grace".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationState {
    Ok,
    /// >= 4h since the window base and we haven't toasted yet.
    RemindDue,
    /// >= 5h since the window base with no "keep going" — stop + log.
    AutoStopDue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningTimer {
    pub client: String,
    pub engagement: String,
    pub narrative: String,
    pub billable: bool,
    pub started_at: DateTime<Local>,
    /// When the continuation window was last re-armed by the user pressing
    /// "Keep going". `None` => measure from `started_at`. `#[serde(default)]`
    /// so timer-state.json files written by older builds still load.
    #[serde(default)]
    pub reminder_base: Option<DateTime<Local>>,
    /// The toast for the *current* window has already been shown — don't
    /// re-toast on every 30s tick during the 1h grace.
    #[serde(default)]
    pub reminded: bool,
}

impl RunningTimer {
    /// (minutes, exceeded_cap). The minutes value is the capped value;
    /// the bool flags that the actual elapsed time was greater than the
    /// 12h cap so the UI can prompt the user.
    pub fn elapsed_minutes(&self) -> (u32, bool) {
        let now = Local::now();
        let raw = (now - self.started_at).num_minutes().max(0) as u32;
        let exceeds_cap = raw > SANITY_CAP_MINUTES;
        (raw.min(SANITY_CAP_MINUTES), exceeds_cap)
    }

    /// Start of the active continuation window: the last "Keep going", or
    /// the original start if it's never been confirmed.
    fn window_base(&self) -> DateTime<Local> {
        self.reminder_base.unwrap_or(self.started_at)
    }

    /// Where this timer sits in its continuation window right now.
    pub fn continuation_state(&self) -> ContinuationState {
        let mins = (Local::now() - self.window_base()).num_minutes().max(0);
        if mins >= AUTOSTOP_AFTER_MIN {
            ContinuationState::AutoStopDue
        } else if mins >= REMIND_AFTER_MIN && !self.reminded {
            ContinuationState::RemindDue
        } else {
            ContinuationState::Ok
        }
    }
}

#[derive(Debug, Default)]
pub struct Timer {
    current: Option<RunningTimer>,
}

impl Timer {
    /// Try to restore from disk. Returns an empty Timer on missing or
    /// unparseable file (rather than panic - we always want the app to
    /// boot even if the state file is corrupt).
    pub fn load() -> Self {
        let path = state_path();
        let current = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        if current.is_some() {
            tracing::info!(path = %path.display(), "restored running timer from disk");
        }
        Self { current }
    }

    pub fn is_running(&self) -> bool {
        self.current.is_some()
    }

    pub fn peek(&self) -> Option<&RunningTimer> {
        self.current.as_ref()
    }

    /// Start a new timer. Returns Err if one is already running (caller
    /// should surface this to the user, not panic).
    pub fn start(
        &mut self,
        client: String,
        engagement: String,
        narrative: String,
        billable: bool,
    ) -> Result<(), &'static str> {
        if self.current.is_some() {
            return Err("timer already running");
        }
        self.current = Some(RunningTimer {
            client,
            engagement,
            narrative,
            billable,
            started_at: Local::now(),
            reminder_base: None,
            reminded: false,
        });
        let _ = self.persist();
        Ok(())
    }

    /// Continuation state of the running timer, or `None` if idle.
    pub fn continuation_state(&self) -> Option<ContinuationState> {
        self.current.as_ref().map(RunningTimer::continuation_state)
    }

    /// Record that the continuation toast has been shown for the current
    /// window so the next ticks don't re-toast. Persisted so a restart
    /// during the 1h grace doesn't re-prompt.
    pub fn mark_reminded(&mut self) {
        if let Some(rt) = self.current.as_mut() {
            rt.reminded = true;
            let _ = self.persist();
        }
    }

    /// User pressed "Keep going": re-base the continuation window to now
    /// and clear the reminded flag so the next prompt is REMIND_AFTER_MIN
    /// out again.
    pub fn ack_continue(&mut self) {
        if let Some(rt) = self.current.as_mut() {
            rt.reminder_base = Some(Local::now());
            rt.reminded = false;
            let _ = self.persist();
        }
    }

    /// Stop and return the timer (so the caller can write an Entry).
    /// Returns None if no timer was running.
    pub fn stop(&mut self) -> Option<RunningTimer> {
        let taken = self.current.take();
        if taken.is_some() {
            let _ = self.clear_disk();
        }
        taken
    }

    /// Re-persist the current state. Called periodically by the main
    /// loop to satisfy SPEC §3.2 "save timer state on every state
    /// mutation (start, every 30s tick, manual stop)".
    pub fn tick_persist(&self) {
        let _ = self.persist();
    }

    fn persist(&self) -> std::io::Result<()> {
        let path = state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match &self.current {
            Some(rt) => {
                let json = serde_json::to_string_pretty(rt)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, json)?;
                std::fs::rename(&tmp, &path)?;
            }
            None => {
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        Ok(())
    }

    fn clear_disk(&self) -> std::io::Result<()> {
        let path = state_path();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

fn state_path() -> PathBuf {
    crate::paths::data_dir().join(STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as CDuration;

    #[test]
    fn empty_timer_not_running() {
        let t = Timer::default();
        assert!(!t.is_running());
        assert!(t.peek().is_none());
    }

    #[test]
    fn start_then_peek_returns_fields() {
        let mut t = Timer::default();
        // We bypass disk here by not asserting persist succeeded; the
        // `start` call ignores persist errors so this is fine.
        t.start("ClientCo".into(), "K-1".into(), "phone".into(), true)
            .unwrap();
        let r = t.peek().expect("running");
        assert_eq!(r.client, "ClientCo");
        assert_eq!(r.engagement, "K-1");
        assert_eq!(r.narrative, "phone");
        assert!(r.billable);
    }

    #[test]
    fn cannot_start_when_running() {
        let mut t = Timer::default();
        let _ = t.start("A".into(), "".into(), "x".into(), true);
        let err = t.start("B".into(), "".into(), "y".into(), false);
        assert!(err.is_err());
    }

    #[test]
    fn stop_returns_taken_timer() {
        let mut t = Timer::default();
        let _ = t.start("A".into(), "".into(), "x".into(), true);
        let taken = t.stop().expect("returned");
        assert_eq!(taken.client, "A");
        assert!(!t.is_running());
    }

    #[test]
    fn stop_when_not_running_returns_none() {
        let mut t = Timer::default();
        assert!(t.stop().is_none());
    }

    /// A RunningTimer started `ago` in the past, never confirmed.
    fn rt_started(ago: CDuration) -> RunningTimer {
        RunningTimer {
            client: "A".into(),
            engagement: "".into(),
            narrative: "x".into(),
            billable: true,
            started_at: Local::now() - ago,
            reminder_base: None,
            reminded: false,
        }
    }

    #[test]
    fn elapsed_under_cap_returns_unflagged() {
        let rt = rt_started(CDuration::minutes(30));
        let (mins, exceeded) = rt.elapsed_minutes();
        assert!(mins >= 29 && mins <= 31, "expected ~30, got {mins}");
        assert!(!exceeded);
    }

    #[test]
    fn elapsed_over_12h_caps_and_flags() {
        let rt = rt_started(CDuration::hours(15));
        let (mins, exceeded) = rt.elapsed_minutes();
        assert_eq!(mins, SANITY_CAP_MINUTES);
        assert!(exceeded);
    }

    #[test]
    fn continuation_ok_before_4h() {
        assert_eq!(
            rt_started(CDuration::hours(3)).continuation_state(),
            ContinuationState::Ok
        );
    }

    #[test]
    fn continuation_remind_due_at_4h_then_silenced() {
        let mut rt = rt_started(CDuration::minutes(REMIND_AFTER_MIN + 5));
        assert_eq!(rt.continuation_state(), ContinuationState::RemindDue);
        // Once toasted, we stay Ok through the 1h grace (not re-prompting).
        rt.reminded = true;
        assert_eq!(rt.continuation_state(), ContinuationState::Ok);
    }

    #[test]
    fn continuation_autostop_at_5h_even_if_reminded() {
        let mut rt = rt_started(CDuration::minutes(AUTOSTOP_AFTER_MIN + 1));
        rt.reminded = true;
        assert_eq!(rt.continuation_state(), ContinuationState::AutoStopDue);
    }

    #[test]
    fn ack_continue_rebases_window() {
        let mut t = Timer::default();
        t.start("C".into(), "E".into(), "".into(), true).unwrap();
        // Force the running timer to look 4.5h old.
        t.current.as_mut().unwrap().started_at =
            Local::now() - CDuration::minutes(REMIND_AFTER_MIN + 30);
        assert_eq!(
            t.continuation_state(),
            Some(ContinuationState::RemindDue)
        );
        t.ack_continue();
        // Re-based to now → back to Ok, reminded cleared.
        assert_eq!(t.continuation_state(), Some(ContinuationState::Ok));
        assert!(!t.current.as_ref().unwrap().reminded);
    }

    #[test]
    fn idle_timer_has_no_continuation_state() {
        assert_eq!(Timer::default().continuation_state(), None);
    }
}
