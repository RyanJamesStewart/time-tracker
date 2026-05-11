# Sign the MSIX with Azure Trusted Signing (now "Azure Artifact Signing").
#
# Run this on your (Ryan's) Windows machine, AFTER building an unsigned package:
#   .\msix\build-msix.ps1 -SkipSign            # produces release\RyanStewart.TimeTracker.msix (unsigned)
#   .\msix\sign-trusted.ps1                     # signs it in place
#
# One-time prerequisites on this machine:
#   1. Windows 10/11 SDK (provides signtool.exe)  - you already have it.
#   2. The Artifact Signing client tools (provides the dlib + bundled .NET):
#         winget install -e --id Microsoft.Azure.ArtifactSigningClientTools
#      Installs PER-USER to:
#         %LOCALAPPDATA%\Microsoft\MicrosoftArtifactSigningClientTools\Azure.CodeSigning.Dlib.dll
#      (sign-trusted.ps1 auto-discovers it; if not, pass -DlibPath explicitly.
#       Older/alt installs may put it under Program Files\Microsoft\ArtifactSigningClientTools\bin\,
#       or via NuGet: `.\nuget.exe install Microsoft.ArtifactSigning.Client -x`.)
#   3. Be signed in to Azure CLI as the account that has the
#      "Artifact Signing Certificate Profile Signer" role on the `ryanstewart-signing`
#      account (that's ryan@databa.io):
#         az login
#
# What gets signed-with: account `ryanstewart-signing`, profile `public-trust`,
# endpoint https://eus.codesigning.azure.net/ - see trusted-signing-metadata.json.
# Trusted Signing issues a fresh ~3-day cert per signing; the /tr timestamp keeps
# the signature valid forever after that. The cert chain is the Microsoft Identity
# Verification Root CA, which is pre-trusted on Win10 1809+/Win11 - so the partner
# machine installs the .msix with no cert-import step.

param(
    [string]$MsixPath  = (Join-Path (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)) "release\RyanStewart.TimeTracker.msix"),
    [string]$Metadata  = (Join-Path $PSScriptRoot "trusted-signing-metadata.json"),
    [string]$DlibPath  = "",                       # auto-discovered if not given
    [string]$TimestampUrl = "http://timestamp.acs.microsoft.com",
    [switch]$Debug
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $MsixPath))  { throw "Package not found: $MsixPath  (build it first: .\msix\build-msix.ps1 -SkipSign)" }
if (-not (Test-Path $Metadata))  { throw "Metadata file not found: $Metadata" }

function Find-Dlib {
    param([string]$Explicit)

    # 1. explicit path wins
    if ($Explicit) {
        if (Test-Path $Explicit) { return (Resolve-Path $Explicit).Path }
        throw "Specified -DlibPath does not exist: $Explicit"
    }

    # 2. known fixed install locations.
    #    The current "ArtifactSigning Client Tools" MSI (winget id
    #    Microsoft.Azure.ArtifactSigningClientTools) installs PER-USER to
    #    %LOCALAPPDATA%\Microsoft\MicrosoftArtifactSigningClientTools\ -
    #    that's the one that actually matters now. The Program Files / older
    #    "Trusted Signing Client" / bin\x64 paths are kept for older installs.
    $known = @(
        "$env:LOCALAPPDATA\Microsoft\MicrosoftArtifactSigningClientTools\Azure.CodeSigning.Dlib.dll"
        "$env:ProgramFiles\Microsoft\ArtifactSigningClientTools\bin\x64\Azure.CodeSigning.Dlib.dll"
        "$env:ProgramFiles\Microsoft\ArtifactSigningClientTools\bin\Azure.CodeSigning.Dlib.dll"
        "${env:ProgramFiles(x86)}\Microsoft\ArtifactSigningClientTools\bin\x64\Azure.CodeSigning.Dlib.dll"
        "${env:ProgramFiles(x86)}\Microsoft\ArtifactSigningClientTools\bin\Azure.CodeSigning.Dlib.dll"
        "$env:ProgramFiles\Microsoft\Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll"
        "${env:ProgramFiles(x86)}\Microsoft\Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll"
    )
    foreach ($p in $known) { if ($p -and (Test-Path $p)) { return (Resolve-Path $p).Path } }

    # 3. NuGet package cache (Microsoft.ArtifactSigning.Client / Microsoft.Trusted.Signing.Client)
    $nugetRoots = @(
        "$env:USERPROFILE\.nuget\packages\microsoft.artifactsigning.client"
        "$env:USERPROFILE\.nuget\packages\microsoft.trusted.signing.client"
        (Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) "packages")  # repo-local nuget -OutputDirectory
    )
    foreach ($root in $nugetRoots) {
        if ($root -and (Test-Path $root)) {
            $hit = Get-ChildItem -Path $root -Recurse -Filter "Azure.CodeSigning.Dlib.dll" -File -ErrorAction SilentlyContinue |
                Sort-Object { try { [version]$_.VersionInfo.FileVersion } catch { [version]"0.0" } } -Descending |
                Select-Object -First 1
            if ($hit) { return $hit.FullName }
        }
    }

    # 4. last resort: recursive scan of the likely roots
    Write-Host "Scanning for the Artifact Signing dlib (one-time)..."
    foreach ($root in @("$env:LOCALAPPDATA\Microsoft", $env:LOCALAPPDATA, $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData)) {
        if ($root -and (Test-Path $root)) {
            $hit = Get-ChildItem -Path $root -Recurse -Filter "Azure.CodeSigning.Dlib.dll" -File -ErrorAction SilentlyContinue |
                Sort-Object { try { [version]$_.VersionInfo.FileVersion } catch { [version]"0.0" } } -Descending |
                Select-Object -First 1
            if ($hit) { return $hit.FullName }
        }
    }

    throw @"
Could not find Azure.CodeSigning.Dlib.dll anywhere.
Install the Artifact Signing client tools:
  winget install -e --id Microsoft.Azure.ArtifactSigningClientTools
or fetch the NuGet package:
  .\nuget.exe install Microsoft.ArtifactSigning.Client -x -OutputDirectory .\packages
then re-run. (You can also pass -DlibPath C:\path\to\Azure.CodeSigning.Dlib.dll explicitly.)
"@
}

$DlibPath = Find-Dlib -Explicit $DlibPath
Write-Host "dlib: $DlibPath"

# Locate signtool.exe (Windows SDK).
$SignTool = Get-Command signtool.exe -ErrorAction SilentlyContinue
if (-not $SignTool) {
    $cand = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if ($cand) { $SignTool = $cand.FullName } else { throw "signtool.exe not found. Install the Windows 10/11 SDK." }
} else { $SignTool = $SignTool.Source }
Write-Host "signtool: $SignTool"

# Sanity-check Azure auth (the dlib uses DefaultAzureCredential, which picks up `az login`).
$acct = az account show --query "user.name" -o tsv 2>$null
if (-not $acct) {
    Write-Warning "Not signed in to Azure CLI. Run 'az login' as the account with the Certificate Profile Signer role (ryan@databa.io), then re-run."
    throw "az login required."
}
Write-Host "Azure CLI account: $acct"

Write-Host "==> Signing $MsixPath with Trusted Signing (account=ryanstewart-signing, profile=public-trust)"
$dbg = @(); if ($Debug) { $dbg = @("/debug") }
& $SignTool sign /v @dbg /fd SHA256 `
    /tr $TimestampUrl /td SHA256 `
    /dlib $DlibPath `
    /dmdf $Metadata `
    $MsixPath
if ($LASTEXITCODE -ne 0) {
    throw "signtool exited $LASTEXITCODE. Re-run with -Debug for the cert-chain / auth detail. A 403 usually means a region/endpoint mismatch in metadata.json, or the signed-in account lacks the 'Artifact Signing Certificate Profile Signer' role."
}

Write-Host "==> Verifying signature"
& $SignTool verify /pa /v $MsixPath
if ($LASTEXITCODE -ne 0) { throw "signtool verify failed ($LASTEXITCODE)." }

Write-Host ""
Write-Host "Signed: $MsixPath"
Write-Host "Publisher should match the AppxManifest <Identity Publisher>:"
Write-Host "  CN=Ryan Stewart, O=Ryan Stewart, L=Bellingham, S=Washington, C=US"
Write-Host ""
Write-Host "Onto the USB:  release\RyanStewart.TimeTracker.msix  (that's it - no .cer, no install.ps1 needed)"
Write-Host "At the partner: double-click the .msix -> App Installer -> Install. No admin, no cert step."
Write-Host "  (If Windows shows a brief SmartScreen prompt: More info -> Run anyway. That reputation"
Write-Host "   clears as installs accumulate; the cert chain itself is trusted from day one.)"
