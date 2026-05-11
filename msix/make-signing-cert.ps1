# Generate the self-signed code-signing cert used to sign the MSIX for the
# hand-delivered v1 install. Run this ONCE on your (Ryan's) machine.
#
# Why self-signed: Azure Trusted Signing's identity validation takes 1-20
# business days. For the Wednesday partner ship we sign with a self-signed
# cert whose public key gets trusted on the partner's machine by install.ps1.
# When Trusted Signing completes, switch build-msix.ps1 to sign with that
# instead; if the Subject DN matches, the partner auto-updates seamlessly.
#
# The cert Subject MUST exactly equal the <Identity Publisher="..."> value in
# msix\AppxManifest.xml, which is set to match the Azure Trusted Signing
# certificate Subject for "Ryan Stewart" (PublicTrust profile `public-trust` on
# the `ryanstewart-signing` account):
#   CN=Ryan Stewart, O=Ryan Stewart, L=Bellingham, S=Washington, C=US
# Wednesday's hand-delivered build is signed by a self-signed cert with this
# same Subject; when you later sign with Trusted Signing the partner machine
# sees the same publisher and auto-updates with no reinstall.
#
# Outputs (all under repo-root\release\, which is .gitignored):
#   TimeTracker.cer            public cert - goes on the USB, install.ps1 trusts it
#   TimeTracker-signing.pfx    cert + private key, password-protected - BACK THIS UP
#                          somewhere safe (NOT the repo, NOT the USB). Losing it
#                          means the partner machine will reject all future
#                          updates until they uninstall + reinstall.
#   signing-thumbprint.txt the cert thumbprint - pass to build-msix.ps1
#
# Usage:
#   .\msix\make-signing-cert.ps1
#   .\msix\make-signing-cert.ps1 -Years 3
#   .\msix\make-signing-cert.ps1 -PfxPassword (Read-Host -AsSecureString)

param(
    [string]$Subject = "CN=Ryan Stewart, O=Ryan Stewart, L=Bellingham, S=Washington, C=US",
    [int]$Years = 3,
    [securestring]$PfxPassword
)

$ErrorActionPreference = "Stop"

# Normalize a DN for comparison: lowercase attribute names, collapse whitespace,
# sort RDNs. Used so an RDN-order or spacing difference doesn't cause a false
# mismatch — but we still warn if the *raw* strings differ, because MSIX is
# happiest when cert Subject and manifest Publisher are byte-identical.
function Get-NormDn([string]$dn) {
    if (-not $dn) { return "" }
    ($dn -split '\s*,\s*' |
        ForEach-Object {
            $kv = $_ -split '\s*=\s*', 2
            "{0}={1}" -f $kv[0].Trim().ToLowerInvariant(), $kv[1].Trim()
        } | Sort-Object) -join ','
}

$RepoRoot = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$Release = Join-Path $RepoRoot "release"
New-Item -ItemType Directory -Path $Release -Force | Out-Null

$CerPath        = Join-Path $Release "TimeTracker.cer"
$PfxPath        = Join-Path $Release "TimeTracker-signing.pfx"
$ThumbprintPath = Join-Path $Release "signing-thumbprint.txt"

# Cross-check the manifest so the Subject can't silently drift from Publisher.
$ManifestPath = Join-Path $RepoRoot "msix\AppxManifest.xml"
$publisher = $null
if (Test-Path $ManifestPath) {
    [xml]$m = Get-Content $ManifestPath
    $publisher = $m.Package.Identity.Publisher
    if ($publisher -and (Get-NormDn $publisher) -ne (Get-NormDn $Subject)) {
        Write-Warning "AppxManifest Publisher is '$publisher' but -Subject is '$Subject'."
        Write-Warning "MSIX signing requires these to match. Re-run with -Subject '$publisher' or fix the manifest."
        throw "Subject / Publisher mismatch."
    }
}

# Reuse an existing cert whose Subject matches (normalized), otherwise create one.
$existing = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { (Get-NormDn $_.Subject) -eq (Get-NormDn $Subject) -and $_.NotAfter -gt (Get-Date) } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1

if ($existing) {
    Write-Host "Reusing existing cert: $($existing.Thumbprint)  (expires $($existing.NotAfter))"
    $cert = $existing
} else {
    Write-Host "Creating self-signed code-signing cert: $Subject"
    $cert = New-SelfSignedCertificate `
        -Type Custom `
        -Subject $Subject `
        -KeyUsage DigitalSignature `
        -KeyAlgorithm RSA -KeyLength 3072 `
        -KeyExportPolicy Exportable `
        -CertStoreLocation "Cert:\CurrentUser\My" `
        -NotAfter (Get-Date).AddYears($Years) `
        -FriendlyName "Time Tracker signing cert" `
        -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
    Write-Host "Created: $($cert.Thumbprint)  (expires $($cert.NotAfter))"
}

# The MSIX manifest Publisher must match the cert's ACTUAL Subject. New-Self-
# SignedCertificate occasionally reorders RDNs; verify and warn (or stop).
$actualSubject = $cert.Subject
if ($publisher) {
    if ($actualSubject -ceq $publisher) {
        Write-Host "Cert Subject matches AppxManifest Publisher exactly: $actualSubject"
    } elseif ((Get-NormDn $actualSubject) -eq (Get-NormDn $publisher)) {
        Write-Warning "Cert Subject and manifest Publisher have the same RDNs but differ in order/spacing:"
        Write-Warning "  cert:     $actualSubject"
        Write-Warning "  manifest: $publisher"
        Write-Warning "Set the manifest <Identity Publisher> to EXACTLY the cert string above, then rebuild."
    } else {
        Write-Warning "Cert Subject does NOT match manifest Publisher:"
        Write-Warning "  cert:     $actualSubject"
        Write-Warning "  manifest: $publisher"
        throw "Cert Subject / manifest Publisher mismatch — the package won't install. Fix one to match the other."
    }
}

# Public cert -> USB, install.ps1 imports this into the partner's TrustedPeople.
Export-Certificate -Cert $cert -FilePath $CerPath -Force | Out-Null
Write-Host "Wrote public cert: $CerPath"

# Private key backup. Prompt for a password if not supplied.
if (-not $PfxPassword) {
    $PfxPassword = Read-Host -Prompt "PFX backup password (you'll need this to restore the key later)" -AsSecureString
}
Export-PfxCertificate -Cert $cert -FilePath $PfxPath -Password $PfxPassword -Force | Out-Null
Write-Host "Wrote private-key backup: $PfxPath"
Write-Host "  >> MOVE THIS .pfx SOMEWHERE SAFE (password manager / encrypted backup). NOT the repo. NOT the USB."

# Thumbprint for build-msix.ps1.
Set-Content -Path $ThumbprintPath -Value $cert.Thumbprint -NoNewline
Write-Host "Wrote thumbprint: $ThumbprintPath  ($($cert.Thumbprint))"

Write-Host ""
Write-Host "Next:"
Write-Host "  1. Build + sign:   .\msix\build-msix.ps1 -Thumbprint $($cert.Thumbprint)"
Write-Host "  2. Onto the USB:   release\RyanStewart.TimeTracker.msix  +  release\TimeTracker.cer  +  msix\install.ps1"
Write-Host "  3. At the partner: run install.ps1 as administrator (see PARTNER.md)"
