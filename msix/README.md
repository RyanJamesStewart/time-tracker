# MSIX packaging

Per SPEC §6.1 / §6.4. Hand-delivered v1; auto-update infra deferred to v1.1.

## One-time prerequisites (Windows side)

1. **Windows 10 SDK** — provides `MakeAppx.exe` and `signtool.exe`.
   Default install path: `C:\Program Files (x86)\Windows Kits\10\bin\<ver>\x64\`.
2. **Code-signing cert** in your user's Personal cert store. The same
   cert used for prompt-time deployment per SPEC §6.1. Find the
   thumbprint with:
   ```powershell
   Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -like '*Ryan Stewart*' } | Format-List Subject, Thumbprint
   ```

## One-time edits before first build

- `AppxManifest.xml` — set `Identity Publisher="CN=..."` to **exactly** match
  the Subject of the code-signing cert. signtool will reject mismatches.

## Build flow

From WSL (cross-compile):
```bash
cd /mnt/c/Users/RyanJ/projects/time-tracker
cargo xwin build --release --target x86_64-pc-windows-msvc --bin time-tracker
```

From PowerShell on Windows side (in the repo root):
```powershell
# First time: generate placeholder visual assets
.\msix\generate-placeholder-icons.ps1

# Build + sign
.\msix\build-msix.ps1                          # uses the "best" cert via /a
.\msix\build-msix.ps1 -Thumbprint AABBCC...    # pin a specific cert
.\msix\build-msix.ps1 -SkipSign                # build unsigned for local testing
```

Output: `release\RyanStewart.TimeTracker.msix`.

## Install (what the partner does)

`build-msix.ps1` leaves two files in `release\`:

```
release\
  RyanStewart.TimeTracker.msix     the signed package — this is what ships
  install.ps1                      power-user helper (-Uninstall / -Launch / WebView2 check)
```

Publishing a release: upload the `.msix` (renamed to something self-documenting
like `Install-right-click-then-press-Install.msix`) as the only asset on a
GitHub Release. The filename itself is the partner instruction. They:

1. Download the `.msix` from the Release page (lands as a single file in their
   Downloads — no zip, no subfolder).
2. **Right-click → Install** (or double-click — same App Installer dialog).
3. App Installer shows "Publisher: Ryan Stewart" — they press **Install**.

No command line, no administrator, no certificate import, no warning. The
chain ends at the Microsoft Identity Verification Root CA, which Windows
already trusts on Win 10 1809+ and Win 11.

**Why no installer `.bat` anymore:** `.bat` files can't be Authenticode-signed,
so Windows always slaps a "Publisher cannot be verified" warning on a `.bat`
downloaded from the web — even one that calls a perfectly signed payload.
Shipping the signed `.msix` itself sidesteps that entirely.

**Uninstall** (their logged time + config are kept either way): Start menu →
right-click *Time Tracker* → Uninstall, or Settings → Apps → Time Tracker →
Uninstall. Power-user equivalents from PowerShell:
`Get-AppxPackage *TimeTracker* | Remove-AppxPackage`, or
`powershell -ExecutionPolicy Bypass -File .\release\install.ps1 -Uninstall`.

Dev/test on this machine:
```powershell
Add-AppxPackage -Path .\release\RyanStewart.TimeTracker.msix -ForceApplicationShutdown
Get-AppxPackage RyanStewart.TimeTracker | Format-List
```

## What lands inside the package

```
RyanStewart.TimeTracker.msix
├── AppxManifest.xml         identity, capabilities, autostart, virt opt-out
├── time-tracker.exe the cross-compiled release binary
└── Assets/
    ├── StoreLogo.png        50x50 (Properties/Logo)
    ├── Square44x44Logo.png  44x44 (taskbar/Start)
    ├── Square150x150Logo.png 150x150 (medium tile)
    └── Wide310x150Logo.png  310x150 (wide tile)
```

## Capabilities declared

- `runFullTrust` — required for global hotkey registration + Win32 named pipe IPC.
- `windows.startupTask` (uap5) — REQUESTS autostart on user login.
  Windows enforces user opt-in via Settings → Apps → Startup; the manifest
  cannot enable it unilaterally. First-launch UX should walk the partner
  through this (per SPEC §6.4).
- `windows.fileSystemWriteVirtualization=disabled` (desktop6) — opts the
  package out of the per-package shadow that would silently redirect
  `%LOCALAPPDATA%\TimeTracker` writes (per the prompt-time learning recorded
  in SPEC §6.1).

## Install ceremony reference

Per SPEC §6.4, reconciled with the **shipping `--features live-view` build**
(SPEC §3.1–3.2 describe the older `--no-feature` tray-only hotkey set — that
build's `Ctrl+Shift+H` = quick-entry popup and `Ctrl+Shift+'` = confirmable
stop popup are NOT what ships). On the partner's machine:
0. **Right-click the `.msix` → Install** (downloaded from the GitHub Release as
   `Install-right-click-then-press-Install.msix`). App Installer shows
   "Publisher: Ryan Stewart"; they press **Install**. No warning, no admin, no
   cert step, no command line. — *the steps below are post-install setup, not
   part of the install itself.* (Windows 10 machines without WebView2: if a
   panel ever shows blank, grab WebView2 from <https://aka.ms/webview2>. Win 11
   ships with it.)
1. First-launch self-test banner (`crash::first_launch_self_test` — a
   write-and-readback in `%USERPROFILE%\TimeTracker\`; one-line pass/fail box).
   (A "crashed last session — send the diagnostic?" box only appears if the
   *previous* run crashed; first run won't show it.)
2. "Start with Windows?" — toggle it on in Settings → Apps → Startup if they
   want autostart (Windows requires the user to enable the MSIX StartupTask; the
   manifest only requests it, and `[startup] enabled = false` in `config.toml`
   keeps the app from arming any other autostart). Optional.
3. Set staff name in `%LOCALAPPDATA%\TimeTracker\config.toml`
   (`[identity] staff = "…"`) → quit + relaunch. The file also has a
   commented-out `[hotkeys]` template for rebinds (live now — a hand-edited
   rebind takes effect on next launch). Settings *GUI* is v1.1.
4. Demo the **four** hotkeys of the live-view build:
   - `Ctrl+Shift+H` → opens the tray **popover** with the add-workstream form
     expanded (NOT a quick-entry popup — that's the tray "Log a block…" item)
   - `Ctrl+Shift+;` → start-timer popup
   - `Ctrl+Shift+'` → **stops the timer immediately + writes the entry, no
     popup**. Disclose this. (There's an "undo" affordance; rows are also
     editable on the Recorded Time page.)
   - `Ctrl+Shift+/` → toggles the popover
5. **Excel hotkey takeover disclosure**: "Once this is running,
   `Ctrl+Shift+;` and `Ctrl+Shift+'` trigger the timer instead of Excel's
   insert-time / copy-value-above. Rebind via `[hotkeys]` in `config.toml`."
6. Show CSV file location: `%USERPROFILE%\TimeTracker\YYYY-MM.csv`; show the
   Recorded Time / Export pages at `http://localhost:17893/recorded`.
7. Demo: open the CSV in Excel, log a new entry via a hotkey, verify it
   appears once Excel releases (proves the Excel-lock retry + queue path).

## Updating

For v1, updates are hand-delivered: publish a fresh GitHub Release with the
new signed `.msix`; the partner downloads it and right-clicks → Install again.
App Installer detects the prior version and prompts to update in place. On
this dev machine:
```powershell
Add-AppxPackage -Path .\release\RyanStewart.TimeTracker.msix -ForceApplicationShutdown
```
Either way the user's data (`%LOCALAPPDATA%\TimeTracker\`, `%USERPROFILE%\TimeTracker\`)
is preserved.

Auto-update via `.appinstaller` deferred to v1.1 alongside payment-gated
distribution (per SPEC §6.2 / §6.3).

## Rollback (when install or upgrade goes sideways)

User data is the priority. Logged time entries (`%USERPROFILE%\TimeTracker\*.csv`)
and config (`%LOCALAPPDATA%\TimeTracker\config.toml`) are never touched by
`Remove-AppxPackage`. Reinstalls likewise leave them in place.

**If `Add-AppxPackage` fails** (signature mismatch, dependency error, version
downgrade refused):
```powershell
# 1. Inspect the failure detail (Add-AppxPackage's error message is usually generic)
Get-AppxLog -All | Where-Object { $_.Message -match 'TimeTracker' } | Select-Object -First 5

# 2. If a partial state landed, force-remove all variants
Get-AppxPackage *TimeTracker* | Remove-AppxPackage -ErrorAction Continue

# 3. Confirm clean slate
Get-AppxPackage *TimeTracker*    # expect: nothing

# 4. Reinstall the LAST known-good MSIX (keep one prior signed build on disk)
Add-AppxPackage -Path .\release\RyanStewart.TimeTracker-LASTGOOD.msix
```

**If the new version starts but misbehaves** (hotkeys silent, popup won't show,
crash on launch):
```powershell
# Pull recent logs + crash dumps for diagnosis BEFORE rolling back
Copy-Item "$env:LOCALAPPDATA\TimeTracker\logs\*"     $env:TEMP\TimeTracker-rollback-logs\   -Recurse
Copy-Item "$env:LOCALAPPDATA\TimeTracker\crashes\*"  $env:TEMP\TimeTracker-rollback-logs\   -Recurse

# Then downgrade. MSIX refuses lower Version by default; pass -ForceApplicationShutdown
# and (if needed) bump the rollback MSIX's Identity Version to be HIGHER than the
# bad one even though the code is older. Keep a "rollback-bumped" copy ready.
Get-AppxPackage *TimeTracker* | Remove-AppxPackage
Add-AppxPackage -Path .\release\RyanStewart.TimeTracker-ROLLBACK.msix
```

**If user data is corrupted** (rare: typically AV/EDR rollback or disk full mid-write
defeating the post-write self-verify):
```powershell
# 1. Snapshot before touching anything
Compress-Archive -Path "$env:USERPROFILE\TimeTracker","$env:LOCALAPPDATA\TimeTracker" `
    -DestinationPath "$env:USERPROFILE\Desktop\time-tracker-recovery-$(Get-Date -Format yyyyMMdd-HHmmss).zip"

# 2. Restore last-good monthly CSV from the snapshot or from prior backup tooling.
#    (CSVs are append-only; a prior copy is always equal-or-shorter than current.)
```

Keep at least one prior signed `.msix` and the most recent
`%USERPROFILE%\TimeTracker\` snapshot before delivering an update.

## Time / clock notes

CSV `timestamp_iso` is `Local::now()` formatted as RFC 3339 with offset (e.g.
`2026-05-09T14:32:11-07:00`). Two real-world wrinkles to document for
billing-side consumers:

- **Clock skew during a session:** if Windows runs an NTP correction or
  the user manually adjusts the system clock between timer-start and
  timer-stop, elapsed minutes are computed from wall-clock deltas. A
  backwards jump > 12h trips the sanity cap (per SPEC §3.2) and prompts
  the user. Smaller skews silently affect elapsed; we don't try to be
  clever about it because Wall-clock-vs-monotonic is the user's choice
  to make. When in doubt, look at `started_at` in
  `%LOCALAPPDATA%\TimeTracker\timer-state.json` and the `timestamp_iso` of
  the resulting row.
- **DST transitions** are absorbed into the offset suffix in
  `timestamp_iso`. Two consecutive entries straddling the spring-forward
  hour will show different offsets; this is correct but worth flagging
  if a downstream tool sorts by string-as-clock instead of parsing.

## Locale assumption (en-US)

The duration parser accepts decimal hours with `.` as the separator
(`0.3`, `1.75`). Users on a comma-decimal locale (de-DE, fr-FR, etc.)
typing `1,5` will see a parse error. v1 ships en-US only; comma-decimal
input is a v1.1 follow-up. CSV `hours_decimal` is also written with `.`
regardless of system locale, so Excel / accounting-system imports are stable
across staff machines with different regional settings.
