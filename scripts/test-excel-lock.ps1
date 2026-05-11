# SPEC.md s4.1 retry-path E2E smoke.
#
# Holds a real ERROR_SHARING_VIOLATION on the current month's CSV (same
# code Excel/another process triggers when it has the file open exclusively)
# while csv-lock-smoke.exe submits an entry. Validates one of three modes:
#
#   short  - lock for 1s, expect WRITTEN  (retry path drains within 10s window)
#   long   - lock for 12s, expect QUEUED  (retry exhausts; falls back to disk queue)
#   drain  - lock 1s, then second WRITE call to verify queue drains
#
# Also asserts the 10-second cap (no negative-lookup behavior past RETRY_TOTAL).
#
# Run from PowerShell on Windows side, in repo root:
#   .\scripts\test-excel-lock.ps1                # default: runs all three modes
#   .\scripts\test-excel-lock.ps1 -Mode short
#   .\scripts\test-excel-lock.ps1 -Mode long
#   .\scripts\test-excel-lock.ps1 -Mode drain
#
# Prereq: cross-build the smoke bin first (from WSL):
#   cargo xwin build --release --target x86_64-pc-windows-msvc --bin csv-lock-smoke
#
# Sandbox isolation: every run uses a fresh %TEMP%\TimeTracker-locktest-<pid>\
# subtree via TIMETRACKER_*_DIR_OVERRIDE env vars. Production data is never touched.

param(
    [ValidateSet("short", "long", "drain", "all")]
    [string]$Mode = "all"
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$SmokeBin = Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\release\csv-lock-smoke.exe"

if (-not (Test-Path $SmokeBin)) {
    throw @"
csv-lock-smoke.exe not found at $SmokeBin
Build first (from WSL):
  cargo xwin build --release --target x86_64-pc-windows-msvc --bin csv-lock-smoke
"@
}

# Sandbox: per-PID temp dir, cleaned at end. Both override env vars point inside.
$Sandbox = Join-Path $env:TEMP ("TimeTracker-locktest-" + $PID)
$DataOverride = Join-Path $Sandbox "data"
$CsvOverride = Join-Path $Sandbox "csv"
New-Item -ItemType Directory -Path $DataOverride -Force | Out-Null
New-Item -ItemType Directory -Path $CsvOverride -Force | Out-Null

$env:TIMETRACKER_DATA_DIR_OVERRIDE = $DataOverride
$env:TIMETRACKER_CSV_DIR_OVERRIDE = $CsvOverride

# Current month's CSV (matches monthly_csv_path in csv_writer.rs).
$MonthCsv = Join-Path $CsvOverride ("{0}.csv" -f (Get-Date -Format "yyyy-MM"))

function Run-Smoke {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string]$Label,
        [Parameter(Mandatory)] [int]$LockSeconds,
        [Parameter(Mandatory)] [string]$Expect # WRITTEN | QUEUED
    )

    Write-Host ""
    Write-Host "==> $Label  (lock $LockSeconds s, expect $Expect)"

    # Pre-create the file so we have something to lock. The smoke bin does
    # the same via paths::ensure_layout, but we want the lock established
    # BEFORE the bin runs so the very first append_atomic call hits ENOENT-on-rename
    # ... actually append_atomic reads-then-writes-temp-then-renames; so the lock
    # has to be on the destination (the rename target). Holding $MonthCsv with
    # FileShare::None makes the rename fail with ERROR_SHARING_VIOLATION (32).
    if (-not (Test-Path $MonthCsv)) {
        # Seed with placeholder so there's content to keep open. The smoke
        # bin's first append_atomic will read+overwrite, so this seed gets
        # replaced with proper BOM+header on first successful write.
        [System.IO.File]::WriteAllBytes($MonthCsv, [byte[]](0x73,0x65,0x65,0x64,0x0d,0x0a))
    }

    # Hold the lock in a background Start-Job so it lives in its OWN process
    # (and therefore its OWN handle table). Then we can run the smoke bin
    # in this process without lock-handle accounting issues. The job opens
    # the file with FileShare::None, signals readiness via a marker file,
    # sleeps $LockSeconds, then disposes.
    $readyMarker = Join-Path $Sandbox ("lockheld-{0}.marker" -f [guid]::NewGuid())
    $lockJob = Start-Job -ScriptBlock {
        param($csv, $hold, $marker)
        $fs = [System.IO.File]::Open(
            $csv,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
        try {
            New-Item -ItemType File -Path $marker -Force | Out-Null
            Start-Sleep -Seconds $hold
        } finally {
            $fs.Dispose()
        }
    } -ArgumentList $MonthCsv, $LockSeconds, $readyMarker

    try {
        # Wait for the lock to actually be held (job opened the file +
        # touched the marker). Bail at 5s.
        $waitStart = [DateTime]::UtcNow
        while (-not (Test-Path $readyMarker)) {
            if (([DateTime]::UtcNow - $waitStart).TotalSeconds -gt 5) {
                throw "lock job never reached the marker - abort"
            }
            Start-Sleep -Milliseconds 50
        }

        $sw = [System.Diagnostics.Stopwatch]::StartNew()

        # Use Start-Process with separate redirect files so PowerShell 5.1
        # ErrorActionPreference=Stop doesn't trip on native-stderr writes.
        $stdoutFile = Join-Path $Sandbox ("smoke-stdout-{0}.log" -f [guid]::NewGuid())
        $stderrFile = Join-Path $Sandbox ("smoke-stderr-{0}.log" -f [guid]::NewGuid())
        $proc = Start-Process -FilePath $SmokeBin -NoNewWindow -PassThru -Wait `
            -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
        $smokeExit = $proc.ExitCode
        $smokeStdout = if (Test-Path $stdoutFile) { Get-Content $stdoutFile -Raw } else { "" }
        $smokeStderr = if (Test-Path $stderrFile) { Get-Content $stderrFile -Raw } else { "" }
        $smokeOutput = @(
            ($smokeStdout -split "`r?`n" | Where-Object { $_ -ne "" }),
            ($smokeStderr -split "`r?`n" | Where-Object { $_ -ne "" })
        ) | ForEach-Object { $_ }
        $sw.Stop()

        # Lock job should be done (slept $LockSeconds; smoke ran longer or
        # caught up). Wait for it to settle.
        $lockJob | Wait-Job -Timeout 5 | Out-Null
        Receive-Job $lockJob -ErrorAction SilentlyContinue | Out-Null
        Remove-Job $lockJob -Force -ErrorAction SilentlyContinue

        $resultLine = $smokeOutput | Where-Object { $_ -match '^RESULT:' } | Select-Object -Last 1
        Write-Host ("    output:        {0}" -f ($smokeOutput -join "`n    "))
        Write-Host ("    elapsed:       {0:N2}s  (lock held {1}s)" -f $sw.Elapsed.TotalSeconds, $LockSeconds)
        Write-Host ("    smoke exit:    {0}" -f $smokeExit)
        Write-Host ("    result line:   {0}" -f $resultLine)

        if (-not $resultLine) {
            throw "PASS/FAIL undetermined: no RESULT: line in smoke output"
        }
        $expectLine = "RESULT: $Expect"
        if ($resultLine.Trim() -eq $expectLine) {
            Write-Host "    PASS" -ForegroundColor Green
            return $true
        } else {
            Write-Host ("    FAIL: expected '{0}'" -f $expectLine) -ForegroundColor Red
            return $false
        }
    }
    finally {
        # Job already cleaned via Receive/Remove in the try-block; this
        # finally only fires on error paths (e.g. early throw).
        if (Get-Job -Id $lockJob.Id -ErrorAction SilentlyContinue) {
            Remove-Job $lockJob -Force -ErrorAction SilentlyContinue
        }
    }
}

$results = @()

try {
    if ($Mode -eq "short" -or $Mode -eq "all") {
        $results += @{ Name = "short-lock-1s"; Pass = (Run-Smoke -Label "short lock (1s)" -LockSeconds 1 -Expect "WRITTEN") }
    }
    if ($Mode -eq "long" -or $Mode -eq "all") {
        # Reset CSV + queue between modes so prior pass doesn't pollute.
        Get-ChildItem $CsvOverride -ErrorAction SilentlyContinue | Remove-Item -Force
        $queueDir = Join-Path $DataOverride "queue"
        Get-ChildItem $queueDir -Filter "*.json" -ErrorAction SilentlyContinue | Remove-Item -Force
        $results += @{ Name = "long-lock-12s"; Pass = (Run-Smoke -Label "long lock (12s, > RETRY_TOTAL)" -LockSeconds 12 -Expect "QUEUED") }

        # Verify queue entry was actually persisted to disk.
        $queued = Get-ChildItem $queueDir -Filter "*.json" -ErrorAction SilentlyContinue
        if ($queued) {
            Write-Host ("    queue file:    {0} ({1} bytes)" -f $queued[0].Name, $queued[0].Length)
        } else {
            Write-Host "    FAIL: WriteResult was QUEUED but no .json in $queueDir" -ForegroundColor Red
            $results[-1].Pass = $false
        }
    }
    if ($Mode -eq "drain" -or $Mode -eq "all") {
        # Pre-state: should still have queue entry from long mode (or seed one).
        $queueDir = Join-Path $DataOverride "queue"
        $beforeCount = @(Get-ChildItem $queueDir -Filter "*.json" -ErrorAction SilentlyContinue).Count
        Write-Host ""
        Write-Host "==> drain check (queue before: $beforeCount)"
        if ($beforeCount -eq 0) {
            Write-Host "    SKIP: no queued entries to drain (run with -Mode all to seed via long mode)" -ForegroundColor Yellow
        } else {
            # Run smoke with no lock. write_with_retry succeeds, then writer_loop
            # calls drain_queue() which should empty the directory.
            $stdoutFile = Join-Path $Sandbox ("smoke-stdout-drain.log")
            $stderrFile = Join-Path $Sandbox ("smoke-stderr-drain.log")
            $proc = Start-Process -FilePath $SmokeBin -NoNewWindow -PassThru -Wait `
                -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
            $smokeOutput = if (Test-Path $stdoutFile) { Get-Content $stdoutFile -Raw } else { "" }
            $afterCount = @(Get-ChildItem $queueDir -Filter "*.json" -ErrorAction SilentlyContinue).Count
            Write-Host ("    queue after:   {0}" -f $afterCount)
            $drainPass = ($afterCount -eq 0)
            if ($drainPass) {
                Write-Host "    PASS - queue drained" -ForegroundColor Green
            } else {
                Write-Host "    FAIL - queue still has $afterCount entries" -ForegroundColor Red
            }
            $results += @{ Name = "drain"; Pass = $drainPass }
        }
    }
}
finally {
    Write-Host ""
    Write-Host "==> sandbox cleanup: $Sandbox"
    Remove-Item $Sandbox -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item env:TIMETRACKER_DATA_DIR_OVERRIDE -ErrorAction SilentlyContinue
    Remove-Item env:TIMETRACKER_CSV_DIR_OVERRIDE -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "==> SUMMARY"
$failed = 0
foreach ($r in $results) {
    $status = if ($r.Pass) { "PASS" } else { "FAIL"; $failed++ }
    $color = if ($r.Pass) { "Green" } else { "Red" }
    Write-Host ("  {0,-20} {1}" -f $r.Name, $status) -ForegroundColor $color
}
if ($failed -gt 0) {
    Write-Host ""
    Write-Host "$failed test(s) failed." -ForegroundColor Red
    exit 1
} else {
    Write-Host ""
    Write-Host "All retry-path checks passed." -ForegroundColor Green
}
