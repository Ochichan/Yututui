param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [int]$IdleSeconds = 10
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$isWindowsPlatform = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
if (-not $isWindowsPlatform) {
    throw "windows-tray-process-smoke.ps1 must run on Windows"
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$BinDir = Join-Path $RepoRoot "target\$Target\$Profile"
$Tray = Join-Path $BinDir "ytt-desktop.exe"
$Ytt = Join-Path $BinDir "ytt.exe"
$LogDir = Join-Path $env:LOCALAPPDATA "ytm-tui\cache"
$LogPattern = "ytm-tui.log*"
$createdTrayPid = $null

function Assert-FileExists {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "missing file: $Path"
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

function Get-ChildProcesses {
    param([int]$ParentPid)
    return @(
        Get-CimInstance Win32_Process -Filter "ParentProcessId = $ParentPid" -ErrorAction SilentlyContinue |
            Select-Object ProcessId, Name, CommandLine
    )
}

try {
    Assert-FileExists $Tray
    Assert-FileExists $Ytt

    $preexistingTray = @(Get-Process -Name "ytt-desktop" -ErrorAction SilentlyContinue)
    if ($preexistingTray.Count -ne 0) {
        $ids = ($preexistingTray | ForEach-Object { $_.Id }) -join ", "
        throw "ytt-desktop.exe was already running before smoke: $ids"
    }

    $preexistingMpv = @(Get-Process -Name "mpv" -ErrorAction SilentlyContinue)
    if ($preexistingMpv.Count -ne 0) {
        $ids = ($preexistingMpv | ForEach-Object { $_.Id }) -join ", "
        throw "mpv.exe was already running before smoke: $ids"
    }

    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    Get-ChildItem -LiteralPath $LogDir -Filter $LogPattern -File -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue

    $trayProcess = Start-Process -FilePath $Tray -ArgumentList @("--background") -PassThru
    $createdTrayPid = $trayProcess.Id

    Wait-Until -Label "ytt-desktop process to stay alive" -TimeoutSeconds 10 -Condition {
        $proc = Get-Process -Id $createdTrayPid -ErrorAction SilentlyContinue
        return $null -ne $proc -and -not $proc.HasExited
    }

    Wait-Until -Label "ytt-desktop log initialization" -TimeoutSeconds 10 -Condition {
        $latestLog = Get-ChildItem -LiteralPath $LogDir -Filter $LogPattern -File -ErrorAction SilentlyContinue |
            Sort-Object LastWriteTimeUtc -Descending |
            Select-Object -First 1
        if (-not $latestLog) {
            return $false
        }
        try {
            $text = Get-Content -LiteralPath $latestLog.FullName -Raw -ErrorAction Stop
        } catch {
            return $false
        }
        return $text.Contains("ytt-desktop logging initialized")
    }

    Start-Sleep -Seconds $IdleSeconds

    $trayProcess.Refresh()
    if ($trayProcess.HasExited) {
        throw "ytt-desktop.exe exited during idle smoke with code $($trayProcess.ExitCode)"
    }

    $sample = Get-Process -Id $createdTrayPid |
        Select-Object Id, ProcessName, CPU, WorkingSet64, PrivateMemorySize64, StartTime
    if ($sample.WorkingSet64 -le 0 -or $sample.PrivateMemorySize64 -le 0) {
        throw "ytt-desktop.exe memory sample was invalid: $($sample | Format-List | Out-String)"
    }

    $children = Get-ChildProcesses -ParentPid $createdTrayPid
    $unexpectedChildren = @(
        $children | Where-Object { $_.Name -in @("ytt.exe", "mpv.exe") }
    )
    if ($unexpectedChildren.Count -ne 0) {
        throw "ytt-desktop.exe spawned unexpected playback child processes: $($unexpectedChildren | Format-List | Out-String)"
    }

    $newMpv = @(Get-Process -Name "mpv" -ErrorAction SilentlyContinue)
    if ($newMpv.Count -ne 0) {
        throw "ytt-desktop.exe idle smoke started mpv unexpectedly: $($newMpv.Id -join ', ')"
    }

    Write-Host "Windows tray process smoke passed"
    $sample | Format-List | Out-String | Write-Host
} finally {
    if ($createdTrayPid) {
        Stop-Process -Id $createdTrayPid -Force -ErrorAction SilentlyContinue
        Wait-Until -Label "ytt-desktop process exit" -TimeoutSeconds 5 -Condition {
            return $null -eq (Get-Process -Id $createdTrayPid -ErrorAction SilentlyContinue)
        }
    }
}
