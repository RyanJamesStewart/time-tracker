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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningTimer {
    pub client: String,
    pub engagement: String,
    pub narrative: String,
    pub billable: bool,
    pub started_at: DateTime<Local>,
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
        });
        let _ = self.persist();
        Ok(())
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

    #[test]
    fn elapsed_under_cap_returns_unflagged() {
        let rt = RunningTimer {
            client: "A".into(),
            engagement: "".into(),
            narrative: "x".into(),
            billable: true,
            started_at: Local::now() - CDuration::minutes(30),
        };
        let (mins, exceeded) = rt.elapsed_minutes();
        assert!(mins >= 29 && mins <= 31, "expected ~30, got {mins}");
        assert!(!exceeded);
    }

    #[test]
    fn elapsed_over_12h_caps_and_flags() {
        let rt = RunningTimer {
            client: "A".into(),
            engagement: "".into(),
            narrative: "overnight".into(),
            billable: true,
            started_at: Local::now() - CDuration::hours(15),
        };
        let (mins, exceeded) = rt.elapsed_minutes();
        assert_eq!(mins, SANITY_CAP_MINUTES);
        assert!(exceeded);
    }
}
