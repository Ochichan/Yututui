param(
    [Parameter(Mandatory = $true)]
    [string]$EvidenceDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-True {
    param(
        [bool]$Condition,
        [string]$Message
    )
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-File {
    param([string]$Name)
    $path = Join-Path $EvidenceDir $Name
    Assert-True (Test-Path -LiteralPath $path -PathType Leaf) "missing evidence file: $Name"
    return $path
}

function Assert-ResultTrue {
    param(
        [object]$Results,
        [string]$Name
    )
    Assert-True ($Results.PSObject.Properties.Name -contains $Name) "missing results field: $Name"
    Assert-True ([bool]$Results.$Name) "manual QA check did not pass: $Name"
}

function Read-JsonEvidence {
    param([string]$Name)
    $path = Assert-File $Name
    return Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
}

function Read-TextEvidence {
    param([string]$Name)
    $path = Assert-File $Name
    return Get-Content -LiteralPath $path -Raw
}

$resultsPath = Assert-File "results.json"
Assert-File "transcript.txt" | Out-Null
Assert-File "tray-process-before.txt" | Out-Null
Assert-File "ytt-process-before.txt" | Out-Null
Assert-File "mpv-process-before.txt" | Out-Null
Assert-File "ytt-version.txt" | Out-Null
Assert-File "ytt-desktop-version.txt" | Out-Null
Assert-File "startup-status-before.txt" | Out-Null
Assert-File "startup-uninstall-before.txt" | Out-Null
Assert-File "startup-install.txt" | Out-Null
Assert-File "startup-registry-installed.txt" | Out-Null
Assert-File "startup-status-installed.txt" | Out-Null
Assert-File "startup-uninstall-after.txt" | Out-Null
Assert-File "startup-status-after-uninstall.txt" | Out-Null
Assert-File "open-tui-plan.txt" | Out-Null
Assert-File "tray-process-start.txt" | Out-Null
Assert-File "mini-player-disconnected.png" | Out-Null
Assert-File "tray-process-idle.txt" | Out-Null
Assert-File "tray-visual-check.png" | Out-Null
Assert-File "tray-scale-100.png" | Out-Null
Assert-File "tray-scale-150.png" | Out-Null
Assert-File "tray-scale-200.png" | Out-Null
Assert-File "tray-after-explorer-restart.png" | Out-Null
Assert-File "daemon-status-before.txt" | Out-Null
Assert-File "ytt-process-before-daemon.txt" | Out-Null
Assert-File "mpv-process-before-daemon.txt" | Out-Null
Assert-File "daemon-status-idle.txt" | Out-Null
Assert-File "daemon-status-resumed.txt" | Out-Null
Assert-File "mini-player-playback.png" | Out-Null
Assert-File "daemon-status-after-stop.txt" | Out-Null
Assert-File "mpv-after-idle-daemon.txt" | Out-Null
Assert-File "mpv-after-resume-daemon.txt" | Out-Null
Assert-File "mpv-after-stop-daemon.txt" | Out-Null
Assert-File "tray-process-after-quit-player.txt" | Out-Null
Assert-File "tray-process-after-quit-tray.txt" | Out-Null
Assert-File "ytm-tui.log" | Out-Null
Assert-File "daemon.log" | Out-Null

$results = Get-Content -LiteralPath $resultsPath -Raw | ConvertFrom-Json

Assert-True ($results.ytt_subsystem -eq 3) "ytt.exe was not recorded as a console subsystem binary"
Assert-True ($results.ytt_tray_subsystem -eq 2) "ytt-desktop.exe was not recorded as a GUI subsystem binary"
Assert-ResultTrue -Results $results -Name "startup_roundtrip"
Assert-ResultTrue -Results $results -Name "no_console_window"
Assert-ResultTrue -Results $results -Name "notification_icon_visible"
Assert-ResultTrue -Results $results -Name "left_click_menu_opens"
Assert-ResultTrue -Results $results -Name "right_click_menu_opens"
Assert-ResultTrue -Results $results -Name "mini_player_opens"
Assert-ResultTrue -Results $results -Name "mini_player_disconnected_state"
Assert-ResultTrue -Results $results -Name "open_tui_launches_terminal"
Assert-ResultTrue -Results $results -Name "ytt_taskbar_clicks_do_not_crash"
Assert-ResultTrue -Results $results -Name "shortcut_icon_correct"
Assert-ResultTrue -Results $results -Name "explorer_restart_recovered_icon"
Assert-ResultTrue -Results $results -Name "idle_daemon_has_no_mpv_child"
Assert-ResultTrue -Results $results -Name "resume_started_music_without_terminal"
Assert-ResultTrue -Results $results -Name "tray_status_updates"
Assert-ResultTrue -Results $results -Name "playback_menu_controls_work"
Assert-ResultTrue -Results $results -Name "mini_player_playback_controls_work"
Assert-ResultTrue -Results $results -Name "stop_daemon_reaped_mpv"
Assert-ResultTrue -Results $results -Name "quit_player_keeps_tray"
Assert-ResultTrue -Results $results -Name "quit_tray_exits_only_tray"
Assert-ResultTrue -Results $results -Name "tray_scale_100_crisp"
Assert-ResultTrue -Results $results -Name "tray_scale_150_crisp"
Assert-ResultTrue -Results $results -Name "tray_scale_200_crisp"

$scales = [string]$results.display_scaling_values
foreach ($scale in @("100", "150", "200")) {
    Assert-True ($scales -match "(^|[^0-9])$scale([^0-9]|$)") "display scaling $scale was not recorded"
}

$openPlan = Get-Content -LiteralPath (Join-Path $EvidenceDir "open-tui-plan.txt") -Raw
Assert-True ($openPlan.Contains("ytt.exe")) "open TUI plan did not include ytt.exe"

$trayBefore = Read-TextEvidence "tray-process-before.txt"
Assert-True (-not $trayBefore.Contains("Id")) "ytt-desktop.exe was already running before manual QA"

$yttBefore = Read-TextEvidence "ytt-process-before.txt"
Assert-True (-not $yttBefore.Contains("Id")) "ytt.exe was already running before manual QA"

$mpvBefore = Read-TextEvidence "mpv-process-before.txt"
Assert-True (-not $mpvBefore.Contains("Id")) "mpv.exe was already running before manual QA"

$startupInstalled = Get-Content -LiteralPath (Join-Path $EvidenceDir "startup-status-installed.txt") -Raw
Assert-True ($startupInstalled.Contains("--background")) "startup status did not include --background"

$startupAfter = Get-Content -LiteralPath (Join-Path $EvidenceDir "startup-status-after-uninstall.txt") -Raw
Assert-True ($startupAfter.Trim() -eq "disabled") "startup was not disabled after uninstall"

$startupRegistry = Get-Content -LiteralPath (Join-Path $EvidenceDir "startup-registry-installed.txt") -Raw
Assert-True ($startupRegistry.Contains("ytt-desktop.exe")) "startup registry value did not include ytt-desktop.exe"
Assert-True ($startupRegistry.Contains("--background")) "startup registry value did not include --background"

$startProcess = Read-TextEvidence "tray-process-start.txt"
Assert-True ($startProcess.Contains("Id")) "tray process was not recorded after launch"

$idleProcess = Get-Content -LiteralPath (Join-Path $EvidenceDir "tray-process-idle.txt") -Raw
Assert-True ($idleProcess.Contains("Id")) "tray process was not recorded while idle"
Assert-True ($idleProcess.Contains("WorkingSet64")) "idle process sample is missing WorkingSet64"
Assert-True ($idleProcess.Contains("PrivateMemorySize64")) "idle process sample is missing PrivateMemorySize64"

$yttBeforeDaemon = Read-TextEvidence "ytt-process-before-daemon.txt"
Assert-True (-not $yttBeforeDaemon.Contains("Id")) "ytt.exe player was still running before daemon QA"

$mpvBeforeDaemon = Read-TextEvidence "mpv-process-before-daemon.txt"
Assert-True (-not $mpvBeforeDaemon.Contains("Id")) "mpv.exe was still running before daemon QA"

$idleStatus = Read-JsonEvidence "daemon-status-idle.txt"
Assert-True ([bool]$idleStatus.ok) "idle daemon status was not ok"
Assert-True ($idleStatus.status.owner_mode -eq "daemon") "idle daemon status did not report daemon owner"
Assert-True ($idleStatus.status.paused -eq $true) "idle daemon status should be paused"
Assert-True ($idleStatus.status.total -eq 0) "idle daemon should have an empty queue"
Assert-True ([string]::IsNullOrEmpty([string]$idleStatus.status.title)) "idle daemon should not report a track title"

$resumedStatus = Read-JsonEvidence "daemon-status-resumed.txt"
Assert-True ([bool]$resumedStatus.ok) "resumed daemon status was not ok"
Assert-True ($resumedStatus.status.owner_mode -eq "daemon") "resumed daemon status did not report daemon owner"
Assert-True ($resumedStatus.status.paused -eq $false) "resumed daemon should be playing"
Assert-True (-not [string]::IsNullOrEmpty([string]$resumedStatus.status.title)) "resumed daemon should report a track title"
Assert-True ($resumedStatus.status.total -gt 0) "resumed daemon should have a non-empty queue"

$afterStopRaw = Get-Content -LiteralPath (Join-Path $EvidenceDir "daemon-status-after-stop.txt") -Raw
$afterStopStatus = $null
try {
    $afterStopStatus = $afterStopRaw | ConvertFrom-Json
} catch {
}
Assert-True (
    $null -eq $afterStopStatus -or
    $null -eq $afterStopStatus.status -or
    $afterStopStatus.status.owner_mode -ne "daemon"
) "daemon still reported daemon owner after Stop Music Daemon"

$mpvAfterResume = Get-Content -LiteralPath (Join-Path $EvidenceDir "mpv-after-resume-daemon.txt") -Raw
Assert-True ($mpvAfterResume.Contains("Id")) "mpv was not recorded after resume"

$mpvAfterIdle = Get-Content -LiteralPath (Join-Path $EvidenceDir "mpv-after-idle-daemon.txt") -Raw
Assert-True (-not $mpvAfterIdle.Contains("Id")) "mpv was unexpectedly recorded while the daemon was idle"

$mpvAfterStop = Get-Content -LiteralPath (Join-Path $EvidenceDir "mpv-after-stop-daemon.txt") -Raw
Assert-True (-not $mpvAfterStop.Contains("Id")) "mpv was unexpectedly recorded after daemon stop"

$trayAfterQuitPlayer = Read-TextEvidence "tray-process-after-quit-player.txt"
Assert-True ($trayAfterQuitPlayer.Contains("Id")) "ytt-desktop.exe was not alive after Quit Player"

$trayAfterQuitTray = Read-TextEvidence "tray-process-after-quit-tray.txt"
Assert-True (-not $trayAfterQuitTray.Contains("Id")) "ytt-desktop.exe was still running after Quit Tray"

Write-Host "Windows tray manual QA evidence verified: $EvidenceDir"
