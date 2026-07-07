# Manual QA for the Windows SMTC media session (docs/windows-smtc-completion-plan.md §5-6).
#
# Machine half: drives the GSMTC consumer probe (examples/smtc-probe.rs) against the
# session ytt publishes — identity (G2), single-session/no-mpv-duplicate (G3), album (G1),
# playback rate (G4), timeline/seek, OS→ytt round-trips, teardown (G6). Manual half:
# Ask-Check prompts + screenshots for the surfaces only eyes can verify (flyout name/icon,
# lock screen, media keys, Bluetooth). Evidence lands in -EvidenceDir like the tray QA.
#
# Prereqs on the box (interactive desktop session — GSMTC fails over ssh/service):
#   cargo build --release --target <target>              # ytt.exe
#   cargo build --release --example smtc-probe --target <target>
#   mpv + yt-dlp on PATH; network access for playback.

param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [string]$YttPath,
    [string]$ProbePath,
    [string]$EvidenceDir,
    [switch]$SkipDaemon,
    [switch]$KeepIdentityRegistration
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$isWindowsPlatform = [System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT
if (-not $isWindowsPlatform) {
    throw "windows-smtc-manual-qa.ps1 must run on Windows"
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $YttPath) {
    $YttPath = Join-Path $RepoRoot "target\$Target\$Profile\ytt.exe"
}
if (-not $ProbePath) {
    $ProbePath = Join-Path $RepoRoot "target\$Target\$Profile\examples\smtc-probe.exe"
}
if (-not $EvidenceDir) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $EvidenceDir = Join-Path $RepoRoot "target\windows-smtc-manual-qa-$stamp"
}

$ExpectedAumid = "io.github.ochi.yututui"
$IdentityKeyPath = "HKCU:\Software\Classes\AppUserModelId\$ExpectedAumid"
$results = [ordered]@{}
$createdYttPid = $null
$identityKeyExistedBefore = $false

function Assert-FileExists {
    param([string]$Path, [string]$Hint)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "missing file: $Path$(if ($Hint) { "`n  build it with: $Hint" })"
    }
}

function Save-Text {
    param([string]$Name, [string]$Text)
    $path = Join-Path $EvidenceDir $Name
    Set-Content -LiteralPath $path -Value $Text -Encoding UTF8
    return $path
}

function Invoke-Capture {
    param([string]$Name, [string]$File, [string[]]$Arguments)
    $output = & $File @Arguments 2>&1
    $code = $LASTEXITCODE
    $text = ($output | ForEach-Object { $_.ToString() }) -join "`n"
    Save-Text -Name "$Name.txt" -Text $text | Out-Null
    return [pscustomobject]@{ Name = $Name; ExitCode = $code; Output = $text }
}

function Assert-CaptureSuccess {
    param([object]$Capture)
    if ($Capture.ExitCode -ne 0) {
        throw "$($Capture.Name) failed with exit code $($Capture.ExitCode)"
    }
}

# Probe snapshot: parsed JSON of every GSMTC session, evidence saved per call.
$script:probeCallCount = 0
function Get-ProbeSnapshot {
    param([string]$Name)
    $script:probeCallCount++
    $capture = Invoke-Capture -Name ("probe-{0:d2}-{1}" -f $script:probeCallCount, $Name) -File $ProbePath -Arguments @("list")
    Assert-CaptureSuccess $capture
    return ($capture.Output | ConvertFrom-Json)
}

function Get-OurSession {
    param([object]$Snapshot)
    return @($Snapshot.sessions | Where-Object { $_.is_yututui }) | Select-Object -First 1
}

# Poll the probe until $Check (given the snapshot) returns truthy, or time out.
# Media metadata/artwork land asynchronously — assertions must tolerate a few seconds.
function Wait-ProbeCondition {
    param([string]$Name, [scriptblock]$Check, [int]$TimeoutSeconds = 15)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $snapshot = Get-ProbeSnapshot -Name $Name
        if (& $Check $snapshot) {
            return $snapshot
        }
        Start-Sleep -Milliseconds 800
    } while ((Get-Date) -lt $deadline)
    throw "timed out waiting for: $Name (see the probe-*.txt evidence)"
}

function Ask-Check {
    param([string]$Key, [string]$Prompt)
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
        if ($null -ne $graphics) { $graphics.Dispose() }
        if ($null -ne $bitmap) { $bitmap.Dispose() }
    }
}

New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null
Start-Transcript -LiteralPath (Join-Path $EvidenceDir "transcript.txt") | Out-Null

try {
    Assert-FileExists $YttPath "cargo build --$Profile --target $Target"
    Assert-FileExists $ProbePath "cargo build --$Profile --example smtc-probe --target $Target"

    $preexistingYtt = @(Get-Process -Name "ytt" -ErrorAction SilentlyContinue)
    if ($preexistingYtt.Count -ne 0) {
        throw "ytt.exe must not be running before this QA (pids: $(($preexistingYtt | ForEach-Object { $_.Id }) -join ', '))"
    }
    $preexistingMpv = @(Get-Process -Name "mpv" -ErrorAction SilentlyContinue)
    if ($preexistingMpv.Count -ne 0) {
        throw "mpv.exe must not be running before this QA (pids: $(($preexistingMpv | ForEach-Object { $_.Id }) -join ', '))"
    }

    $results["commit"] = (git -C $RepoRoot rev-parse HEAD 2>$null)
    $results["evidence_dir"] = $EvidenceDir
    $results["os"] = Get-ComputerInfo |
        Select-Object WindowsProductName, WindowsVersion, OsBuildNumber, OsArchitecture
    Assert-CaptureSuccess (Invoke-Capture -Name "ytt-version" -File $YttPath -Arguments @("--version"))

    # --- G3a: mpv capability probe, standalone -------------------------------------------
    $mpvVersion = Invoke-Capture -Name "mpv-version" -File "mpv" -Arguments @("--version")
    Assert-CaptureSuccess $mpvVersion
    $mpvFlag = Invoke-Capture -Name "mpv-media-controls-flag" -File "mpv" -Arguments @("--no-config", "--media-controls=no", "--version")
    $results["mpv_media_controls_flag_supported"] = ($mpvFlag.ExitCode -eq 0)
    Write-Host "mpv --media-controls=no supported: $($results['mpv_media_controls_flag_supported'])"

    # --- G2a: identity registration (idempotent; HKCU only) ------------------------------
    $identityKeyExistedBefore = Test-Path -LiteralPath $IdentityKeyPath
    $results["identity_key_existed_before"] = $identityKeyExistedBefore
    $icon = Join-Path $RepoRoot "assets\icons\yututui.ico"
    Assert-CaptureSuccess (Invoke-Capture -Name "register-media-identity" -File $YttPath -Arguments @("register-media-identity", "--icon", $icon))
    Save-Text -Name "identity-registry.txt" -Text ((Get-ItemProperty -Path $IdentityKeyPath | Out-String)) | Out-Null

    # --- baseline: no session before anything plays (EAGER=false) ------------------------
    $baseline = Get-ProbeSnapshot -Name "baseline"
    if ($null -ne (Get-OurSession $baseline)) {
        throw "a yututui GSMTC session exists before ytt was launched — stale session from a previous run?"
    }

    # --- launch + first play --------------------------------------------------------------
    Write-Host ""
    Write-Host "Launching ytt in its own console window."
    $yttProcess = Start-Process -FilePath $YttPath -PassThru
    $createdYttPid = $yttProcess.Id
    Start-Sleep -Seconds 2
    $lateBaseline = Get-ProbeSnapshot -Name "after-launch-before-play"
    $results["no_session_before_first_play"] = ($null -eq (Get-OurSession $lateBaseline))

    Read-Host "In the ytt window: search a catalog song WITH an album (not a single/radio), start it, and QUEUE a few more tracks (e.g. play an album). Then press Enter"

    $playing = Wait-ProbeCondition -Name "playing" -TimeoutSeconds 20 -Check {
        param($s)
        $mine = Get-OurSession $s
        $null -ne $mine -and $mine.status -eq "playing"
    }
    $session = Get-OurSession $playing

    # --- G2: identity; G3: exactly one session, no mpv session ---------------------------
    $results["identity_aumid"] = $session.source_app_user_model_id
    $results["identity_aumid_ok"] = ($session.source_app_user_model_id -eq $ExpectedAumid)
    $ourCount = @($playing.sessions | Where-Object { $_.is_yututui }).Count
    $mpvCount = @($playing.sessions | Where-Object { $_.source_app_user_model_id -match "mpv" }).Count
    $results["single_ytm_session"] = ($ourCount -eq 1)
    $results["no_mpv_session"] = ($mpvCount -eq 0)
    if ($mpvCount -ne 0) {
        Write-Warning "an mpv GSMTC session is present — the --media-controls=no defense failed (G3)"
    }

    # --- G1/G4 + metadata/artwork/timeline -----------------------------------------------
    $withArt = Wait-ProbeCondition -Name "metadata-and-art" -TimeoutSeconds 25 -Check {
        param($s)
        $mine = Get-OurSession $s
        $null -ne $mine -and $mine.title -and $mine.artist -and $mine.thumbnail
    }
    $session = Get-OurSession $withArt
    $results["title"] = $session.title
    $results["artist"] = $session.artist
    $results["album"] = $session.album
    $results["album_present"] = [bool]$session.album
    $results["thumbnail_present"] = [bool]$session.thumbnail
    $results["playback_rate"] = $session.rate
    $results["playback_rate_seeded"] = ($null -ne $session.rate)
    $results["shuffle_reported"] = ($null -ne $session.shuffle)
    $results["repeat_reported"] = ($null -ne $session.repeat)
    $results["timeline_end_s"] = $session.timeline.end_s
    $results["timeline_ok"] = ($session.timeline.end_s -gt 0 -and $session.timeline.max_seek_s -eq $session.timeline.end_s)
    if (-not $results["album_present"]) {
        Write-Warning "album missing — fine for singles; re-run with an album track to assert G1"
    }

    # --- OS→ytt round-trips (the same calls Phone Link / AVRCP make) ---------------------
    Assert-CaptureSuccess (Invoke-Capture -Name "cmd-pause" -File $ProbePath -Arguments @("pause"))
    Wait-ProbeCondition -Name "paused-after-cmd" -TimeoutSeconds 10 -Check {
        param($s) (Get-OurSession $s).status -eq "paused"
    } | Out-Null
    Assert-CaptureSuccess (Invoke-Capture -Name "cmd-play" -File $ProbePath -Arguments @("play"))
    Wait-ProbeCondition -Name "playing-after-cmd" -TimeoutSeconds 10 -Check {
        param($s) (Get-OurSession $s).status -eq "playing"
    } | Out-Null

    Assert-CaptureSuccess (Invoke-Capture -Name "cmd-seek" -File $ProbePath -Arguments @("seek", "30"))
    Wait-ProbeCondition -Name "position-after-seek" -TimeoutSeconds 10 -Check {
        param($s)
        $t = (Get-OurSession $s).timeline
        $t.position_s -ge 25 -and $t.position_s -le 45
    } | Out-Null
    $results["seek_roundtrip"] = $true

    $titleBefore = (Get-OurSession (Get-ProbeSnapshot -Name "before-next")).title
    Assert-CaptureSuccess (Invoke-Capture -Name "cmd-next" -File $ProbePath -Arguments @("next"))
    Wait-ProbeCondition -Name "track-changed-after-next" -TimeoutSeconds 15 -Check {
        param($s)
        $mine = Get-OurSession $s
        $null -ne $mine -and $mine.title -and $mine.title -ne $titleBefore
    } | Out-Null
    $results["next_roundtrip"] = $true

    # --- visual checklist -----------------------------------------------------------------
    Write-Host ""
    Write-Host "Visual checks. Open the media surface for this Windows version:"
    Write-Host "  Win11: volume/quick-settings media card. Win10: press a volume key for the flyout banner."
    Read-Host "Make the media surface visible, then press Enter to capture a screenshot"
    Capture-Screen -Name "flyout" | Out-Null
    Ask-Check -Key "flyout_shows_metadata_art" -Prompt "The media surface shows the correct title, artist, and album art"
    Ask-Check -Key "flyout_identity_ok" -Prompt "It shows 'YuTuTui!' + our icon (NOT 'Unknown app' / a stale icon) — the G2 decision: if NO, note it; the .lnk fallback gets adopted per the plan doc §4"
    Ask-Check -Key "flyout_buttons_work" -Prompt "Previous / play-pause / next buttons on the surface control ytt"
    Ask-Check -Key "media_keys_work" -Prompt "Keyboard media keys (and a Bluetooth headset if available) control ytt while another app has focus"
    Read-Host "Lock the machine (Win+L), check the lock-screen media card (art/controls; 24H2: progress bar), unlock, then press Enter"
    Ask-Check -Key "lock_screen_ok" -Prompt "The lock screen showed the session with working controls"
    Ask-Check -Key "coexistence_ok" -Prompt "OPTIONAL (y if skipped): with Chrome/Edge playing a video simultaneously, both sessions appear and keys follow the most recent one"

    # --- settings toggle off→on (window+session teardown/recreate; termusic risk point) ---
    Read-Host "In ytt: Settings -> Playback -> 'OS media controls' OFF, then press Enter"
    Wait-ProbeCondition -Name "gone-after-toggle-off" -TimeoutSeconds 10 -Check {
        param($s) $null -eq (Get-OurSession $s)
    } | Out-Null
    Read-Host "Toggle it back ON and make sure a track is playing (press space twice if paused), then press Enter"
    Wait-ProbeCondition -Name "back-after-toggle-on" -TimeoutSeconds 15 -Check {
        param($s)
        $mine = Get-OurSession $s
        $null -ne $mine -and $mine.status -eq "playing"
    } | Out-Null
    $results["settings_toggle_cycle"] = $true

    # --- live radio: cleared timeline, no scrubber ----------------------------------------
    Ask-Check -Key "radio_checked" -Prompt "OPTIONAL (y if skipped): play a live radio station — the surface shows station + on-air text and NO progress bar"
    Get-ProbeSnapshot -Name "radio-or-current" | Out-Null

    # --- G6: teardown on quit ---------------------------------------------------------------
    Read-Host "Quit ytt normally (q / Ctrl+C in its window), then press Enter"
    Wait-ProbeCondition -Name "gone-after-quit" -TimeoutSeconds 10 -Check {
        param($s) $null -eq (Get-OurSession $s)
    } | Out-Null
    $results["session_gone_after_quit"] = $true
    $createdYttPid = $null
    Ask-Check -Key "no_ghost_entry_after_quit" -Prompt "The media surface no longer lists ytt (no ghost 'executable name' entry)"

    # --- daemon scenario --------------------------------------------------------------------
    if (-not $SkipDaemon) {
        Write-Host ""
        Write-Host "Daemon scenario: headless core owns the SMTC session."
        Assert-CaptureSuccess (Invoke-Capture -Name "daemon-start" -File $YttPath -Arguments @("daemon", "start"))
        Start-Sleep -Seconds 2
        Invoke-Capture -Name "daemon-resume" -File $YttPath -Arguments @("-r", "resume") | Out-Null
        Read-Host "If nothing resumed, start playback via 'ytt -r play <query>' in another terminal. Once audio plays, press Enter"
        Wait-ProbeCondition -Name "daemon-session-playing" -TimeoutSeconds 20 -Check {
            param($s)
            $mine = Get-OurSession $s
            $null -ne $mine -and $mine.status -eq "playing"
        } | Out-Null
        $results["daemon_session"] = $true
        Assert-CaptureSuccess (Invoke-Capture -Name "daemon-stop" -File $YttPath -Arguments @("daemon", "stop"))
        Wait-ProbeCondition -Name "gone-after-daemon-stop" -TimeoutSeconds 10 -Check {
            param($s) $null -eq (Get-OurSession $s)
        } | Out-Null
        $results["daemon_session_gone_after_stop"] = $true
    }

    # --- diagnostic pointer (open CI bug, NOT a gate here) ----------------------------------
    Write-Host ""
    Write-Host "Reminder (separate, non-blocking): scripts/windows-daemon-smoke.ps1 hangs in CI even with"
    Write-Host "YTM_NO_MEDIA_SESSION=1 (cause OPEN — plan doc §0.2). Run it on this box in a separate"
    Write-Host "session when convenient and note where it stops; do NOT run it mid-QA (it moves profile dirs)."

    $results | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $EvidenceDir "results.json") -Encoding UTF8

    $failed = @($results.GetEnumerator() | Where-Object { $_.Value -is [bool] -and -not $_.Value })
    Write-Host ""
    if ($failed.Count -eq 0) {
        Write-Host "All recorded checks passed. Evidence: $EvidenceDir"
    } else {
        Write-Host "FAILED / declined checks:" -ForegroundColor Yellow
        $failed | ForEach-Object { Write-Host "  - $($_.Key)" -ForegroundColor Yellow }
        Write-Host "Evidence: $EvidenceDir"
    }
} finally {
    if ($createdYttPid) {
        & $YttPath -r quit 2>$null | Out-Null
        Start-Sleep -Seconds 1
        Stop-Process -Id $createdYttPid -ErrorAction SilentlyContinue
    }
    if (-not $SkipDaemon) {
        try { & $YttPath daemon stop 2>$null | Out-Null } catch { }
    }
    if (-not $identityKeyExistedBefore -and -not $KeepIdentityRegistration) {
        Remove-Item -Path $IdentityKeyPath -Recurse -Force -ErrorAction SilentlyContinue
    }
    Stop-Transcript | Out-Null
}
