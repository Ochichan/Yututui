param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [string]$YttPath,
    [string]$TrayPath,
    [string]$EvidenceDir,
    [switch]$SkipDaemon,
    [switch]$KeepTrayRunning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$isWindowsPlatform = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
if (-not $isWindowsPlatform) {
    throw "windows-tray-manual-qa.ps1 must run on Windows"
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $YttPath) {
    $YttPath = Join-Path $RepoRoot "target\$Target\$Profile\ytt.exe"
}
if (-not $TrayPath) {
    $TrayPath = Join-Path $RepoRoot "target\$Target\$Profile\ytt-desktop.exe"
}
if (-not $EvidenceDir) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $EvidenceDir = Join-Path $RepoRoot "target\windows-tray-manual-qa-$stamp"
}

$RunKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
# Must match RUN_VALUE_NAME in src/desktop/startup.rs ("YtmTui Tray"); the binary
# writes/reads/deletes the Run entry under this exact name, so a mismatch makes the
# post-install Get-ItemProperty throw and aborts the whole QA before any visual check.
$RunName = "YtmTui Tray"
$createdTrayPid = $null
$hadRunValue = $false
$oldRunValue = $null
$results = [ordered]@{}

function Assert-FileExists {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "missing file: $Path"
    }
}

function Save-Text {
    param(
        [string]$Name,
        [string]$Text
    )
    $path = Join-Path $EvidenceDir $Name
    Set-Content -LiteralPath $path -Value $Text -Encoding UTF8
    return $path
}

function Invoke-Capture {
    param(
        [string]$Name,
        [string]$File,
        [string[]]$Arguments
    )
    # ytt-desktop.exe is a GUI-subsystem binary. PowerShell's `$out = & gui.exe` neither
    # WAITS for it nor CAPTURES its AttachConsole output: the variable comes back empty and
    # the next statement runs while the child is still alive. That silently broke evidence
    # capture and — worse — raced consecutive registry writers (the --uninstall-startup /
    # --install-startup pair ran concurrently, so the delete could land after the write and
    # wipe the value the next line reads). Start-Process -Wait guarantees the child exits
    # before we return, and RedirectStandard* captures real stdout/stderr + exit code.
    $outFile = Join-Path $EvidenceDir "$Name.out.tmp"
    $errFile = Join-Path $EvidenceDir "$Name.err.tmp"
    $spArgs = @{
        FilePath               = $File
        Wait                   = $true
        NoNewWindow            = $true
        PassThru               = $true
        RedirectStandardOutput = $outFile
        RedirectStandardError  = $errFile
    }
    if ($Arguments -and $Arguments.Count -gt 0) {
        $spArgs.ArgumentList = $Arguments
    }
    $proc = Start-Process @spArgs
    $code = $proc.ExitCode
    $out = (Get-Content -LiteralPath $outFile -Raw -ErrorAction SilentlyContinue)
    $err = (Get-Content -LiteralPath $errFile -Raw -ErrorAction SilentlyContinue)
    Remove-Item -LiteralPath $outFile, $errFile -Force -ErrorAction SilentlyContinue
    $text = (@($out, $err) | Where-Object { $_ } | ForEach-Object { $_.TrimEnd("`r", "`n") }) -join "`n"
    Save-Text -Name "$Name.txt" -Text $text | Out-Null
    return [pscustomobject]@{
        Name = $Name
        ExitCode = $code
        Output = $text
    }
}

function Assert-CaptureSuccess {
    param([object]$Capture)
    if ($Capture.ExitCode -ne 0) {
        throw "$($Capture.Name) failed with exit code $($Capture.ExitCode)"
    }
}

function Save-ProcessList {
    param(
        [string]$Name,
        [string]$ProcessName
    )
    $sample = @(
        Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
            Select-Object Id, ProcessName, CPU, WorkingSet64, PrivateMemorySize64, StartTime
    )
    $text = ($sample | Format-List | Out-String)
    Save-Text -Name "$Name.txt" -Text $text | Out-Null
    return $sample
}

function Assert-NoProcessList {
    param(
        [string]$Label,
        [object[]]$Processes
    )
    if (@($Processes).Count -ne 0) {
        $ids = (@($Processes) | ForEach-Object { $_.Id }) -join ", "
        throw "$Label must not be running before this manual QA step. Stop process id(s): $ids"
    }
}

function Assert-TrayProcessRunning {
    param([string]$Label)
    if ($null -eq $createdTrayPid) {
        throw "ytt-desktop.exe was not launched before $Label"
    }
    Get-Process -Id $createdTrayPid -ErrorAction Stop | Out-Null
}

function Assert-TrayProcessStopped {
    param([string]$Label)
    if ($null -eq $createdTrayPid) {
        return
    }
    $running = Get-Process -Id $createdTrayPid -ErrorAction SilentlyContinue
    if ($null -ne $running) {
        throw "ytt-desktop.exe process id $createdTrayPid was still running after $Label"
    }
}

function Wait-TrayProcessStopped {
    param(
        [string]$Label,
        [int]$TimeoutSeconds = 5
    )
    if ($null -eq $createdTrayPid) {
        return
    }
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $running = Get-Process -Id $createdTrayPid -ErrorAction SilentlyContinue
        if ($null -eq $running) {
            return
        }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    Assert-TrayProcessStopped -Label $Label
}

function Get-PeSubsystem {
    param([string]$Path)
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
    $optionalHeaderOffset = $peOffset + 24
    return [BitConverter]::ToUInt16($bytes, $optionalHeaderOffset + 68)
}

function Ask-Check {
    param(
        [string]$Key,
        [string]$Prompt
    )
    do {
        $answer = Read-Host "$Prompt [y/n]"
    } while ($answer -notin @("y", "Y", "n", "N"))
    $results[$Key] = ($answer -in @("y", "Y"))
}

function Capture-Screen {
    param([string]$Name)
    $bitmap = $null
    $graphics = $null
    try {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
        $bitmap = [System.Drawing.Bitmap]::new($bounds.Width, $bounds.Height)
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
        $path = Join-Path $EvidenceDir "$Name.png"
        $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        return $path
    } catch {
        Save-Text -Name "$Name-screenshot-error.txt" -Text $_.Exception.Message | Out-Null
        return $null
    } finally {
        if ($null -ne $graphics) {
            $graphics.Dispose()
        }
        if ($null -ne $bitmap) {
            $bitmap.Dispose()
        }
    }
}

function Record-ProcessSample {
    param([string]$Name)
    return Save-ProcessList -Name $Name -ProcessName "ytt-desktop"
}

New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null
Start-Transcript -LiteralPath (Join-Path $EvidenceDir "transcript.txt") | Out-Null

try {
    Assert-FileExists $YttPath
    Assert-FileExists $TrayPath

    $preexistingTray = @(Save-ProcessList -Name "tray-process-before" -ProcessName "ytt-desktop")
    Assert-NoProcessList -Label "ytt-desktop.exe" -Processes $preexistingTray
    $preexistingYtt = @(Save-ProcessList -Name "ytt-process-before" -ProcessName "ytt")
    Assert-NoProcessList -Label "ytt.exe" -Processes $preexistingYtt
    $preexistingMpv = @(Save-ProcessList -Name "mpv-process-before" -ProcessName "mpv")
    Assert-NoProcessList -Label "mpv.exe" -Processes $preexistingMpv

    $results["commit"] = (git -C $RepoRoot rev-parse HEAD 2>$null)
    $results["evidence_dir"] = $EvidenceDir
    $results["ytt"] = [System.IO.Path]::GetFullPath($YttPath)
    $results["ytt_tray"] = [System.IO.Path]::GetFullPath($TrayPath)
    $results["ytt_subsystem"] = Get-PeSubsystem $YttPath
    $results["ytt_tray_subsystem"] = Get-PeSubsystem $TrayPath
    $results["os"] = Get-ComputerInfo |
        Select-Object WindowsProductName, WindowsVersion, OsBuildNumber, OsArchitecture

    if ($results["ytt_subsystem"] -ne 3) {
        throw "ytt.exe should use the Windows console subsystem"
    }
    if ($results["ytt_tray_subsystem"] -ne 2) {
        throw "ytt-desktop.exe should use the Windows GUI subsystem"
    }

    Assert-CaptureSuccess (Invoke-Capture -Name "ytt-version" -File $YttPath -Arguments @("--version"))
    Assert-CaptureSuccess (Invoke-Capture -Name "ytt-desktop-version" -File $TrayPath -Arguments @("--version"))
    Assert-CaptureSuccess (Invoke-Capture -Name "startup-status-before" -File $TrayPath -Arguments @("--startup-status"))
    Assert-CaptureSuccess (Invoke-Capture -Name "open-tui-plan" -File $TrayPath -Arguments @("--print-open-tui-plan"))

    $existingRun = Get-ItemProperty -Path $RunKey -Name $RunName -ErrorAction SilentlyContinue
    if ($null -ne $existingRun) {
        $hadRunValue = $true
        $oldRunValue = $existingRun.$RunName
    }

    Assert-CaptureSuccess (Invoke-Capture -Name "startup-uninstall-before" -File $TrayPath -Arguments @("--uninstall-startup"))
    Assert-CaptureSuccess (Invoke-Capture -Name "startup-install" -File $TrayPath -Arguments @("--install-startup"))
    $runValue = (Get-ItemProperty -Path $RunKey -Name $RunName).$RunName
    Save-Text -Name "startup-registry-installed.txt" -Text $runValue | Out-Null
    Assert-CaptureSuccess (Invoke-Capture -Name "startup-status-installed" -File $TrayPath -Arguments @("--startup-status"))
    Assert-CaptureSuccess (Invoke-Capture -Name "startup-uninstall-after" -File $TrayPath -Arguments @("--uninstall-startup"))
    $startupAfter = Invoke-Capture -Name "startup-status-after-uninstall" -File $TrayPath -Arguments @("--startup-status")
    Assert-CaptureSuccess $startupAfter
    $results["startup_roundtrip"] = $startupAfter.Output.Trim() -eq "disabled"

    Write-Host ""
    Write-Host "Launching ytt-desktop. Move the icon out of overflow if needed, then perform the visual checks."
    $trayProcess = Start-Process -FilePath $TrayPath -ArgumentList @("--background") -PassThru
    $createdTrayPid = $trayProcess.Id
    Start-Sleep -Seconds 3
    Assert-TrayProcessRunning -Label "tray launch"
    Record-ProcessSample -Name "tray-process-start" | Out-Null

    Ask-Check -Key "no_console_window" -Prompt "No console window remains open for ytt-desktop.exe"
    Ask-Check -Key "notification_icon_visible" -Prompt "Notification-area icon is visible or present in overflow"
    Ask-Check -Key "left_click_menu_opens" -Prompt "Left click opens the tray context menu and ytt-desktop.exe stays running"
    Assert-TrayProcessRunning -Label "left click tray menu"
    Ask-Check -Key "right_click_menu_opens" -Prompt "Right click opens the tray context menu and ytt-desktop.exe stays running"
    Assert-TrayProcessRunning -Label "right click tray menu"
    Read-Host "Use the tray menu to choose Show Mini Player, then press Enter"
    Assert-TrayProcessRunning -Label "Show Mini Player"
    Capture-Screen -Name "mini-player-disconnected" | Out-Null
    Ask-Check -Key "mini_player_opens" -Prompt "Show Mini Player opens a compact YtmTui Mini Player window"
    Ask-Check -Key "mini_player_disconnected_state" -Prompt "Mini Player shows disconnected or idle state with playback buttons disabled before a track is loaded"
    Ask-Check -Key "open_tui_launches_terminal" -Prompt "Open TUI launches Windows Terminal, PowerShell, or cmd"
    Ask-Check -Key "ytt_taskbar_clicks_do_not_crash" -Prompt "With the launched ytt.exe window open, left and right clicking its taskbar button does not close or crash ytt.exe"
    Ask-Check -Key "shortcut_icon_correct" -Prompt "Start Menu/Explorer shortcut displays the expected icon"
    Read-Host "Close any ytt terminal/player opened by Open TUI, then press Enter"

    $requiredScales = @("100", "150", "200")
    $results["display_scaling_values"] = $requiredScales -join ","
    Read-Host "Open the tray context menu so it is visible, then press Enter to capture a screenshot"
    Capture-Screen -Name "tray-visual-check" | Out-Null
    foreach ($scale in $requiredScales) {
        Read-Host "Set Windows display scaling to $scale percent, make the tray icon/menu visible, then press Enter to capture evidence"
        Capture-Screen -Name "tray-scale-$scale" | Out-Null
        Ask-Check -Key "tray_scale_${scale}_crisp" -Prompt "Tray icon and menu look crisp at $scale percent scaling"
    }

    Write-Host "Waiting 60 seconds to capture idle tray CPU/memory."
    Start-Sleep -Seconds 60
    Assert-TrayProcessRunning -Label "idle process sample"
    $results["tray_process_idle"] = Record-ProcessSample -Name "tray-process-idle"

    Write-Host "Restart Explorer from Task Manager now, then wait for the tray icon to reappear."
    Ask-Check -Key "explorer_restart_recovered_icon" -Prompt "After restarting Explorer, the tray icon reappeared"
    Read-Host "Make the recovered tray icon visible, then press Enter to capture a screenshot"
    Capture-Screen -Name "tray-after-explorer-restart" | Out-Null

    if (-not $SkipDaemon) {
        Invoke-Capture -Name "daemon-status-before" -File $YttPath -Arguments @("daemon", "status", "--json") | Out-Null
        $yttBeforeDaemon = @(Save-ProcessList -Name "ytt-process-before-daemon" -ProcessName "ytt")
        Assert-NoProcessList -Label "ytt.exe player" -Processes $yttBeforeDaemon
        $mpvBeforeDaemon = @(Save-ProcessList -Name "mpv-process-before-daemon" -ProcessName "mpv")
        Assert-NoProcessList -Label "mpv.exe player" -Processes $mpvBeforeDaemon
        Read-Host "Use the tray menu to choose Start Music Daemon, then press Enter"
        Assert-CaptureSuccess (Invoke-Capture -Name "daemon-status-idle" -File $YttPath -Arguments @("daemon", "status", "--json"))
        Save-Text -Name "mpv-after-idle-daemon.txt" -Text (Get-Process -Name "mpv" -ErrorAction SilentlyContinue | Format-List | Out-String) | Out-Null
        Ask-Check -Key "idle_daemon_has_no_mpv_child" -Prompt "Idle daemon reports daemon owner and has no mpv child"
        Read-Host "Use the tray menu to choose Resume Last Session, then press Enter"
        Assert-CaptureSuccess (Invoke-Capture -Name "daemon-status-resumed" -File $YttPath -Arguments @("daemon", "status", "--json"))
        Save-Text -Name "mpv-after-resume-daemon.txt" -Text (Get-Process -Name "mpv" -ErrorAction SilentlyContinue | Format-List | Out-String) | Out-Null
        Ask-Check -Key "resume_started_music_without_terminal" -Prompt "Resume Last Session started music without opening a terminal window"
        Ask-Check -Key "tray_status_updates" -Prompt "Tray title/status updated for daemon playback"
        Ask-Check -Key "playback_menu_controls_work" -Prompt "Play/Pause, Next, and Previous worked from the tray menu"
        Read-Host "Bring the Mini Player window to the front, then press Enter to capture playback state"
        Capture-Screen -Name "mini-player-playback" | Out-Null
        Ask-Check -Key "mini_player_playback_controls_work" -Prompt "Mini Player shows the current track and Play/Pause, Next, Previous, volume, and Streaming controls are enabled and work"
        Read-Host "Use the tray menu to choose Stop Music Daemon, then press Enter"
        Invoke-Capture -Name "daemon-status-after-stop" -File $YttPath -Arguments @("daemon", "status", "--json") | Out-Null
        Save-Text -Name "mpv-after-stop-daemon.txt" -Text (Get-Process -Name "mpv" -ErrorAction SilentlyContinue | Format-List | Out-String) | Out-Null
        Ask-Check -Key "stop_daemon_reaped_mpv" -Prompt "Stop Music Daemon removed the daemon descriptor and reaped mpv"
    }

    Ask-Check -Key "quit_player_keeps_tray" -Prompt "Quit Player was tested while a player was running and left ytt-desktop.exe alive"
    Assert-TrayProcessRunning -Label "Quit Player"
    Record-ProcessSample -Name "tray-process-after-quit-player" | Out-Null
    Ask-Check -Key "quit_tray_exits_only_tray" -Prompt "Quit Tray exits ytt-desktop.exe without killing unrelated processes"
    Wait-TrayProcessStopped -Label "Quit Tray"
    Save-ProcessList -Name "tray-process-after-quit-tray" -ProcessName "ytt-desktop" | Out-Null

    $logDir = Join-Path $env:LOCALAPPDATA "ytm-tui\cache"
    $appLog = Join-Path $logDir "ytm-tui.log"
    Assert-FileExists $appLog
    Copy-Item -LiteralPath $appLog -Destination (Join-Path $EvidenceDir "ytm-tui.log")
    $daemonLog = Join-Path $logDir "logs\daemon.log"
    if (-not $SkipDaemon) {
        Assert-FileExists $daemonLog
        Copy-Item -LiteralPath $daemonLog -Destination (Join-Path $EvidenceDir "daemon.log")
    } elseif (Test-Path -LiteralPath $daemonLog) {
        Copy-Item -LiteralPath $daemonLog -Destination (Join-Path $EvidenceDir "daemon.log")
    }

    $results | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $EvidenceDir "results.json") -Encoding UTF8
    Write-Host "Windows tray manual QA evidence written to $EvidenceDir"
} finally {
    try {
        if (-not $SkipDaemon) {
            & $YttPath daemon stop | Out-Null
        }
    } catch {
    }
    if ($hadRunValue) {
        New-Item -Path $RunKey -Force | Out-Null
        New-ItemProperty -Path $RunKey -Name $RunName -Value $oldRunValue -PropertyType String -Force | Out-Null
    } else {
        Remove-ItemProperty -Path $RunKey -Name $RunName -Force -ErrorAction SilentlyContinue
    }
    if ($createdTrayPid -and -not $KeepTrayRunning) {
        Stop-Process -Id $createdTrayPid -ErrorAction SilentlyContinue
    }
    Stop-Transcript | Out-Null
}
