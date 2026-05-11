# Generate placeholder MSIX visual assets so the package can build before
# real branding lands. Solid dodger-blue (matches the tray icon's procedural
# placeholder). Uses System.Drawing - no extra deps required.
#
# Required assets per AppxManifest.xml:
#   StoreLogo.png             50x50    (Properties/Logo)
#   Square150x150Logo.png   150x150    (medium tile)
#   Square44x44Logo.png       44x44    (taskbar/Start menu)
#   Wide310x150Logo.png     310x150    (wide tile)
#
# Run from PowerShell on Windows side:
#   .\msix\generate-placeholder-icons.ps1

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$AssetsDir = Join-Path $Here "Assets"
New-Item -ItemType Directory -Path $AssetsDir -Force | Out-Null

# Dodger blue (matches the in-app tray icon placeholder #1E90FF)
$BgColor = [System.Drawing.Color]::FromArgb(255, 30, 144, 255)
$TextColor = [System.Drawing.Color]::White

$assets = @(
    @{ Name = "StoreLogo.png"; W = 50; H = 50; Label = "T" }
    @{ Name = "Square44x44Logo.png"; W = 44; H = 44; Label = "T" }
    @{ Name = "Square150x150Logo.png"; W = 150; H = 150; Label = "T" }
    @{ Name = "Wide310x150Logo.png"; W = 310; H = 150; Label = "TimeTracker" }
)

foreach ($a in $assets) {
    $bmp = New-Object System.Drawing.Bitmap $a.W, $a.H
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
    $g.Clear($BgColor)

    # Centered single-letter or short word
    $fontSize = [Math]::Min($a.W, $a.H) * 0.5
    if ($a.Label.Length -gt 1) {
        $fontSize = [Math]::Min($a.W / ($a.Label.Length * 0.7), $a.H * 0.5)
    }
    $font = New-Object System.Drawing.Font "Segoe UI", $fontSize, ([System.Drawing.FontStyle]::Bold)
    $brush = New-Object System.Drawing.SolidBrush $TextColor
    $sf = New-Object System.Drawing.StringFormat
    $sf.Alignment = [System.Drawing.StringAlignment]::Center
    $sf.LineAlignment = [System.Drawing.StringAlignment]::Center
    $rect = New-Object System.Drawing.RectangleF 0, 0, $a.W, $a.H
    $g.DrawString($a.Label, $font, $brush, $rect, $sf)

    $brush.Dispose()
    $font.Dispose()
    $g.Dispose()

    $outPath = Join-Path $AssetsDir $a.Name
    $bmp.Save($outPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "wrote $outPath ($($a.W)x$($a.H))"
}

Write-Host ""
Write-Host "Done. Assets in: $AssetsDir"
Write-Host "Replace with branded artwork later - file names + sizes must stay the same."
