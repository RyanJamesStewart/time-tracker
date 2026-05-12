# Time Tracker

### ⬇ [Download the installer](https://github.com/RyanJamesStewart/time-tracker/releases/latest/download/Install.msix) &nbsp;·&nbsp; [All releases](https://github.com/RyanJamesStewart/time-tracker/releases)

A small, fast Windows desktop time tracker. Hotkeys to start/stop a timer or log
a block, a tray icon, and a built-in dashboard for reviewing your time and
producing a billing-ready export (CSV + PDF). Everything is local — your time
log is a plain CSV on your own disk; nothing leaves the machine.

## Install

The signed installer isn't kept in this source tree (binaries bloat git history
and would have to be re-committed on every build); it lives on the
**[Releases page](https://github.com/RyanJamesStewart/time-tracker/releases/latest)** — also linked from the
*Releases* widget in the right sidebar of this repo's main page.

1. Click **[Download the installer](https://github.com/RyanJamesStewart/time-tracker/releases/latest/download/Install.msix)** above. One file (`Install.msix`) lands in your Downloads, with a short `README.txt` beside it on the Releases page — double-click it and press Install.
2. **Right-click → Install** (or double-click — same App Installer dialog).
3. Press **Install** in the App Installer dialog. Done — no warning, no admin, no command line, no certificate import.

The package is signed via Azure Trusted Signing; its chain ends at the Microsoft Identity Verification Root CA, which Windows already trusts on Windows 10 1809+ and Windows 11. The dashboard and tray popover need the Edge WebView2 runtime — it ships with Windows 11; on Windows 10, grab it from <https://aka.ms/webview2> if a panel ever shows blank.

**Uninstall** (keeps your logged time and settings): **Start menu → right-click *Time Tracker* → Uninstall** (or *Settings → Apps → Time Tracker → Uninstall*).

## Using it

Once running, there's a clock icon in the system tray. Global hotkeys (work from
anywhere — rebindable in `%LOCALAPPDATA%\TimeTracker\config.toml` under `[hotkeys]`):

| Hotkey | Action |
| --- | --- |
| `Ctrl+Shift+/` | Open / close the tracker popover |
| `Ctrl+Shift+'` | Open the popover's workstream filter — type to filter, ↓/Enter to pick (picking a workstream starts a timer on it) |
| `Ctrl+Shift+;` | Stop the running timer — writes the entry immediately, no confirm |
| `Ctrl+Shift+H` | Open the popover ready to add a workstream (client · engagement) |

There's no separate *start-timer* hotkey: you start a timer by opening the
popover (`Ctrl+Shift+/`, or `Ctrl+Shift+'` to filter), picking the workstream
you're working on, and pressing Enter. `Ctrl+Shift+;` stops it.

- **One-off blocks**: right-click the tray icon → *Log a block…*.
- **Review / edit / export**: open the dashboard from the popover, or go to
  `http://localhost:17893/recorded` in a browser. The Export page rolls your
  time up by client · engagement and writes `YYYY-MM` CSV + PDF files to
  `%USERPROFILE%\TimeTracker\exports\`.

If a hotkey is already claimed by another app, Time Tracker tells you on launch
— change it in `config.toml`.

## Where your data lives

- **Time entries** — `%USERPROFILE%\TimeTracker\YYYY-MM.csv` (one file per
  month, append-only, UTF-8 + BOM so Excel opens it cleanly). Editing it in Excel
  while the app runs is safe — the app retries around the lock and queues if
  needed.
- **Exports** — `%USERPROFILE%\TimeTracker\exports\`.
- **Settings, logs, crash dumps** — `%LOCALAPPDATA%\TimeTracker\`.
- **The local dashboard server** binds `127.0.0.1:17893` only and rejects
  cross-origin requests; it's reachable from your machine only.

## Building from source

Cross-compiled for Windows from WSL via [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin):

```bash
cargo xwin build --release --target x86_64-pc-windows-msvc --features live-view --bin time-tracker
cargo test --lib --target x86_64-unknown-linux-gnu        # the platform-agnostic tests
```

Then, on Windows, package + sign + bundle the installer:

```powershell
.\msix\build-msix.ps1 -SkipSign      # makes release\Install.msix (+ README.txt)
.\msix\sign-trusted.ps1              # signs it via Azure Trusted Signing
```

The `--features live-view` build (the one that ships) includes the localhost
dashboard, the Recorded Time / Export pages, and the WebView2-hosted tray
popover. A no-feature `--bin time-tracker` build is the tray-only fallback.

See [`msix/README.md`](msix/README.md) for the full packaging / install / rollback
reference, and `SPEC.md` for the original product spec.

## Status

Pre-1.0, in use. Known v1.1 follow-ups: a Settings GUI (config is hand-edited
for now), an "undo last stop" affordance, and auto-update via `.appinstaller`.
