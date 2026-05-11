# Time Tracker - install engine (invoked by "Install Time Tracker.bat").
#
# This is the work behind the double-click installer. End users never run this
# directly - they double-click "Install Time Tracker.bat" sitting next to it.
#
# It needs two files in the SAME folder as itself:
#   install.ps1                      this script
#   RyanStewart.TimeTracker.msix     the signed package
# ("Install Time Tracker.bat" makes it three.)
#
# No administrator rights, no certificate step: the package is signed via
# Azure Trusted Signing and its chain ends at the Microsoft Identity
# Verification Root CA - already trusted on Windows 10 1809+/11. So this is a
# plain per-user MSIX install plus a check that the WebView2 runtime (which the
# popover + Recorded Time / Export pages need) is present.
#
# Switches (for the .bat / advanced use):
#   -Launch        start the app after installing (the .bat passes this)
#   -Uninstall     remove the app (leaves your logged time + config untouched)
#   -Quiet         minimal output

[CmdletBinding()]
param(
    [string]$MsixPath,
    [switch]$Launch,
    [switch]$Uninstall,
    [switch]$Quiet
)

$ErrorActionPreference = "Stop"
$PkgFamilyHint = "RyanStewart.TimeTracker"

# Where does this script live? $PSScriptRoot is NOT populated inside a param()
# default when the script is launched via  powershell -File  (only via dot-source
# or the call operator), so resolve it here in the body, with belt-and-suspenders
# fallbacks. The .msix is expected right next to this file.
$ScriptDir = if ($PSScriptRoot) { $PSScriptRoot }
             elseif ($PSCommandPath) { Split-Path -Parent $PSCommandPath }
             else { Split-Path -Parent $MyInvocation.MyCommand.Definition }
if (-not $MsixPath) { $MsixPath = Join-Path $ScriptDir "RyanStewart.TimeTracker.msix" }

function Say([string]$m)  { if (-not $Quiet) { Write-Host $m } }
function Ok ([string]$m)  { if (-not $Quiet) { Write-Host "  $m" -ForegroundColor Green } }
function Warn([string]$m) { Write-Host "  $m" -ForegroundColor Yellow }
function Die ([string]$m) { Write-Host ""; Write-Host "  $m" -ForegroundColor Red; Write-Host ""; exit 1 }

# ---- uninstall -----------------------------------------------------------
if ($Uninstall) {
    Say ""
    Say "  Removing Time Tracker..."
    $found = Get-AppxPackage "*$PkgFamilyHint*"
    if (-not $found) { Ok "It wasn't installed. Nothing to do."; exit 0 }
    foreach ($p in $found) {
        Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match 'time-tracker' } | Stop-Process -Force -ErrorAction SilentlyContinue
        Remove-AppxPackage $p.PackageFullName -ErrorAction Stop
        Ok "removed $($p.PackageFullName)"
    }
    Say ""
    Say "  Done. Your logged time ($env:USERPROFILE\TimeTracker\) and settings"
    Say "  ($env:LOCALAPPDATA\TimeTracker\config.toml) were left in place."
    Say ""
    exit 0
}

# ---- install -------------------------------------------------------------
Say ""
Say "  Installing Time Tracker"
Say "  ----------------------"

if (-not (Test-Path -LiteralPath $MsixPath)) {
    Die @"
Couldn't find RyanStewart.TimeTracker.msix next to this installer.
Make sure "Install Time Tracker.bat", "install.ps1" and
"RyanStewart.TimeTracker.msix" are all in the same folder, then run it again.
(Looked for: $MsixPath)
"@
}

# 1) WebView2 runtime - the in-app dashboard / popover are WebView2-hosted.
#    Ships with Windows 11 and most updated Windows 10; if missing, pull the
#    tiny evergreen bootstrapper. Offline? We carry on and tell the user.
function Test-WebView2 {
    $guid = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    foreach ($root in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients',
                      'HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients',
                      'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients') {
        $pv = (Get-ItemProperty -Path (Join-Path $root $guid) -Name pv -ErrorAction SilentlyContinue).pv
        if ($pv -and $pv -ne '0.0.0.0') { return $true }
    }
    return $false
}
Say ""
Say "  [1/2] Checking the Edge WebView2 runtime..."
if (Test-WebView2) {
    Ok "present"
} else {
    Warn "not found - fetching the small installer from Microsoft..."
    $bootstrapper = Join-Path $env:TEMP "MicrosoftEdgeWebview2Setup.exe"
    try {
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $bootstrapper -UseBasicParsing -TimeoutSec 60
        Start-Process -FilePath $bootstrapper -ArgumentList '/silent', '/install' -Wait
        if (Test-WebView2) { Ok "installed" }
        else { Warn "the WebView2 installer ran but the runtime still isn't reporting in - Time Tracker will still install; if a panel shows blank, get WebView2 from https://aka.ms/webview2" }
    } catch {
        Warn "couldn't download it (offline?). Time Tracker will still install - if the"
        Warn "dashboard or popover shows a blank panel, install WebView2 from https://aka.ms/webview2"
        Warn "(it ships with Windows 11)."
    } finally {
        Remove-Item $bootstrapper -ErrorAction SilentlyContinue
    }
}

# 2) The package. Remove any prior install first (the version doesn't bump per
#    build yet, so an in-place upgrade can be refused; remove-then-add always
#    works and never touches user data).
Say ""
Say "  [2/2] Installing the app..."
Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -match 'time-tracker' } | Stop-Process -Force -ErrorAction SilentlyContinue
$prior = Get-AppxPackage "*$PkgFamilyHint*"
if ($prior) {
    Ok "updating (a version was already installed)"
    foreach ($p in $prior) { Remove-AppxPackage $p.PackageFullName -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 400
}
try {
    Add-AppxPackage -Path $MsixPath -ForceApplicationShutdown -ErrorAction Stop
} catch {
    Die @"
Windows couldn't install the package automatically.
$($_.Exception.Message)

Easiest fix: in this folder, right-click  RyanStewart.TimeTracker.msix
and choose  Install.  (Same package, same result.)
"@
}
$pkg = Get-AppxPackage "*$PkgFamilyHint*" | Select-Object -First 1
if (-not $pkg) {
    Die "The installer reported success but the app isn't registered. Try the right-click -> Install fallback on RyanStewart.TimeTracker.msix, or check the AppxDeploymentServer event log."
}
Ok "installed: $($pkg.Name) $($pkg.Version)"

# 3) Launch (the .bat passes -Launch)
if ($Launch) {
    try {
        $appId = (Get-AppxPackageManifest $pkg).Package.Applications.Application.Id
        Start-Process ("shell:AppsFolder\$($pkg.PackageFamilyName)!$appId")
        Ok "started"
    } catch {
        Warn "couldn't auto-start it - open it from the Start menu (search ""Time Tracker"")."
    }
}

Say ""
Say "  Done. Time Tracker is installed."
Say ""
Say "  - It's in your Start menu, and a small clock icon sits in the system"
Say "    tray (bottom-right of the taskbar) while it's running."
Say "  - Hotkeys, from anywhere:"
Say "        Ctrl+Shift+;     start the timer"
Say "        Ctrl+Shift+'     stop the timer (writes the entry immediately)"
Say "        Ctrl+Shift+/     open / close the tracker popover"
Say "        Ctrl+Shift+H     add a workstream"
Say "    (If a hotkey is already taken by another app, Time Tracker says so on"
Say "     launch -- change it in  %LOCALAPPDATA%\TimeTracker\config.toml  under [hotkeys].)"
Say "  - One-off blocks: right-click the tray icon -> ""Log a block...""."
Say "  - Your time-entry sheet: %USERPROFILE%\TimeTracker\<year>-<month>.csv"
Say "  - Recorded time & billing export: open it from the popover, or"
Say "    http://localhost:17893/recorded  in a browser."
Say ""
Say "  To remove later: double-click  Uninstall Time Tracker.bat  (next to this"
Say "  installer), or Settings -> Apps -> Time Tracker -> Uninstall. Either way"
Say "  your logged time and settings are kept."
Say ""
exit 0
