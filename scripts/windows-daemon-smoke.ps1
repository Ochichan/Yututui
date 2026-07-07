param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [switch]$AllowProfileMove
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$isWindowsPlatform = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
if (-not $isWindowsPlatform) {
    throw "windows-daemon-smoke.ps1 must run on Windows"
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$BinDir = Join-Path $RepoRoot "target\$Target\$Profile"
$Ytt = Join-Path $BinDir "ytt.exe"
$Tray = Join-Path $BinDir "yututray.exe"
$WorkRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("yututui-windows-smoke-" + [Guid]::NewGuid().ToString("N"))
$BackupRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("yututui-windows-smoke-backup-" + [Guid]::NewGuid().ToString("N"))
$RunKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$RunName = "YuTuTray!"

function Assert-FileExists {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "missing file: $Path"
    }
}

function Invoke-Checked {
    param(
        [string]$File,
        [string[]]$Arguments
    )
    $output = & $File @Arguments 2>&1
    $code = $LASTEXITCODE
    $text = ($output | ForEach-Object { $_.ToString() }) -join "`n"
    if ($code -ne 0) {
        throw "$File $($Arguments -join ' ') failed with exit code ${code}: $text"
    }
    return $text
}

function Invoke-CaptureProcess {
    param(
        [string]$File,
        [string[]]$Arguments
    )
    $stdout = Join-Path $WorkRoot ("stdout-" + [Guid]::NewGuid().ToString("N") + ".txt")
    $stderr = Join-Path $WorkRoot ("stderr-" + [Guid]::NewGuid().ToString("N") + ".txt")
    $proc = Start-Process `
        -FilePath $File `
        -ArgumentList $Arguments `
        -Wait `
        -PassThru `
        -WindowStyle Hidden `
        -RedirectStandardOutput $stdout `
        -RedirectStandardError $stderr
    $outText = if (Test-Path -LiteralPath $stdout) { Get-Content -LiteralPath $stdout -Raw } else { "" }
    $errText = if (Test-Path -LiteralPath $stderr) { Get-Content -LiteralPath $stderr -Raw } else { "" }
    if ($proc.ExitCode -ne 0) {
        throw "$File $($Arguments -join ' ') failed with exit code $($proc.ExitCode): $errText$outText"
    }
    return [pscustomobject]@{
        Stdout = $outText
        Stderr = $errText
    }
}

function Wait-Until {
    param(
        [scriptblock]$Condition,
        [string]$Label,
        [int]$TimeoutSeconds = 10
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if (& $Condition) {
            return
        }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    throw "timed out waiting for $Label"
}

function Get-DaemonStatus {
    $json = Invoke-Checked $Ytt @("daemon", "status", "--json")
    return $json | ConvertFrom-Json
}

function Get-NewMpvProcesses {
    param([int[]]$Baseline)
    # NOTE: callers must wrap this in @(..) — PowerShell unrolls a returned array,
    # so an empty result arrives as $null and `.Count` throws under StrictMode.
    $all = @(Get-Process -Name "mpv" -ErrorAction SilentlyContinue)
    return @($all | Where-Object { $_.Id -notin $Baseline })
}

function New-SilentWav {
    param(
        [string]$Path,
        [int]$Seconds = 20
    )
    $sampleRate = 44100
    $channels = 1
    $bitsPerSample = 16
    $blockAlign = [uint16](($channels * $bitsPerSample) / 8)
    $byteRate = [uint32]($sampleRate * $blockAlign)
    $dataBytes = [uint32]($sampleRate * $Seconds * $blockAlign)
    $riffBytes = [uint32](36 + $dataBytes)

    $fs = [System.IO.File]::Create($Path)
    try {
        $bw = [System.IO.BinaryWriter]::new($fs)
        $ascii = [System.Text.Encoding]::ASCII
        $bw.Write($ascii.GetBytes("RIFF"))
        $bw.Write($riffBytes)
        $bw.Write($ascii.GetBytes("WAVE"))
        $bw.Write($ascii.GetBytes("fmt "))
        $bw.Write([uint32]16)
        $bw.Write([uint16]1)
        $bw.Write([uint16]$channels)
        $bw.Write([uint32]$sampleRate)
        $bw.Write($byteRate)
        $bw.Write($blockAlign)
        $bw.Write([uint16]$bitsPerSample)
        $bw.Write($ascii.GetBytes("data"))
        $bw.Write($dataBytes)
        for ($i = 0; $i -lt ($dataBytes / 2); $i++) {
            $bw.Write([int16]0)
        }
    } finally {
        $fs.Dispose()
    }
}

function Move-ProfileDir {
    param(
        [string]$Path,
        [string]$Name
    )
    if (-not (Test-Path -LiteralPath $Path)) {
        return $null
    }
    $mayMove = $AllowProfileMove -or ($env:GITHUB_ACTIONS -eq "true")
    if (-not $mayMove) {
        throw "refusing to move existing profile dir without -AllowProfileMove: $Path"
    }
    New-Item -ItemType Directory -Force -Path $BackupRoot | Out-Null
    $backup = Join-Path $BackupRoot $Name
    Move-Item -LiteralPath $Path -Destination $backup
    return [pscustomobject]@{
        Original = $Path
        Backup = $backup
    }
}

function Restore-ProfileDir {
    param([object[]]$Backups)
    foreach ($item in $Backups) {
        if ($null -eq $item) {
            continue
        }
        if (Test-Path -LiteralPath $item.Original) {
            Remove-Item -LiteralPath $item.Original -Recurse -Force
        }
        Move-Item -LiteralPath $item.Backup -Destination $item.Original
    }
}

function Remove-TestProfileDirs {
    param([object[]]$Backups)
    $restoredPaths = @(
        $Backups |
            Where-Object { $null -ne $_ } |
            ForEach-Object { $_.Original }
    )
    foreach ($path in @($roamingRoot, $localRoot)) {
        if ($path -notin $restoredPaths -and (Test-Path -LiteralPath $path)) {
            Remove-Item -LiteralPath $path -Recurse -Force
        }
    }
}

function Clear-RuntimeDescriptor {
    $raw = if ($env:USERNAME) { $env:USERNAME } else { "default" }
    $tag = (($raw.ToCharArray() | Where-Object { $_ -match "[A-Za-z0-9]" }) -join "")
    if ($tag.Length -gt 16) {
        $tag = $tag.Substring(0, 16)
    }
    if ($tag.Length -eq 0) {
        $tag = "default"
    }
    $runtimeDir = Join-Path ([System.IO.Path]::GetTempPath()) "yututui-$tag"
    Remove-Item -LiteralPath (Join-Path $runtimeDir "yututui-remote-$tag.json") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path ([System.IO.Path]::GetTempPath()) "yututui-remote-$tag.json") -Force -ErrorAction SilentlyContinue
}

Assert-FileExists $Ytt
Assert-FileExists $Tray
if (-not (Get-Command "mpv" -ErrorAction SilentlyContinue)) {
    throw "mpv is required for the daemon smoke test"
}

# The daemon publishes SMTC (System Media Transport Controls), which needs a real top-level
# window + message pump on an interactive session (see src/media/smtc.rs). The CI runner's
# non-interactive, DETACHED_PROCESS daemon has none, so SMTC init wedges the daemon's event
# loop on the first playing snapshot. Run the daemon headless: this whitelisted env var
# reaches the spawned child (util/process.rs) and disables the OS media session — the same
# escape hatch the macOS/unix smoke uses.
$env:YTM_NO_MEDIA_SESSION = "1"

New-Item -ItemType Directory -Force -Path $WorkRoot | Out-Null

$roamingRoot = Join-Path $env:APPDATA "yututui"
$localRoot = Join-Path $env:LOCALAPPDATA "yututui"
$dataDir = Join-Path $roamingRoot "data"
$configDir = Join-Path $roamingRoot "config"
$cacheDir = Join-Path $localRoot "cache"
$profileBackups = @()
$hadRunValue = $false
$oldRunValue = $null
$oldMpvExtra = [Environment]::GetEnvironmentVariable("YTM_MPV_EXTRA", "Process")
$mpvBaseline = @(Get-Process -Name "mpv" -ErrorAction SilentlyContinue | ForEach-Object { $_.Id })

try {
    $existingRun = Get-ItemProperty -Path $RunKey -Name $RunName -ErrorAction SilentlyContinue
    if ($null -ne $existingRun) {
        $hadRunValue = $true
        $oldRunValue = $existingRun.$RunName
    }

    $profileBackups += Move-ProfileDir -Path $roamingRoot -Name "roaming"
    $profileBackups += Move-ProfileDir -Path $localRoot -Name "local"
    Clear-RuntimeDescriptor

    New-Item -ItemType Directory -Force -Path $dataDir, $configDir, $cacheDir | Out-Null

    $startup = Invoke-CaptureProcess $Tray @("--startup-status")
    if ($startup.Stdout.Trim() -notin @("disabled", "unsupported") -and -not $startup.Stdout.Contains("enabled:")) {
        throw "unexpected startup status output: $($startup.Stdout)"
    }

    Invoke-CaptureProcess $Tray @("--uninstall-startup") | Out-Null
    Invoke-CaptureProcess $Tray @("--install-startup") | Out-Null
    $runValue = (Get-ItemProperty -Path $RunKey -Name $RunName).$RunName
    $trayFull = [System.IO.Path]::GetFullPath($Tray)
    $expectedRun = '"' + $trayFull + '" --background'
    if (-not [string]::Equals($runValue, $expectedRun, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "unexpected startup command: $runValue"
    }
    $startup = Invoke-CaptureProcess $Tray @("--startup-status")
    if (-not $startup.Stdout.Contains("--background")) {
        throw "startup status did not report the background command: $($startup.Stdout)"
    }
    Invoke-CaptureProcess $Tray @("--uninstall-startup") | Out-Null
    $startup = Invoke-CaptureProcess $Tray @("--startup-status")
    if ($startup.Stdout.Trim() -ne "disabled") {
        throw "startup status should be disabled after uninstall: $($startup.Stdout)"
    }

    $openPlan = Invoke-CaptureProcess $Tray @("--print-open-tui-plan")
    if (-not $openPlan.Stdout.Contains("wt.exe") -or -not $openPlan.Stdout.Contains("ytt.exe")) {
        throw "open TUI launch plan did not include Windows terminal candidates: $($openPlan.Stdout)"
    }

    $wavOne = Join-Path $WorkRoot "windows-smoke-one.wav"
    $wavTwo = Join-Path $WorkRoot "windows-smoke-two.wav"
    New-SilentWav -Path $wavOne
    New-SilentWav -Path $wavTwo

    @{
        volume = 0
        gapless = $false
        speed = 1.0
        seek_seconds = 5
        repeat = "off"
        # This smoke uses local WAV fixtures only; avoid managed yt-dlp downloads racing
        # Windows executable locks during temp profile cleanup.
        tools = @{
            ytdlp_managed = $false
        }
    } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $configDir "config.json") -Encoding UTF8

    @{
        last_mode = "normal"
    } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $cacheDir "session.json") -Encoding UTF8

    @{
        favorites = @()
        history = @(
            @{
                video_id = "local:windows-smoke-one"
                title = "Windows Smoke One"
                artist = "yututui"
                duration = "0:20"
                local_path = $wavOne
            },
            @{
                video_id = "local:windows-smoke-two"
                title = "Windows Smoke Two"
                artist = "yututui"
                duration = "0:20"
                local_path = $wavTwo
            }
        )
        radio_favorites = @()
        radios = @()
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $dataDir "library.json") -Encoding UTF8

    [Environment]::SetEnvironmentVariable("YTM_MPV_EXTRA", "--ao=null --volume=0", "Process")

    Invoke-Checked $Ytt @("daemon", "start") | Out-Null
    Wait-Until -Label "idle daemon status" -Condition {
        try {
            $status = Get-DaemonStatus
            return $status.ok -and $status.status.owner_mode -eq "daemon" -and $null -eq $status.status.title -and $status.status.paused
        } catch {
            return $false
        }
    }
    $idleMpv = @(Get-NewMpvProcesses -Baseline $mpvBaseline)
    if ($idleMpv.Count -ne 0) {
        throw "idle daemon spawned mpv unexpectedly: $($idleMpv.Id -join ', ')"
    }

    Invoke-Checked $Ytt @("daemon", "start", "--resume") | Out-Null
    Wait-Until -Label "resumed daemon playback" -Condition {
        try {
            $status = Get-DaemonStatus
            return $status.ok -and $status.status.owner_mode -eq "daemon" -and $status.status.title -eq "Windows Smoke One" -and -not $status.status.paused
        } catch {
            return $false
        }
    }
    Wait-Until -Label "mpv child after resume" -Condition {
        $newMpv = @(Get-NewMpvProcesses -Baseline $mpvBaseline)
        return $newMpv.Count -ge 1
    }

    Invoke-Checked $Ytt @("-r", "pp") | Out-Null
    Wait-Until -Label "remote pause" -Condition {
        try {
            $status = Get-DaemonStatus
            return $status.status.title -eq "Windows Smoke One" -and $status.status.paused
        } catch {
            return $false
        }
    }
    Invoke-Checked $Ytt @("-r", "pp") | Out-Null
    Wait-Until -Label "remote resume" -Condition {
        try {
            $status = Get-DaemonStatus
            return $status.status.title -eq "Windows Smoke One" -and -not $status.status.paused
        } catch {
            return $false
        }
    }
    Invoke-Checked $Ytt @("-r", "next") | Out-Null
    Wait-Until -Label "remote next" -Condition {
        try {
            $status = Get-DaemonStatus
            return $status.status.title -eq "Windows Smoke Two"
        } catch {
            return $false
        }
    }
    Invoke-Checked $Ytt @("-r", "prev") | Out-Null
    Wait-Until -Label "remote previous" -Condition {
        try {
            $status = Get-DaemonStatus
            return $status.status.title -eq "Windows Smoke One"
        } catch {
            return $false
        }
    }

    Invoke-Checked $Ytt @("daemon", "stop") | Out-Null
    Wait-Until -Label "daemon stop" -Condition {
        try {
            Get-DaemonStatus | Out-Null
            return $false
        } catch {
            return $true
        }
    }
    Wait-Until -Label "mpv cleanup" -Condition {
        $newMpv = @(Get-NewMpvProcesses -Baseline $mpvBaseline)
        return $newMpv.Count -eq 0
    }

    $daemonLogDir = Join-Path $cacheDir "logs"
    $daemonLog = Get-ChildItem -LiteralPath $daemonLogDir -Filter "daemon.log*" -File -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $daemonLog) {
        throw "daemon log was not created under $daemonLogDir"
    }

    Write-Host "Windows daemon/tray smoke passed"
} finally {
    try {
        Invoke-Checked $Ytt @("daemon", "stop") | Out-Null
    } catch {
    }
    [Environment]::SetEnvironmentVariable("YTM_MPV_EXTRA", $oldMpvExtra, "Process")
    if ($hadRunValue) {
        New-Item -Path $RunKey -Force | Out-Null
        New-ItemProperty -Path $RunKey -Name $RunName -Value $oldRunValue -PropertyType String -Force | Out-Null
    } else {
        Remove-ItemProperty -Path $RunKey -Name $RunName -Force -ErrorAction SilentlyContinue
    }
    Remove-TestProfileDirs -Backups $profileBackups
    Restore-ProfileDir -Backups $profileBackups
    Clear-RuntimeDescriptor
    Remove-Item -LiteralPath $WorkRoot -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $BackupRoot -Recurse -Force -ErrorAction SilentlyContinue
}

# The cleanup block intentionally probes `ytt daemon stop` after the smoke already stopped
# the daemon. GitHub Actions' pwsh wrapper can otherwise propagate that native
# `$LASTEXITCODE` even though the expected cleanup failure was caught above.
$global:LASTEXITCODE = 0
