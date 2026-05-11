# Time Tracker — Product Breakdown

> Self-contained product + technical brief. Snapshot of state as of 2026-05-09.
> Companion to `SPEC.md` (which is the working spec; this is the executive read).

---

## What it is

A Windows desktop time tracker for Ghumans Accounting (B-315). Hotkey opens a popup, partner types client + narrative + duration, hits Enter, a row lands in `%USERPROFILE%\TimeTracker\2026-05.csv`. Closes in under 5 seconds. Auto-starts on login, lives in the system tray, never asks for confirmation.

**Wedge thesis:** replace ad-hoc time tracking (paper, memory, manual Excel typing) with something so fast that staff actually use it. CSV format is intentionally Excel-native so partner reviews + invoices the same way they always have. Real usage data justifies a Part 2 product (email-to-financials automation) by quantifying the time saved.

**v1 customer:** one accounting partner, hand-delivered MSIX, bilateral invoice. No public download, no auto-update, no payment-gating infra. All deferred to v1.1 once customer #2 is in sight.

---

## User flows

**Quick entry (the 80% case):**

```
Partner just got off a 20-min call with ClientCo.
  ↓
Ctrl+Shift+H               (popup appears at cursor monitor, <100ms)
  ↓
"ClientCo"                 (autocomplete shows previous matches inline)
  ↓ Tab
"phone re K-1 questions"
  ↓ Tab
"0.3"                      (or "18m" / "1h30m" / "1:15" - all parsed)
  ↓ Enter
Toast: "Logged 0.3h to ClientCo"
  ↓
CSV row appended atomically. Self-verified. Popup gone.
Total: ~4 seconds.
```

**Live timer (split start/stop):**

```
Ctrl+Shift+;               (Start timer popup - same fields minus duration)
  ↓ Submit
Timer running, persisted to disk every state change.
... 47 minutes later ...
Ctrl+Shift+'               (Stop timer popup - pre-filled with elapsed)
  ↓ Edit duration to 0.75 (rounded)
  ↓ Enter
CSV row written, timer cleared.
```

**End of day:** tray menu → "Open this month's CSV" → opens in Excel for partner review/billing.

**Reboot:** auto-start fires on login, tray icon present, hotkeys live, no user action.

**Excel open on the file:** partner has CSV open in Excel, hits hotkey, logs entry. Atomic temp+rename retries with exponential backoff up to 10s; if still locked, queues to `%LOCALAPPDATA%\TimeTracker\queue\` and surfaces a tray badge. Drains on next successful write.

---

## Architecture (rocket-ship view)

No wrapper libraries. Every layer wired by hand so future iteration can reach into any of them. The principle: build rocket ships, not the fastest car — slow setup, then leaves wrappers behind permanently.

```
┌──────────────────────────────────────────────────────────────┐
│  Time Tracker  (Rust binary, x86_64-pc-windows-msvc) │
├──────────────────────────────────────────────────────────────┤
│  Application layer                                           │
│    popup.rs        egui form + 5-field state machine         │
│    timer.rs        running-timer state + 12h sanity cap      │
│    csv_writer.rs   single-writer task + retry/queue/verify   │
│    autocomplete.rs 90-day cache + prefix-rank-then-substring │
│    duration.rs     "0.3"/"18m"/"1h30m"/"1:15" parser         │
│    config.rs       config.toml subset (identity + defaults)  │
│    usage.rs        JSONL instrumentation (PII-free)          │
│    crash.rs        panic_hook → panic-{ts}.log               │
│    single_instance.rs  named mutex + Win32 named-pipe IPC    │
│    paths.rs        MSIX-aware path resolution helper         │
│    logging.rs      tracing + daily rotation + 30-day retain  │
├──────────────────────────────────────────────────────────────┤
│  Integration primitives (assembled by hand, no wrappers)     │
│    winit            single Win32 message pump                │
│    tray-icon        shell-notification tray + right-click    │
│    global-hotkey    RegisterHotKey + WM_HOTKEY               │
│    glutin           OpenGL context (WGL on Windows)          │
│    glow             GL bindings                              │
│    egui_glow        egui → GL renderer                       │
│    egui-winit       winit events → egui input                │
│    egui             immediate-mode UI                        │
│    windows-sys      Win32 syscalls (mutex/pipe/console)      │
│    tracing+appender daily-rotated structured logs            │
├──────────────────────────────────────────────────────────────┤
│  OS                                                          │
│    Windows 10+ (MSIX runFullTrust + uap5:StartupTask)        │
└──────────────────────────────────────────────────────────────┘
```

**Hot-path event flow:**

```
  Hotkey       Tray menu     2nd-instance .exe launch
  Ctrl+Shift+X (right-click)   ↓
      │           │      single_instance::acquire_or_notify
      │           │           ↓ (mutex held: send pipe msg + exit)
      │           │      \\.\pipe\TimeTracker
      │           │           ↓
      │           │      pipe-server thread (1st instance)
      │           │           ↓ EventLoopProxy.send_event
      ▼           ▼           │
   ┌────────────────────────────────────┐
   │  winit::EventLoop  (single pump)   │
   └────────────────┬───────────────────┘
                    │
                    ▼
            popup.show(mode)
                    │
        set_visible + focus + request_redraw
                    │
                    ▼
   egui frame:
   ├ Client field (autocomplete suggestions inline)
   ├ Engagement field (autocomplete)
   ├ Narrative (multi-line)
   ├ Duration ("0.3" → 18 min via duration::parse)
   └ Billable checkbox
                    │
              Submit (Enter)
                    │
                    ▼
   csv_writer::write_blocking         (sync, ~10-30ms on SSD)
   ├ atomic temp+rename                (per SPEC §4.1)
   ├ post-write self-verify            (catches AV/EDR rollback)
   ├ exponential-backoff retry         (Excel-lock case)
   └ queue fallback at 10s             (drain on next success)
                    │
                    ▼
   autocomplete::observe(client, engagement)
   usage::entry_written(method, minutes, billable)
   toast: "Logged 0.3h to ClientCo"
   popup.close("submit")
```

---

## On-disk layout

```
%LOCALAPPDATA%\TimeTracker\
    config.toml                     identity.staff, defaults.billable
    autocomplete.json               cached client + engagement names (90-day window)
    timer-state.json                ONLY present when a timer is running
    queue\
        20260508-152311-XXXXXX.json queued entries (locked-CSV fallback)
    logs\
        time-tracker.2026-05-08          tracing daily-rotated logs
    crashes\
        panic-20260508-152311.log   panic dumps
    usage.log                       JSONL instrumentation (one event per line)

%USERPROFILE%\TimeTracker\         NOT under Documents (bypasses OneDrive
                                    Known-Folder redirect that would silently
                                    sync client narratives to MS cloud)
    2026-05.csv                     UTF-8 BOM + CRLF, 9 columns, one row per entry
    2026-04.csv
```

---

## CSV format

Header (written once on file create, after UTF-8 BOM):

```
timestamp_iso,staff,client,engagement,narrative,minutes,hours_decimal,billable,entry_method
```

Example row:

```
2026-05-08T15:23:11-07:00,Ryan,ClientCo,K-1,phone re K-1 questions,18,0.30,true,quick
```

- **`hours_decimal`** rounded to 2 places (CPA convention).
- **`billable`** as `true` / `false`.
- **`entry_method`** as `quick` / `timer`.
- **CRLF** line endings, **UTF-8 with BOM**, RFC 4180 quoting (commas, quotes, newlines in narrative get quoted; embedded quotes doubled).
- **Append-only.** Edits happen in Excel.
- One file per month, rolls automatically.

---

## Hotkeys (shipping `--features live-view` build)

| Default | Action | Notes |
|---|---|---|
| **Ctrl+Shift+H** | Open popover (add-workstream form expanded) | Quick one-off blocks: tray menu → "Log a block…" → the egui quick-entry popup. |
| **Ctrl+Shift+;** | Start timer (popup for fields) | Right-pinky/ring. Adjacent to `'`. |
| **Ctrl+Shift+'** | **Stop timer — immediate**, writes the entry, no popup/rounding/cancel | Rows are editable on the Recorded Time page; brief "stopped — undo" affordance in the popover. |
| **Ctrl+Shift+/** | Toggle the popover | |

(The `--no-feature` tray-only build keeps the older set: `Ctrl+Shift+H` = quick-entry popup, `Ctrl+Shift+'` = confirmable stop popup, no `Ctrl+Shift+/`. SPEC §3.1–3.2 describe that build.)

**Rebinds:** `config.toml` has a `[hotkeys]` section (commented-out template written on first launch) — combo syntax `"Ctrl+Shift+H"`. A typo'd rebind falls back to the default and logs it.

**Known Excel conflict (disclosed during install):** `Ctrl+Shift+;` and `Ctrl+Shift+'` are bound in Excel (insert-time, copy-value-above). The global hotkeys intercept those bindings while the app runs. Rebindable via `[hotkeys]`. Other Outlook/browser conflicts (Ctrl+Shift+I/O/P) explicitly avoided in the default set.

**Hotkey reg failure** (some other app already owns the combo): a `hotkey_registered { which, ok }` usage event + a one-shot MessageBox at launch listing the dead combos and pointing at `config.toml`. (Tray-icon dynamic warning state is still v1.1.)

---

## Source tree

```
time-tracker/
├── SPEC.md                         single-source-of-truth product+tech spec
├── PRODUCT.md                      this file
├── Cargo.toml / Cargo.lock         deps pinned, both bins declared
├── .gitignore                      target/, *.msix, smoke.out, etc.
├── src/
│   ├── main.rs                     winit event loop + subsystem wiring
│   ├── popup.rs                    egui popup window + 5-field UI + submit
│   ├── timer.rs                    timer state + persistence + 12h cap (+ tests)
│   ├── csv_writer.rs               atomic CSV append + retry/queue (+ tests)
│   ├── autocomplete.rs             cache + rank + minimal CSV parser (+ tests)
│   ├── duration.rs                 user-input parser (+ 25 tests covering SPEC §12.1)
│   ├── config.rs                   config.toml read/write
│   ├── usage.rs                    JSONL instrumentation events
│   ├── crash.rs                    panic_hook + recent-crash detection
│   ├── single_instance.rs          mutex + named-pipe IPC
│   ├── paths.rs                    %LOCALAPPDATA% + %USERPROFILE% resolution (+ pre-create placeholders)
│   ├── logging.rs                  tracing setup + 30-day retention enforcement
│   └── bin/smoke_hotkey.rs         item-0 architectural regression guard (~80 lines)
└── msix/
    ├── AppxManifest.xml            identity, runFullTrust, autostart, virt-opt-out
    ├── build-msix.ps1              MakeAppx + signtool wrapper (auto-discovers SDK paths)
    ├── generate-placeholder-icons.ps1   System.Drawing icon-gen (no extra deps)
    └── README.md                   Windows-side packaging instructions + ceremony recap
```

---

## Key decisions (and what was rejected)

| Decision | Alternative | Why this |
|---|---|---|
| Rust + winit + egui_glow + glutin (primitives) | eframe wrapper | Build rocket ships, not fastest cars. eframe owns the loop, hides control points future iteration may need. |
| Cross-compile from WSL via `cargo-xwin` | Install Rust on Windows | No Windows-side toolchain needed for builds. MSVC SDK auto-downloaded once. Build cycle stays in WSL. |
| Single winit event loop owns process | Multiple loops with channel coordination | tray-icon + global-hotkey events are channel-based; one loop suffices. |
| Pre-warmed popup window | Cold-create on each hotkey | egui first-paint compiles shaders + builds font atlas (200-400ms). Pre-warm pays the cost at app start, hotkey path is just `set_visible(true)`. |
| Sync CSV write before popup close | Async write after close | 50ms inline write is invisible UX; durability matters more than perceived latency for billable hours. |
| Atomic temp+rename + post-write self-verify | Direct append + fsync | Survives crash mid-write AND catches AV/EDR rollback. |
| Single-writer task with mpsc channel | Locks around the file | Eliminates queue/drain race conditions. All writes serialize naturally. |
| Named-pipe IPC for second-instance show | Just exit silently | Second launch focuses existing instance with the requested popup mode — better UX than "nothing happens." |
| Wall-clock timer with 12h sanity cap | Monotonic clock | Monotonic doesn't survive serialization. Wall-clock + a confirm prompt for >12h is robust enough for the partner's day. |
| Hotkeys: Ctrl+Shift+H/;/' | Ctrl+Alt+T/R | Conflicts in user's stack. Researched landscape; H/;/' minimize conflict in target apps (Outlook/Excel/QB/Edge). |
| CSV path: `%USERPROFILE%\TimeTracker\` | `%USERPROFILE%\Documents\TimeTracker\` | Documents folder silently redirects to OneDrive when backup is on; client narratives would auto-sync to MS cloud. Explicit opt-in only. |
| `desktop6:FileSystemWriteVirtualization=disabled` | Default MSIX virtualization | `%LOCALAPPDATA%` writes go to a per-package shadow otherwise. |
| Hand-delivered MSIX, no auto-update for v1 | `.appinstaller` + GitHub Releases | Private/paid product, single customer. Auto-update + payment infra deferred to v1.1. |
| TOML config + JSONL usage log + structured `tracing` for app logs | Single log format | Each format optimized for its consumer: TOML for human edit, JSONL for analysis, tracing for live debugging. |

---

## Build & deployment

**WSL side (cross-compile):**
```bash
cd /mnt/c/Users/RyanJ/projects/time-tracker
cargo xwin build --release --target x86_64-pc-windows-msvc --bin time-tracker
# Produces: target\x86_64-pc-windows-msvc\release\time-tracker.exe (4.2 MB)
```

**Windows side (5 commands + 1 manifest edit, ~10 minutes total):**
```powershell
cd C:\Users\RyanJ\projects\time-tracker

# Generate placeholder visual assets (one-time)
.\msix\generate-placeholder-icons.ps1

# Edit msix\AppxManifest.xml: set Publisher CN to match your code-signing
# cert's Subject (find via: Get-ChildItem Cert:\CurrentUser\My)

# Build + sign
.\msix\build-msix.ps1                          # auto-finds cert via /a
# OR: .\msix\build-msix.ps1 -Thumbprint AABBCC...

# Test install on your machine first
Add-AppxPackage .\release\RyanStewart.TimeTracker.msix

# Cleanup if needed
Get-AppxPackage *TimeTracker* | Remove-AppxPackage
```

**Customer install ceremony** (per SPEC §6.4): drive to partner with the `.msix` on a thumb drive. SmartScreen prompt → first-launch self-test → "Start with Windows?" consent → set staff name → demo all three hotkeys → disclose Excel-takeover → demo Excel-while-write retry → leave printed cheat sheet.

---

## Verification status

| Capability | Verified by |
|---|---|
| Cross-compile produces working .exe | `cargo xwin build` → 4.2 MB PE32+ binary |
| Process boots, registers tray + 3 hotkeys | PowerShell launch, stderr shows "Tray icon active. Hotkeys live." |
| Single-instance enforcement | Two-process test: B detects mutex, sends "show_quick_entry", exits |
| Named-pipe IPC roundtrip | A's log shows `INFO ui: popup shown mode=QuickEntry` after B's launch |
| Popup window actually appears | Get-Process detected visible window titled "Time Tracker" |
| Data layout creation | Fresh-state launch creates `%LOCALAPPDATA%\TimeTracker\` + 5 placeholder files |
| Documents path bypasses OneDrive | `C:\Users\RyanJ\TimeTracker` exists; `C:\Users\RyanJ\Documents\TimeTracker` does NOT |
| Tracing logs are written | `time-tracker.2026-05-09` file with structured INFO events for start/hotkey/tray/popup-shown |
| config.toml created on first launch | Default content with `staff = "RyanJ"` |
| usage.log app_start event | Valid JSONL line with timestamp + version |
| SIGINT handler installs cleanly | smoke_hotkey.exe responds to Ctrl+C |

| Not yet verified (needs human keystrokes or a real Excel hold) |
|---|
| Hotkey actually fires popup from a real keypress |
| Submit actually writes a CSV row + creates `%USERPROFILE%\TimeTracker\2026-05.csv` |
| Cancel/Esc hides popup |
| Toast renders + auto-dismisses |
| Excel-lock retry path under real Excel hold |

---

## Spec coverage

| §7 Item | Status |
|---|---|
| 0a Bare-exe hotkey smoke test | ✅ |
| 0b MSIX-packaged smoke under runFullTrust | ⏸ Folded into real-app packaging |
| 1 Cargo + manifest scaffold | ✅ |
| 2 Single tao→winit event loop | ✅ |
| 3 Tray icon + menu + Quit | ✅ |
| 4 Single-instance mutex + named-pipe IPC | ✅ |
| 5 Logging + path resolution + placeholders | ✅ |
| 6 Hotkey reg + conflict surfacing | ✅ logged; ⏸ tray dynamic warning state deferred |
| 7 Pre-warmed popup window | ✅ |
| 8 Quick-entry popup with 5 fields | ✅ |
| 9 CSV append with atomic write + retry + queue + self-verify | ✅ |
| 10 Autocomplete from CSV | ✅ |
| 11 Toast on success | ✅ |
| 12 Timer mode + persistence | ✅ |
| 13 Settings UI | ⏸ deferred — config.toml editable directly for v1 |
| 14 Autostart manifest entry | ✅ in `AppxManifest.xml` |
| 15 Usage instrumentation hooks | ✅ |
| 16 Crash handler | ✅ panic_hook fallback |
| 17 Test pass | ⏳ tests written for duration/csv_writer/autocomplete/timer (~50 cases); `cargo test` runs deferred until Windows-side cargo |
| 18 MSIX build, sign | ⏳ scaffold ready; user runs |
| 19-21 Test install + customer install | ⏳ user actions |

**Out of scope for v1** (queued for v1.1+):
- Settings UI window
- Tray icon dynamic warning state on hotkey reg failure
- `.appinstaller` auto-update + payment-gated distribution
- Stripe/Gumroad signed download URLs
- minidumper full crash dumps
- Diagnostics export "send to developer" auto-upload
- Multi-staff merging
- Accounting-system sync
- Calendar import (Outlook → entries)
- Idle detection
- Mobile companion
- macOS / Linux

---

## Where this compounds

Reusable primitives for any subsequent Windows desktop product:

| Primitive | Lives in | Reusable for |
|---|---|---|
| WSL→Windows cross-compile via cargo-xwin | toolchain | Every future Rust+Windows project |
| Single winit event loop with channel-polled subsystems | `main.rs` pattern | Any tray + hotkey + window app |
| Pre-warmed window pattern (created hidden, shown on demand) | `popup.rs` | Any app where "open instantly" matters |
| Single-writer mpsc task for append-mostly files | `csv_writer.rs` | Any log/journal/audit-trail file |
| Atomic temp+rename + post-write self-verify | `csv_writer.rs`, `config.rs`, `timer.rs`, `autocomplete.rs` | Any persistent state file on Windows |
| MSIX-aware path resolution + placeholder pre-creation | `paths.rs` | Any MSIX-packaged app |
| Named-pipe IPC for single-instance show | `single_instance.rs` | Any "second launch focuses existing" UX |
| Win32 SetConsoleCtrlHandler for clean SIGINT | `main.rs` + `smoke_hotkey.rs` | Any tray app that runs from a console during dev |
| MSIX manifest + signtool packaging script | `msix/` | Any MSIX-packaged Rust app |
| `tracing` + daily rotation + retention enforcement | `logging.rs` | Any long-running desktop daemon |
| JSONL usage instrumentation pattern | `usage.rs` | Any product where Part-2 needs quantified pain |

The principle: every primitive above is hours-of-future-savings, not just one product's plumbing. We don't fight about being the fastest car. We build rocket ships so it looks slow but after you never lose.
