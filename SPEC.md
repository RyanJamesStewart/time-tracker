# Time Tracker — v1 Spec

**Owner:** Ryan
**Customer:** Ghumans Accounting | Tax | Advisory (B-315)
**Target ship:** mid-week (Wed)
**Posture:** prove-it wedge. Ship clean, instrument heavily, generate Part 2 pitch from real usage.

---

## 1. Goals

### 1.1 Primary
Replace whatever ad-hoc time recording staff do today (paper, memory, spreadsheet) with a frictionless hotkey-driven CSV log. Latency from intent → logged entry should be **under 5 seconds** for a typical entry.

### 1.2 Secondary
Generate usage data that justifies Part 2 (email-to-financials). Every entry, hotkey fire, and time-to-save is instrumented locally.

### 1.3 Non-goals (v1)
- Accounting-system integration (Part 1.5)
- Cloud sync, multi-machine
- Multi-user / staff roster (each install is single-user)
- Reporting dashboards beyond CSV
- Reminders, AI parsing, email integration
- macOS / Linux

---

## 2. User stories

1. **Retrospective entry.** Partner just got off a 20-minute call with ClientCo. Hits hotkey, types `ClientCo` (autocomplete), `phone re K-1 questions`, `0.3`, Enter. Done in 4 seconds.
2. **Live timer.** Staff starts a research task. Hits `Ctrl+Shift+;`, fills client + narrative, starts. 47 minutes later hits `Ctrl+Shift+'`, sees elapsed, rounds to `0.75`, Enter.
3. **End of day review.** Tray menu → "Open this month's CSV" → opens in Excel for billing review.
4. **Reboot.** Machine restarts. App auto-starts on login, tray icon present, hotkeys live, no user action.
5. **Excel open on file.** User has CSV open in Excel. Hits hotkey, logs entry. App writes successfully without prompting.

---

## 3. Functional requirements

> **⚠️ §3.1 / §3.2 are superseded by the shipping `--features live-view` build (2026-05-11).**
> §3.1–3.2 below describe the original `--no-feature` tray-only build. What actually ships:
> - `Ctrl+Shift+H` → opens the tray **popover** with the add-workstream form expanded (NOT a quick-entry popup). Quick one-off blocks are the tray menu's **"Log a block…"** item, which opens the §3.1 popup.
> - `Ctrl+Shift+;` → start-timer popup (matches §3.2).
> - `Ctrl+Shift+'` → **stops the running timer immediately and writes the entry — no popup, no rounding/cancel, no 12h-cap prompt.** (The §3.2 confirmable stop + 12h prompt still exists in the egui popup, but the live-view build's hotkey path bypasses it. A brief "stopped — undo" affordance + the editable Recorded Time page (`/recorded`) cover fat-finger recovery.)
> - `Ctrl+Shift+/` → toggles the popover (a fourth hotkey not in §3.1–3.2).
> - Hotkeys are rebindable via a `[hotkeys]` section in `config.toml` (commented-out template written on first launch). The Settings *GUI* (§3.5) remains unbuilt — v1.1.
> See `PARTNER.md` / `msix/README.md` for the install-ceremony version that matches reality.

### 3.1 Quick-entry hotkey

- **Default:** `Ctrl+Shift+H` (mnemonic: **H**ours; right-hand reachable with left-hand Ctrl+Shift; minimal conflict in Outlook/Excel/QB/Edge)
- **Behavior:** opens popup near cursor on the active monitor
- **Fields, in tab order:**
  1. Client (text, autocomplete from prior entries, required)
  2. Engagement (text, autocomplete, optional)
  3. Narrative (text, required, no length limit)
  4. Duration (text, parsed — see §3.3)
  5. Billable (checkbox, default per config, default true)
- **Submit:** Enter from any field → validate → **synchronous CSV write (~5-30ms on SSD)** → post-write self-verify (re-read last line, confirm match) → close popup → toast "Logged 0.3h to ClientCo" for 1.5s. If write fails or self-verify mismatches, popup stays open with inline error — no data loss.
- **Cancel:** Esc → close, no write
- **No confirmation dialog.** Friction kills adoption.

### 3.2 Timer hotkeys (split: separate start + stop)

- **Start default:** `Ctrl+Shift+;` (semicolon — right-pinky/ring-finger reachable, adjacent to `'`)
- **Stop default:** `Ctrl+Shift+'` (apostrophe — adjacent to `;` so the start/stop pair lives in one motor pattern)
- **Conflict note:** Excel binds `Ctrl+Shift+;` (insert current time) and `Ctrl+Shift+'` (copy value from cell above). Once this app's hotkeys are registered, those Excel shortcuts are intercepted globally. Surface this in install ceremony §6.4 + Settings UI §3.5 so the partner knows. Both are rebindable.
- **Behavior of start (no timer running):** opens popup with same fields as quick-entry minus duration. Submit → starts timer, stores client/engagement/narrative in memory, closes popup. Tray icon changes to running state, tooltip shows `● ClientCo · 0:12:34`. **Pressing start while a timer is already running is a no-op + toast** "Timer already running for ClientCo. Press Ctrl+Shift+' to stop."
- **Behavior of stop (timer running):** opens popup pre-filled with started fields and elapsed time as duration (rounded to nearest 0.1h, editable). Submit → write entry, clear timer state. Cancel → keep timer running. **Pressing stop with no timer running is a no-op + toast** "No timer running. Press Ctrl+Shift+; to start."
- **Persistence:** save timer state on every state mutation (start, every 30s tick, manual stop) — not just on exit — so a crash never loses elapsed time. State file: `%LOCALAPPDATA%\TimeTracker\timer-state.json`, atomic write (temp + rename).
- **Elapsed clock:** stored as wall-clock `started_at` (RFC 3339 with offset). On display/computation, compare `Local::now()` against `started_at`. **Sanity cap:** if computed elapsed exceeds 12h, popup pre-fills the cap and prompts "Timer running >12h — confirm or edit." Guards against laptop sleep, DST shifts, and NTP clock jumps.
- **Clock-skew note (intentional):** elapsed is wall-clock, not monotonic. An NTP correction or manual clock adjustment between start and stop distorts elapsed minutes silently for skews under the 12h cap. Documented behavior, not a bug — see `msix/README.md` "Time / clock notes". Dual monotonic+wall tracking is a v1.1 item if a real partner case surfaces.

### 3.3 Duration parser

Accept all of:
- `0.3` → 18 minutes (decimal hours)
- `0.25`, `1.5`, `1.75` etc.
- `18m`, `90m` → minutes
- `1h`, `2h` → hours
- `1h30m`, `1h 30m` → 90 minutes
- `1:15` → 75 minutes (h:mm)
- `1:15:30` → reject, too granular

**Explicit rejects** (in addition to malformed input): `0`, `0.0`, `00:00`, negatives (`-0.5`), and anything > 24h (sanity cap; a single entry larger than a day is almost always a typo). Reject and surface error inline (red border + hint text). Don't try to be clever.

**Locale assumption (en-US):** the parser uses `.` as the decimal separator. A user on a comma-decimal locale (de-DE, fr-FR, etc.) typing `1,5` gets a parse error. CSV `hours_decimal` is also written with `.` regardless of system locale, so Excel / accounting-system imports stay stable across staff machines with different regional settings. Comma-decimal input handling is a v1.1 follow-up.

### 3.4 Tray menu

- App name + version (greyed header)
- **Today:** `5 entries · 4.2h` (live)
- ───
- ▶ Quick entry (`Ctrl+Shift+H`)
- ▶ Start timer (`Ctrl+Shift+;`)
- ▶ Stop timer (`Ctrl+Shift+'`)
- ───
- Open this month's CSV
- Open data folder
- ───
- Pause hotkeys (toggle)
- Settings…
- ───
- About
- Quit

### 3.5 Settings UI

Single window, opened from tray. Sections:
- **Identity:** staff name (single text field, used in CSV `staff` column)
- **Hotkeys:** rebind quick-entry and timer (capture key combo, validate against registered hotkeys, show "this combo is taken by another app" if `RegisterHotKey` fails). **On registration failure at app start (not just rebind), also flip the tray icon to a warning state and emit a Windows toast notification** — never let a hotkey silently fail to register; user will think the app is broken.
- **Defaults:** billable on/off default, default rounding (0.1h / 0.25h)
- **Files:** show data folder path, "Open" button
- **Startup:** auto-start on login toggle
- **About:** version, build date, "Export diagnostics" button (zips logs + config)

### 3.6 Autocomplete

- Source: distinct values of `client` and `engagement` columns from the last 90 days of CSV entries
- Cached in `%LOCALAPPDATA%\TimeTracker\autocomplete.json`, rebuilt on app start and after each entry
- Match: case-insensitive prefix + substring, prefix matches ranked first
- Show top 5 suggestions inline; arrow keys to select, Tab/Enter to accept

### 3.7 CSV format

**Path:** `%USERPROFILE%\TimeTracker\YYYY-MM.csv` (one file per month, rolls automatically)

**Why not `Documents`:** when OneDrive Documents-folder backup is enabled (default for many Microsoft-account configurations), `%USERPROFILE%\Documents\` silently redirects to `%USERPROFILE%\OneDrive\Documents\`, syncing every CSV to Microsoft cloud. For accountant client data we want users to explicitly opt in to cloud sync, not inherit it via Known Folder redirection. Putting the CSV folder directly under `%USERPROFILE%` (a non-Known-Folder location) gives a stable, predictable, redirect-free path. The partner can always move or symlink to a OneDrive/Dropbox/etc. location if they want sync; the default avoids surprise.

**Columns (header row written on file create):**

```
timestamp_iso, staff, client, engagement, narrative, minutes, hours_decimal, billable, entry_method
```

- `timestamp_iso`: RFC 3339, local time with offset (e.g. `2026-05-08T15:23:11-07:00`)
- `staff`: from settings
- `client`, `engagement`, `narrative`: as entered, CSV-escaped
- `minutes`: integer
- `hours_decimal`: `minutes / 60`, rounded to 2 decimal places (CPA convention)
- `billable`: `true` / `false`
- `entry_method`: `quick` / `timer`

**Encoding:** UTF-8 with BOM (Excel-friendly on Windows)
**Line endings:** CRLF
**Append-only.** Edits happen in Excel, not the app.

---

## 4. Non-functional requirements

### 4.1 Reliability

- **Atomic writes (all state files, not just CSV):** every write to `config.toml`, `timer-state.json`, `autocomplete.json`, queue files, and CSV uses the temp + fsync + rename pattern. Survives crash mid-write.
- **Post-write self-verify:** after every CSV append AND every config save, re-read the last line / full file and confirm content matches what was just written. Mismatch → log + retry once + surface error if still failing. Catches AV/EDR rollbacks and MSIX virtualization redirects (lesson from prompt-time).
- **Pre-create placeholder files on first launch:** touch `usage.log`, `autocomplete.json` (with `{}`), and a `.keep` file in `queue\`, `logs\`, `crashes\`. MSIX file virtualization redirects first writes to a per-package shadow path; pre-creating real files makes subsequent appends modify the real file, not the shadow.
- **Single-writer model for CSV:** all CSV writes (live submit + queue drain) go through one async task with an mpsc channel. Submit handler sends `WriteEntry`; the writer task serializes. No file locks needed in app code. Eliminates queue+drain race conditions.
- **File locking (Excel):** if rename fails because Excel has the file open with write lock, retry with exponential backoff up to 10s. If still failing, queue entry to `%LOCALAPPDATA%\TimeTracker\queue\` and surface tray badge "1 entry pending — close Excel to flush". Drain queue on next successful write (via the single-writer task above).
- **Crash handler:** `minidumper` on panic, write to `%LOCALAPPDATA%\TimeTracker\crashes\`. Surface on next launch with "Send to developer" link (mailto, no auto-upload). **Fallback (if Day 3 PM runs hot):** simple `std::panic::set_hook` → write panic message + backtrace to log → show "last run crashed, view log?" on next launch. No minidump IPC server needed.
- **Single instance:** named mutex `Global\TimeTracker-<user-sid>`. Second launch → connect to first instance via named pipe `\\.\pipe\TimeTracker-<user-sid>` and send `show_quick_entry`; first instance pops the popup. Second exits cleanly.
- **Diagnostics export must exclude PII:** `Settings → Export diagnostics` zips `logs\`, `config.toml`, `usage.log`, version info — but **NOT** `queue\` (contains raw entries with narrative + client) and **NOT** `autocomplete.json` (contains client list). Surface scope in UI: "Logs and config only. No client data."
- **Log retention:** `tracing-appender` rotates daily but does not enforce retention. Add a startup task that deletes `time-tracker.YYYY-MM-DD.log` files older than 30 days (~10 lines).

### 4.2 Performance

- Cold start: < 500ms to tray icon present
- Hotkey → popup visible: < 100ms via **pre-warmed popup window**. The popup window is created hidden at app start (off-screen + `visible=false`), so hotkey fire reduces to: reposition to cursor monitor → show + focus. Cold-create egui windows take 200-400ms on first show due to shader/font-atlas init — pre-warming hides that cost in the always-running tray process. Adds ~10-20MB resident (within memory budget).
- Submit → popup closed: < 50ms total, including the **synchronous CSV write inline** (sync write to NVMe is typically 5-15ms; the budget covers SSD and most HDDs). On submit failure, popup stays open with inline error.
- Memory: < 50MB resident (egui+eframe baseline ~15-30MB + pre-warmed popup +10-20MB + autocomplete cache).

### 4.3 UI quality

- DPI: per-monitor v2 awareness declared in manifest. No blurry popups on 4K.
- Multi-monitor: popup positioned via `MonitorFromPoint(GetCursorPos)`, not primary monitor.
- Theme: respect `AppsUseLightTheme` registry value. Light/dark popup matches OS.
- Font: Segoe UI, OS default sizing.

### 4.4 Security / privacy

- All data local. **No network calls in v1** (no auto-update — see §6.3).
- No telemetry leaves the machine without explicit "Export diagnostics" user action.
- Logs scrub narrative text (only field lengths logged, not content).
- Diagnostics export excludes `queue\` and `autocomplete.json` (see §4.1) so client + narrative data never ships in support zips.

---

## 5. Technical architecture

### 5.1 Stack

- **Language:** Rust, `x86_64-pc-windows-msvc` target
- **Event loop:** `tao` (or `winit` via eframe) — **single Win32 message loop owns the process.** `tray-icon`, `global-hotkey`, and the egui popup window all register their event channels into this one loop. Avoids dueling-message-pump bugs.
- **UI:** `egui` + `eframe` for popup and settings, configured to use the existing `tao`/`winit` event loop rather than spawning its own.
- **Tray:** `tray-icon` crate
- **Hotkeys:** `global-hotkey` crate
- **IPC (single-instance):** Win32 named pipe via `windows` crate
- **Windows APIs:** `windows` crate for mutex, named pipe, registry, monitor detection, `GetCurrentPackageFamilyName`
- **Paths:** `directories` crate (with MSIX-aware fallback — see §5.2)
- **Time:** `chrono` (parsing + formatting); wall-clock for all elapsed-time calculations with the §3.2 sanity cap
- **CSV:** `csv` crate (UTF-8 BOM written explicitly on file create — `csv` crate does not add BOM by default)
- **Logging:** `tracing` + `tracing-appender` (daily rotation; retention enforced by app — see §4.1)
- **Crash:** `minidumper` (with `panic_hook + log` fallback per §4.1 if Day 3 PM runs hot)
- **Serialization:** `serde` + `toml` (config) + `serde_json` (autocomplete cache, queue, timer-state)

### 5.2 File layout (on disk)

```
%USERPROFILE%\TimeTracker\
    2026-05.csv
    2026-04.csv

%LOCALAPPDATA%\TimeTracker\
    config.toml             (atomic write; pre-created on first launch)
    autocomplete.json       (atomic write; pre-created with `{}` on first launch)
    timer-state.json        (only present when timer running; atomic write)
    queue\                  (.keep file pre-created on first launch)
        20260508-152311.json (queued entries when CSV locked)
    logs\                   (.keep file pre-created; daily rotation, 30-day retention)
        time-tracker.2026-05-08.log
    crashes\                (.keep file pre-created)
    usage.log               (instrumentation, see §5.4; pre-created on first launch)
```

**Path resolution:** Use `GetCurrentPackageFamilyName` (Win32) to detect MSIX context. When packaged, write to the per-package `LocalCache\Roaming\TimeTracker\` is fine because §6.1 disables file virtualization. For non-MSIX dev builds (`cargo run`), fall back to `%LOCALAPPDATA%\TimeTracker\` directly. Use one path-resolution helper for the whole app — never hardcode `%LOCALAPPDATA%`.

### 5.3 Config schema (`config.toml`)

```toml
[identity]
staff = "Ryan"

[hotkeys]
quick_entry = "Ctrl+Shift+H"
timer_start = "Ctrl+Shift+Semicolon"
timer_stop = "Ctrl+Shift+Quote"

[defaults]
billable = true
rounding_hours = 0.1

[startup]
auto_start = true
```

### 5.4 Usage instrumentation

`usage.log` is JSONL, one event per line. Append-only. Used for the Part 2 pitch ("your team logged 47 entries last week, here's the time you'd save with email automation").

Events:
- `hotkey_fire` — `{ts, hotkey, success}`
- `popup_open` — `{ts, source}`
- `popup_close` — `{ts, action: "submit"|"cancel", duration_ms}`
- `entry_written` — `{ts, method, minutes, billable}`
- `entry_queued` — `{ts, reason: "csv_locked"}`
- `queue_drain` — `{ts, count}`
- `app_start`, `app_exit`, `crash`

No client names, narratives, or other content — just structural. Local only.

---

## 6. Deployment

### 6.1 Packaging

- **MSIX**, code-signed with the cert used for prompt-time
- Per-user install, no admin required
- Manifest declares `runFullTrust` capability (needed for global hotkeys)
- Manifest declares `desktop6:FileSystemWriteVirtualization="disabled"` (`xmlns:desktop6="http://schemas.microsoft.com/appx/manifest/desktop/windows10/6"`) — opts the package out of MSIX file virtualization so writes to `%LOCALAPPDATA%` go to the real path, not a per-package shadow. Mitigates the prompt-time "shadow config trap" lesson.
- Autostart via `<uap5:StartupTask>` in manifest. **Note:** Windows enforces user opt-in on first launch — the manifest *requests* startup but the user must accept via Settings → Startup Apps (or the toast on first launch). First-launch UX includes a "Start with Windows? [Yes / No, I'll start manually]" prompt that opens the right Settings pane on Yes.
- Bundle identity: `RyanStewart.TimeTracker`

### 6.2 Hosting & distribution

**v1: hand-delivered MSIX. No public hosting, no `.appinstaller`, no auto-update infra.**

- Source repo: **private GitHub repo** (single dev, no contributors yet)
- Build artifacts (signed `.msix`) live in `release/` locally, version-tagged. Optionally archive to a private blob (S3/R2/OneDrive) for backup, never publicly downloadable.
- Distribution to customer: drive over (or remote screen-share) with the signed `.msix` on a thumb drive / file transfer / direct download from a one-off signed URL. Install in person (per §6.4).

**Why no public download:** this is a paid product. Anonymous downloads = freeloaders. Single customer in v1 — bilateral invoice/contract is the gate. Auto-update + payment-gated distribution moves to v1.1 (likely Stripe/Gumroad signed download URLs; see §9).

### 6.3 Update cadence

**v1: no auto-update mechanism.**

- Updates ship via in-person re-install or remote screen-share. Version bump → rebuild signed MSIX → install via `Add-AppxPackage` (which keeps user data in `%LOCALAPPDATA%\TimeTracker\`).
- During the first 2 weeks of partner usage, expect 1-2 patch deploys for found bugs. Plan for it in your calendar.
- Auto-update infra (`.appinstaller`, `HoursBetweenUpdateChecks`) deferred to v1.1 alongside the payment-gated distribution channel.

### 6.4 Install ceremony

- First install: do it in person on the partner's machine. Walk through:
  1. SmartScreen prompt (cert should suppress, but plan for it)
  2. First-launch self-test banner (per §6.1 / prompt-time "loud failures" lesson): hotkey registration ✓/✗, CSV path writable ✓/✗, autostart task installed ✓/✗. Any ✗ → actionable hint, not just a code.
  3. Accept "Start with Windows?" prompt → opens Settings → Startup Apps → enable
  4. First launch → settings → set staff name
  5. Demo all three hotkeys (Ctrl+Shift+H quick entry, Ctrl+Shift+; start timer, Ctrl+Shift+' stop timer)
  6. **Excel hotkey takeover disclosure:** "Once this is running, Ctrl+Shift+; and Ctrl+Shift+' will trigger the timer instead of Excel's insert-time / copy-value-above. If you use those Excel shortcuts often, we can rebind right now." Demo rebind in Settings if requested.
  7. Show CSV file location
  8. Demo: open the CSV in Excel, log a new entry, verify it appears (proves Excel-lock retry path)
- Leave a one-pager (printed) with hotkey reference, file location, your phone number, and how to find logs (`%LOCALAPPDATA%\TimeTracker\logs\`).

---

## 7. Build order (ship sequence for mid-week)

**Day 1 (Mon evening):**
0. **Smoke test (do this FIRST, before any UI work):** ~50-line throwaway that registers `Ctrl+Alt+T` via `global-hotkey`, beeps via `Beep()`, runs from inside an MSIX-packaged shell with `runFullTrust`. Verifies the foundational assumption that MSIX `runFullTrust` + `RegisterHotKey` works on the target environment. If it doesn't, you find out Monday — not Tuesday afternoon.
1. Cargo new, manifest, MSIX scaffold (copy from prompt-time, swap identity, add `desktop6:FileSystemWriteVirtualization="disabled"` per §6.1)
2. Single `tao` event loop scaffold (per §5.1) — tray-icon registers into it, hotkey registers into it
3. Tray icon + menu + quit working
4. Single-instance mutex + named-pipe IPC (`show_quick_entry` message handler)
5. Logging scaffold + path-resolution helper (MSIX-aware per §5.2) + pre-create placeholder files

**Day 2 (Tue):**
6. Hotkey registration with conflict surfacing (tray icon warning state on failure per §3.5)
7. Pre-warmed popup window (created hidden at startup per §4.2)
8. Quick-entry popup with all 5 fields, validation, duration parser
9. CSV append with atomic write + post-write self-verify + Excel-lock retry + queue fallback (single-writer task per §4.1)
10. Autocomplete from CSV (90-day window, prefix-then-substring ranking)
11. Toast on success

**Day 3 (Wed AM):**
12. Timer mode + persistence (every-30s ticks per §3.2, sanity cap)
13. Settings UI (all 6 sections per §3.5)
14. Autostart manifest entry + first-launch "Start with Windows?" prompt (per §6.1)
15. Usage instrumentation hooks
16. Crash handler (`minidumper`, with panic-hook fallback ready per §4.1)
17. **Test pass:** write the unit tests listed in §12 (~3-4h). Skipping this is how you ship a duration parser that silently rejects valid input.

**Day 3 (Wed PM):**
18. MSIX build, sign locally
19. Test install on a clean Windows VM (verify SmartScreen behavior, autostart consent flow, first-launch self-test)
20. Test with QB Desktop running and Excel open on the CSV (the partner's actual workflow)
21. Drive over MSIX file (or remote-share); install with Ghumans per §6.4

**Buffer:** Thu morning if anything slips. Don't slip into Friday.

---

## 8. Acceptance criteria (Wed demo)

Live demo, on Ghumans' actual machine if possible:

- [ ] All three hotkeys (Ctrl+Shift+H, Ctrl+Shift+;, Ctrl+Shift+') fire and open popup within 100ms
- [ ] Hotkey conflicts surface clearly in settings
- [ ] Quick entry: client autocomplete works, narrative + duration accepted, CSV written
- [ ] CSV opens cleanly in Excel with proper columns
- [ ] Timer: start, wait 30s, stop, entry written with correct elapsed time
- [ ] Timer survives app restart with running state intact
- [ ] CSV write succeeds while Excel has the file open (retry visible in tray badge if needed)
- [ ] Autostart on login works after reboot
- [ ] Single instance enforced (second launch focuses existing)
- [ ] Popup renders sharp on 4K monitor
- [ ] Popup appears on the monitor with the cursor
- [ ] Light/dark theme matches OS
- [ ] MSIX installs without admin prompt
- [ ] First-launch self-test banner shows all checks PASS (per §6.4)
- [ ] CSV write self-verify (post-write read-back) succeeds for every entry written during demo
- [ ] All entries appear in `usage.log`
- [ ] All §12 unit tests pass (`cargo test`)

---

## 9. Out of scope — explicit v1.1+ list

Track these as they come up so they don't pollute v1:

- **Payment-gated distribution + auto-update infra** (Stripe/Gumroad signed download URLs, `.appinstaller` channel, in-app license activation). v1 ships hand-delivered MSIX with bilateral invoice. Build this once customer #2 is in sight.
- Multi-staff (each user gets own install for now; merge later)
- Project/engagement codes from a defined list (vs free text)
- Rate column → invoice-ready CSV
- Accounting-system sync (flat-file export first, native API later)
- Per-client reports / dashboard
- Reminders ("you haven't logged time today")
- Edit/delete entries from app (do it in Excel for now)
- Pause/resume timer (just stop and start a new one)
- Idle detection ("you've been idle 15 min, pause timer?")
- Calendar import (auto-create entries from Outlook events)
- Mobile companion
- Diagnostics auto-upload (mailto only in v1, no auto-send)

---

## 10. Risks & mitigations

| Risk | Mitigation |
|---|---|
| SmartScreen warning on first install | Code-signing cert from prompt-time; in-person install lets you click through if it appears |
| Hotkey conflict with QB Desktop or Excel | Test on partner's actual machine before install; configurable hotkeys; tray icon warning state on registration failure (per §3.5). **Default `Ctrl+Shift+;` and `Ctrl+Shift+'` are bound by Excel** (insert-time, copy-value-above) — partner loses those Excel shortcuts while app runs. Surface during install ceremony (§6.4); rebindable. |
| CSV corruption from concurrent Excel + app writes | Atomic temp+rename, post-write self-verify, retry-on-lock, queue fallback, single-writer task (per §4.1) |
| MSIX file virtualization eats writes silently | `desktop6:FileSystemWriteVirtualization="disabled"` in manifest; pre-create placeholder files; post-write self-verify (per §6.1, §4.1) |
| AV/EDR rolls back state writes silently | Post-write self-verify on every CSV + config write (per §4.1; lesson from prompt-time) |
| MSIX `runFullTrust` + global hotkeys doesn't work in target environment | Day 1 smoke test (item 0 in §7) — find out Monday, not Tuesday afternoon |
| Three event loops collide (tray + hotkey + egui) | Single `tao` event loop owns everything (per §5.1) |
| Timer state lost on crash | Persist on every state mutation, not just exit (per §3.2) |
| Clock skew / DST / sleep corrupts timer elapsed | Sanity cap at 12h with confirm prompt (per §3.2) |
| Partner forgets hotkeys | One-page printed cheat sheet on install; tray menu shows hotkeys inline |
| Staff resist new tool | Ship to partner first, let him advocate. Don't push to staff in week 1. |
| Scope creep mid-week ("can it also do X") | Spec freeze. Anything new is v1.1. |
| Day 3 PM runs hot, can't ship | Crash handler falls back to simple panic-hook + log (per §4.1); Thu morning buffer; never slip into Friday |

---

## 11. Part 2 hooks (what this spec sets up)

- Usage log gives quantified pain ("47 entries × 6s save time vs ~30s in Excel = 19 min/day saved")
- Client/engagement autocomplete data becomes the seed of a client list for the financials product
- CSV format is stable enough that a sync-to-QB tool can target it later
- MSIX + signing pipeline reused as-is for the Part 2 product (auto-update infra built in v1.1 will also be reused)
- `usage.log` schema becomes the template for instrumenting subsequent products
- Path-resolution helper, atomic-write-with-self-verify utilities, single-writer-task pattern, and pre-warmed-window pattern are reusable Rust modules for any subsequent Windows desktop product

---

## 12. Tests

`cargo test` for unit + integration. UI is manual (per §8 acceptance criteria — egui has no good test infra).

**Day 3 AM test budget: ~3-4 hours. Skipping this is how a duration parser silently rejects valid input on Wed at the partner's desk.**

### 12.1 Unit tests (in-crate `#[cfg(test)] mod tests`)

Pure-logic modules (`autocomplete`, `csv_writer`, `duration`, `timer`,
`config`, `paths`, `crash`, `logging`, `usage`, `single_instance`) live in
`src/lib.rs`. The Windows-only GUI/IPC stack (winit, glutin, tray-icon,
global-hotkey, egui_glow) is gated behind
`[target.'cfg(windows)'.dependencies]` so:

```bash
cargo test --lib --target x86_64-unknown-linux-gnu
```

runs the full ~60-test suite from WSL without dragging in GTK/X11 or
the Win32 dependency tree. The bin (`src/main.rs`, `src/popup.rs`,
`src/bin/*`) stays Windows-only and is built via `cargo xwin`.

`src/duration.rs`:
- `parse("0.3")` → 18 min
- `parse("0.25")` → 15 min
- `parse("1.5")` → 90 min
- `parse("18m")` → 18 min
- `parse("90m")` → 90 min
- `parse("1h")` → 60 min
- `parse("1h30m")` → 90 min
- `parse("1h 30m")` → 90 min (whitespace tolerated)
- `parse("1:15")` → 75 min
- `parse("1:15:30")` → Err
- `parse("")` → Err
- `parse("abc")` → Err
- `parse("0")` → Err (per §3.3)
- `parse("0.0")` → Err
- `parse("-0.5")` → Err
- `parse("25h")` → Err (per §3.3, >24h sanity cap)

`src/csv_writer.rs`:
- `format_row` escapes commas, quotes, newlines in narrative
- `format_row` rounds `hours_decimal` to 2 places
- `format_row` emits RFC 3339 timestamp with offset
- Lines end with CRLF
- New file: writes UTF-8 BOM as first 3 bytes, then header row matching §3.7 column order
- Existing file: no header, no BOM, append only

`src/autocomplete.rs`:
- `"Cl"` ranks `"ClientCo"` before `"Acme Client"` (prefix > substring)
- Case-insensitive: `"client"` matches `"ClientCo"`
- Returns at most 5 suggestions
- Entries older than 90 days ignored
- Empty CSV → empty result, no panic

`src/timer.rs`:
- Start, advance clock 30m, elapsed = 30m
- Persisted across simulated restart, elapsed correct
- Wall-clock advancing >12h → cap triggers confirm prompt (per §3.2)

### 12.2 Integration tests (`tests/` dir + harness scripts)

- Atomic CSV write: write to `.tmp`, rename, verify content
- **Excel-lock E2E (implemented):** `scripts/test-excel-lock.ps1` drives `csv-lock-smoke.exe` against a real Windows ERROR_SHARING_VIOLATION held by a background PowerShell job. Three modes:
  - `-Mode short` (1s lock) → expects `WRITTEN` (retry drains within RETRY_TOTAL=10s)
  - `-Mode long` (12s lock) → expects `QUEUED` (retry exhausts; entry persists to `%LOCALAPPDATA%\TimeTracker\queue\*.json`)
  - `-Mode drain` → next successful write empties the queue
  - `-Mode all` runs all three with sandbox isolation via `TIMETRACKER_DATA_DIR_OVERRIDE` / `TIMETRACKER_CSV_DIR_OVERRIDE` env vars (production data never touched)
- Queue drain: enqueue 3 entries, release lock, drain via single-writer task, verify CSV has 3 rows in order, queue dir empty (covered by `-Mode drain`)
- Single-instance: spawn process twice, second exits cleanly, first receives `show_quick_entry` over named pipe (validated manually via PowerShell two-process launch)
- Path resolution: dev mode resolves to `%LOCALAPPDATA%\TimeTracker\`; mocked MSIX context resolves to package family path

### 12.3 Manual / acceptance

Mapped 1:1 to §8 acceptance criteria. Run on a clean Windows VM AND the partner's actual machine before install.
