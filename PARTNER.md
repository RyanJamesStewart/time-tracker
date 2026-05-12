# Time Tracker — Install + Quick Reference

For the Wed-AM hand-deliver. One side of one page. Print this.

## Install (you do this with the partner watching)

The package is signed with **Azure Trusted Signing** — the cert chains to a root
Windows already trusts, so there's no cert-import step and no admin needed. The
partner just opens the package.

1. **Copy `Install.msix` from the USB** to the partner's machine
   (Desktop is fine). That's the only file you need.
2. **Double-click the `.msix`** in File Explorer (not from an email preview pane).
   App Installer opens → click **Install**. Per-user, no admin.
   - If Windows shows a brief SmartScreen prompt ("Windows protected your PC" /
     "unrecognized app") → click **More info** → **Run anyway**. This is just
     download-reputation, not a trust failure — it clears as installs accumulate;
     the signature itself is trusted from day one. (Less likely to fire at all
     since this came off a USB, not a browser download.)
3. **Launch once from the Start menu** (search "TimeTracker") so it creates its data
   folder. Tray icon appears — on Win11 the partner may need the chevron `^` to
   see hidden tray icons.
4. **Set staff name**: edit `%LOCALAPPDATA%\TimeTracker\config.toml`, set
   `[identity] staff = "<partner first name>"`, then quit + relaunch the tracker.
5. **Test the four hotkeys live with the partner** (this is the silent-failure
   prevention). The shipping build is the **live-view build** — its hotkeys:
   - `Ctrl+Shift+H` → opens the tray **popover**, with the "add workstream"
     form expanded. (Quick one-off block entry is the **"Log a block…"** item
     in the tray menu, not a hotkey.)
   - `Ctrl+Shift+;` → start-timer popup appears.
   - `Ctrl+Shift+'` → **stops the running timer immediately and writes the
     entry — no popup, no rounding/cancel.** Tell the partner this: if they
     fat-finger it mid-task the timer stops; there's a brief **"stopped — undo"**
     affordance in the popover/dashboard, and the row is always editable on the
     Recorded Time page (`Ctrl+Shift+/` → popover → "Open Recorded Time", or
     `http://localhost:17893/recorded`).
   - `Ctrl+Shift+/` → toggles the popover (also: tray menu → "Open popover").
   - If any of these doesn't fire, the app shows a warning box on launch listing
     the conflict — read it together; rebind in `config.toml` under `[hotkeys]`
     (the file has a commented-out template).
   - *(This supersedes the older `Ctrl+Shift+H = quick-entry popup` / confirmable-
     stop description in SPEC §3.1–3.2, which describes the `--no-feature`
     tray-only build.)*

**Future updates:** ship a new `.msix`, signed the same way (Trusted Signing,
`public-trust` profile). The partner machine already trusts the chain, so the new
version installs over the old one — still no admin. Wire up an `.appinstaller`
URL later and updates become automatic + silent.

## How they use it (verbal walkthrough)

- **Hotkey/tray-driven, no window-management.** The popover (`Ctrl+Shift+/` or tray → "Open popover") is the daily driver — pick a workstream, start/stop the timer. Quick one-off blocks: tray → **"Log a block…"** → a small popup (client + engagement + narrative + duration like `1.5h`/`90m` + billable). The popup appears centered on the monitor under the cursor.
- **Timer**: `Ctrl+Shift+;` start (popup for fields) → walk away → come back → `Ctrl+Shift+'` stop. **Stop is immediate** — it writes the entry with the elapsed time, no confirm dialog. There's a brief "stopped — undo" affordance, and every row is editable later on the **Recorded Time** page (popover → "Open Recorded Time", or `http://localhost:17893/recorded`).
- **All entries land in `%USERPROFILE%\TimeTracker\<YYYY-MM>.csv`**. One file per month. Excel-friendly. Open whenever; edit in Excel or on the Recorded Time page.
- **Don't edit the CSV in Excel while the timer is running** — Excel locks the file; the tracker queues the entry and drains when Excel releases. End-of-day Excel review is fine. (The Recorded Time page also goes through the tracker's one writer thread, so editing there + a hotkey can't corrupt the file.)
- **If you sleep with a timer running**, the elapsed time may be very large — sanity-check it on the Recorded Time page and adjust the row. (The egui stop popup in the tray-only build prompts ">12h — confirm" here; the live-view build's immediate stop relies on the Recorded Time editor instead.)

## Tray menu

Right-click the tray icon → "Add workstream", "Start timer", "Stop timer", "Log a block…", "Open popover", Quit. Quit ends the tracker (it relaunches at next login if `<uap5:StartupTask>` is on and the partner consented in Settings → Startup Apps).

## Support — quick scripts the partner can paste

If something feels broken, ask the partner to open PowerShell and paste these:

### Where do my logs live?
```powershell
explorer "$env:LOCALAPPDATA\TimeTracker"
```

### Recent errors only
```powershell
Get-ChildItem "$env:LOCALAPPDATA\TimeTracker\logs" |
    Sort LastWriteTime -Desc | Select -First 1 |
    Get-Content -Tail 50 |
    Select-String 'ERROR|WARN'
```

### Send diagnostics zip (logs + settings + usage events, no client narratives)
```powershell
$ts = Get-Date -Format yyyyMMdd-HHmmss
$out = "$env:USERPROFILE\Desktop\time-tracker-diagnostics-$ts.zip"
Compress-Archive -Path `
    "$env:LOCALAPPDATA\TimeTracker\logs", `
    "$env:LOCALAPPDATA\TimeTracker\config.toml", `
    "$env:LOCALAPPDATA\TimeTracker\usage.log", `
    "$env:LOCALAPPDATA\TimeTracker\crashes" `
    -DestinationPath $out -Force
explorer "/select,`"$out`""
```
*Excludes `queue/` and `autocomplete.json` per SPEC §4.1+§4.4 PII scrubbing.*
Then email the zip to Ryan.

### "Tracker doesn't work after restart" — silent block check
Per `pane-asr-defender-rules.md` (N1), a managed-MSP environment may apply Defender ASR rules that block the app's launch. Symptom: app starts then exits with no obvious error. Check Event Log:

```powershell
Get-WinEvent -LogName 'Microsoft-Windows-Windows Defender/Operational' -MaxEvents 50 |
    Where Id -in 1121,1122 |
    Format-List TimeCreated, Id, Message
```
If `time-tracker.exe` shows up, the partner's MSP needs to add an ASR exclusion for the publisher cert. Forward the event to Ryan + their IT contact.

### "App won't install" — most common errors
| Symptom | Run | Fix |
|---------|-----|-----|
| `0x80073CFB` | nothing | DON'T enter UAC creds. Per-user install. Re-download as the same user. |
| `0x8007000B` | nothing | Cert mismatch — Ryan's cert reissue. |
| Updates "stop working" silently | `Get-AppxPackage Microsoft.DesktopAppInstaller \| Select Version` | If `1.27.350.0`, run `\| Reset-AppxPackage`. Known-broken App Installer build. |

## What you (Ryan) verified before driving over

- One-time machine setup: Windows SDK present (`signtool.exe`), Artifact Signing client tools installed (`winget install -e --id Microsoft.Azure.ArtifactSigningClientTools`), and `az login` done as `ryan@time-tracker.io` (the account with the "Artifact Signing Certificate Profile Signer" role on `ryanstewart-signing`).
- `msix\AppxManifest.xml` `<Identity Publisher>` is exactly `CN=Ryan Stewart, O=Ryan Stewart, L=Bellingham, S=Washington, C=US` — matches the Trusted Signing cert Subject (profile `public-trust`). Don't change one without the other.
- `MaxVersionTested="10.0.26100.0"` in the manifest — covers Win10 22H2 through Win11 24H2; no need to know the partner's exact build (it only affects appcompat shims, never install).
- `cargo xwin build --release --target x86_64-pc-windows-msvc --bin time-tracker` succeeded (note: on this WSL box the build needs `mt.exe` in PATH — `ln -sf /usr/bin/llvm-mt ~/.cargo/bin/mt.exe`).
- `.\msix\build-msix.ps1 -SkipSign` produced an unsigned `release\Install.msix` (the icon-gen self-heal triggers if `msix\Assets\` is empty — though it isn't, the 4 placeholder PNGs are committed).
- `.\msix\sign-trusted.ps1` signed it — `signtool verify /pa` passes, Publisher matches the manifest.
- Smoke-installed the signed `.msix` on your own machine (just double-click it): the four hotkeys (`Ctrl+Shift+H`/`;`/`'`/`/`) + tray menu + "Log a block…" + a CSV write + the Recorded Time page (`http://localhost:17893/recorded`) all work. Then `Get-AppxPackage *TimeTracker* | Remove-AppxPackage` to clean up.
- USB has exactly: `Install.msix`.

## Contact

- **Ryan**: <phone> / <email>
- **App version**: see tray menu header line.
- **Spec**: lives at `<repo>/SPEC.md` — partner doesn't need this; you might.
