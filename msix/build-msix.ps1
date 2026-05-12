# SPEC §6.1 / §7 Day-3-PM packaging script.
#
# Builds a signed .msix from the cross-compiled binary + AppxManifest +
# Assets and outputs it to release\.
#
# Prerequisites (Windows side, one-time):
#   - Windows 10 SDK installed (provides MakeAppx.exe + signtool.exe).
#     Default location: C:\Program Files (x86)\Windows Kits\10\bin\<ver>\x64\
#   - Code-signing cert installed in the user's Personal cert store.
#     The signtool /a flag picks the "best" cert; pass /sha1 <thumbprint>
#     for explicit selection.
#   - cargo-xwin built the exe at:
#       target\x86_64-pc-windows-msvc\release\time-tracker.exe
#     (Run from WSL: cargo xwin build --release \
#                    --target x86_64-pc-windows-msvc \
#                    --bin time-tracker)
#
# Usage (from PowerShell on Windows side, in repo root):
#   .\msix\build-msix.ps1                       # uses defaults
#   .\msix\build-msix.ps1 -Thumbprint AABBCC..  # pin signing cert
#   .\msix\build-msix.ps1 -SkipSign             # build unsigned for testing

param(
    [string]$Thumbprint = "",
    [string]$Version = "",
    [switch]$SkipSign
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Exe = Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\release\time-tracker.exe"
$Manifest = Join-Path $RepoRoot "msix\AppxManifest.xml"
$Assets = Join-Path $RepoRoot "msix\Assets"
$Release = Join-Path $RepoRoot "release"
$Staging = Join-Path $env:TEMP ("TimeTracker-msix-staging-" + (Get-Random))
$Output = Join-Path $Release "RyanStewart.TimeTracker.msix"

Write-Host "==> Pre-flight checks"
if (-not (Test-Path $Exe)) {
    throw "exe not found at $Exe. Build first: cargo xwin build --release --target x86_64-pc-windows-msvc --bin time-tracker"
}
# Refuse to package a binary built without --features live-view: the whole app
# (tray popover + browser surfaces) is the embedded axum server, so a build that
# doesn't contain the HTTP routes is dead on arrival. Cheap string-scan guard.
if (-not (Select-String -Path $Exe -Pattern "/api/entries" -SimpleMatch -Quiet)) {
    throw "exe at $Exe has no '/api/entries' route string - it was almost certainly built WITHOUT --features live-view. Rebuild: cargo xwin build --release --target x86_64-pc-windows-msvc --features live-view --bin time-tracker"
}
if (-not (Test-Path $Manifest)) {
    throw "manifest missing: $Manifest"
}
if (-not (Test-Path $Assets) -or @(Get-ChildItem $Assets -ErrorAction SilentlyContinue).Count -eq 0) {
    # B3: self-heal an empty Assets/ by generating placeholder icons in
    # place. Keeps the repo clean of binary placeholder PNGs while
    # ensuring a fresh checkout's first `build-msix.ps1` run succeeds.
    Write-Host "    Assets folder empty; running generate-placeholder-icons.ps1"
    $IconGen = Join-Path (Split-Path -Parent $PSCommandPath) "generate-placeholder-icons.ps1"
    if (-not (Test-Path $IconGen)) {
        throw "Assets folder empty AND generate-placeholder-icons.ps1 missing at $IconGen"
    }
    & $IconGen
    if ($LASTEXITCODE -ne 0) {
        throw "generate-placeholder-icons.ps1 exited $LASTEXITCODE"
    }
    if (@(Get-ChildItem $Assets -ErrorAction SilentlyContinue).Count -eq 0) {
        throw "Assets still empty after running generate-placeholder-icons.ps1"
    }
}

# Locate MakeAppx + signtool (try the standard SDK paths)
$MakeAppx = Get-Command MakeAppx.exe -ErrorAction SilentlyContinue
if (-not $MakeAppx) {
    $candidates = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin\*\x64\MakeAppx.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending
    if ($candidates) { $MakeAppx = $candidates[0].FullName }
}
if (-not $MakeAppx) {
    throw "MakeAppx.exe not found. Install the Windows 10 SDK."
}
Write-Host "    MakeAppx: $MakeAppx"

if (-not $SkipSign) {
    $SignTool = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if (-not $SignTool) {
        $candidates = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending
        if ($candidates) { $SignTool = $candidates[0].FullName }
    }
    if (-not $SignTool) {
        throw "signtool.exe not found. Install the Windows 10 SDK or pass -SkipSign."
    }
    Write-Host "    signtool: $SignTool"
}

Write-Host "==> Stage files at $Staging"
New-Item -ItemType Directory -Path $Staging | Out-Null
Copy-Item $Exe -Destination $Staging
Copy-Item $Manifest -Destination $Staging
New-Item -ItemType Directory -Path (Join-Path $Staging "Assets") | Out-Null
Copy-Item (Join-Path $Assets "*") -Destination (Join-Path $Staging "Assets")

# Optional version override (useful for CI / manual bumps without touching
# the manifest in git).
if ($Version) {
    $stagingManifest = Join-Path $Staging "AppxManifest.xml"
    [xml]$xml = Get-Content $stagingManifest
    $xml.Package.Identity.Version = $Version
    $xml.Save($stagingManifest)
    Write-Host "    overrode Version to $Version"
}

Write-Host "==> Pack into $Output"
New-Item -ItemType Directory -Path $Release -Force | Out-Null
& $MakeAppx pack /d $Staging /p $Output /o
if ($LASTEXITCODE -ne 0) { throw "MakeAppx failed" }

if (-not $SkipSign) {
    Write-Host "==> Sign with code-signing cert"
    $signArgs = @("sign", "/fd", "SHA256", "/tr", "http://timestamp.digicert.com", "/td", "SHA256")
    if ($Thumbprint) {
        $signArgs += @("/sha1", $Thumbprint)
    } else {
        $signArgs += @("/a")
    }
    $signArgs += $Output
    & $SignTool @signArgs
    if ($LASTEXITCODE -ne 0) { throw "signtool failed" }
}

# The partner-facing artifact is the signed .msix itself - they download a
# single file, right-click -> Install (App Installer shows "Publisher: Ryan
# Stewart" with no warning, since the chain ends at the Microsoft Identity
# Verification Root CA, already trusted on Win10 1809+/11). The .bat-based
# bundle was removed: .bat files can't be Authenticode-signed and Windows
# slapped a "Publisher cannot be verified" warning on them.
#
# install.ps1 is still copied into release\ for power-user convenience
# (-Uninstall, -Launch, dev-test from this machine), but it's NOT part of the
# Release asset - only the .msix is.
$MsixDir = Split-Path -Parent $PSCommandPath
$psSrc = Join-Path $MsixDir "install.ps1"
if (Test-Path $psSrc) { Copy-Item -Path $psSrc -Destination $Release -Force }
else { Write-Warning "expected $psSrc to exist; skipping" }

Write-Host ""
Write-Host "==> SUCCESS"
Write-Host "    Built in:  $Release"
Write-Host "      RyanStewart.TimeTracker.msix     <- the signed package (this is what ships)"
Write-Host "      install.ps1                      power-user helper (-Uninstall / -Launch)"
Write-Host ""
Write-Host "To publish:"
Write-Host "  gh release upload v<X.Y.Z> '$Release\RyanStewart.TimeTracker.msix' --clobber"
Write-Host "Partner workflow:"
Write-Host "  Download the .msix from the Release page -> right-click -> Install. No warning."
Write-Host "Dev-test on this machine:"
Write-Host "  Add-AppxPackage -Path '$Release\RyanStewart.TimeTracker.msix' -ForceApplicationShutdown"
Write-Host "  Inspect:  Get-AppxPackage RyanStewart.TimeTracker | Format-List"
