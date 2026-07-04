# ytm-tui installer (Windows) — makes `ytt` runnable from any terminal, no manual setup.
# Release/source installs also place the `ytt-desktop` tray companion beside it.
#
#   irm https://raw.githubusercontent.com/Ochichan/ytm-tui/main/install.ps1 | iex
#                                                            # download a prebuilt — no clone needed
#   powershell -ExecutionPolicy Bypass -File .\install.ps1   # from a clone: dist\ or a release zip
#   powershell -ExecutionPolicy Bypass -File .\install.ps1 --build   # force a source build (needs Rust)
#
# Pin a version with  $env:YTT_VERSION = 'v1.5.8'  (default: the latest release).
# Adds the install dir to your user PATH and checks for the mpv / yt-dlp / ffmpeg runtime tools.
# (On Windows the one-command path is Scoop:  scoop install ytm-tui.)

$ErrorActionPreference = 'Stop'
$Bin = 'ytt'
$DesktopBin = 'ytt-desktop'
$RepoSlug = 'Ochichan/ytm-tui'

# $PSScriptRoot is reliably set for script-file execution; fall back to the CWD when the script
# is run via -Command / Invoke-Expression (where $MyInvocation.MyCommand.Path is $null).
$ScriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
Set-Location $ScriptDir

function Info($m) { Write-Host "==> $m"   -ForegroundColor Cyan }
function Ok($m)   { Write-Host "OK  $m"    -ForegroundColor Green }
function Warn($m) { Write-Host "warn: $m"  -ForegroundColor Yellow }
function Die($m)  { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

$ForceBuild = $args -contains '--build'
$Prebuilt   = Join-Path $ScriptDir 'dist\ytt.exe'
$script:TrayInstalled = $false

# These drive the install directory below; on Server Core / some CI / redirected-profile
# environments they can be unset, which would silently produce a bogus root-relative path.
if (-not $env:LOCALAPPDATA) { Die '$env:LOCALAPPDATA is not set; cannot determine install directory.' }
if (-not $env:USERPROFILE)  { Die '$env:USERPROFILE is not set.' }

# Copy an existing ytt.exe into the per-user programs dir; sets $script:InstallDir.
function Install-File($srcExe) {
    $script:InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\ytt'
    New-Item -ItemType Directory -Force -Path $script:InstallDir | Out-Null
    $dest = Join-Path $script:InstallDir "$Bin.exe"
    Copy-Item $srcExe $dest -Force
    # The media-flyout icon rides along when the source ships it (release zips carry
    # ytm-tui.ico at the root; older archives simply don't have it).
    $srcIcon = Join-Path (Split-Path $srcExe -Parent) 'ytm-tui.ico'
    if (Test-Path $srcIcon) {
        Copy-Item $srcIcon (Join-Path $script:InstallDir 'ytm-tui.ico') -Force
    }
    Ok "Installed -> $dest"

    $srcTray = Join-Path (Split-Path $srcExe -Parent) "$DesktopBin.exe"
    if (Test-Path $srcTray) {
        $trayDest = Join-Path $script:InstallDir "$DesktopBin.exe"
        Copy-Item $srcTray $trayDest -Force
        $script:TrayInstalled = $true
        Ok "Installed tray companion -> $trayDest"
    }
}

# Download the prebuilt zip from GitHub Releases, verify its SHA-256 against checksums.txt,
# extract ytt.exe, and install it. Only an x64 build ships (it also runs on ARM64 via emulation).
function Install-Download {
    $archive = 'ytm-tui-windows-x64.zip'
    $ver  = if ($env:YTT_VERSION) { $env:YTT_VERSION } else { 'latest' }
    $base = "https://github.com/$RepoSlug/releases"
    if ($ver -eq 'latest') {
        $url = "$base/latest/download/$archive"; $cksUrl = "$base/latest/download/checksums.txt"
    } else {
        $url = "$base/download/$ver/$archive";    $cksUrl = "$base/download/$ver/checksums.txt"
    }
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ytt-" + [System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $zip = Join-Path $tmp $archive

    try {
        Info "Downloading $archive ($ver)..."
        try { Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing } catch { Die "download failed: $url" }

        $cks = Join-Path $tmp 'checksums.txt'
        try { Invoke-WebRequest -Uri $cksUrl -OutFile $cks -UseBasicParsing } catch { Die "release has no checksums.txt - aborting" }
        $match = Select-String -Path $cks -Pattern ([regex]::Escape($archive)) | Select-Object -First 1
        if ($match) {
            $want = (($match.Line -split '\s+')[0]).ToLower()
            $got  = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
            if ($want -ne $got) { Die "checksum mismatch for $archive - aborting" }
            Ok "Checksum verified"
        } else { Die "no checksum entry for $archive - aborting" }

        Info "Extracting..."
        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        $exe = Join-Path $tmp "$Bin.exe"
        if (-not (Test-Path $exe)) { Die "archive did not contain $Bin.exe" }
        Install-File $exe
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

# Build from a checkout with cargo; sets $script:InstallDir.
function Install-ViaCargo {
    if (-not (Test-Path (Join-Path $ScriptDir 'Cargo.toml'))) {
        Die "Not a ytm-tui checkout, so there's nothing to build.`n  Use Scoop ( scoop install ytm-tui ), or clone the repo and re-run."
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Die "Rust (cargo) is not installed or not on PATH - required for --build.`n  Install Rust: https://rustup.rs  then re-run."
    }
    Info "Building from source with cargo - this can take a few minutes the first time..."
    cargo install --path . --force --features desktop --bin $Bin --bin $DesktopBin
    if ($LASTEXITCODE -ne 0) { Die "cargo install failed." }
    # cargo installs into %USERPROFILE%\.cargo\bin, already on PATH via rustup.
    $script:InstallDir = Join-Path $env:USERPROFILE '.cargo\bin'
    Ok "Built and installed -> $(Join-Path $script:InstallDir "$Bin.exe")"
    if (Test-Path (Join-Path $script:InstallDir "$DesktopBin.exe")) {
        $script:TrayInstalled = $true
        Ok "Built tray companion -> $(Join-Path $script:InstallDir "$DesktopBin.exe")"
    }
}

# --- choose a strategy -----------------------------------------------------------------
# Order: explicit --build -> a local dist\ prebuilt (repo-dev fast path) -> download a prebuilt
# (works with no clone). dist\ is gitignored, so a fresh `git clone` falls through to download.
if ($ForceBuild) {
    Install-ViaCargo
}
elseif (Test-Path $Prebuilt) {
    Install-File $Prebuilt
}
else {
    Install-Download
}

# --- make sure the install dir is on the user PATH -------------------------------------
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = @()
if ($userPath) {
    # GetEnvironmentVariable returns the raw REG_EXPAND_SZ string, so rustup's entry is the
    # literal "%USERPROFILE%\.cargo\bin". Expand each entry before comparing, or it never matches
    # the resolved $InstallDir and we append a duplicate on every run.
    $parts = $userPath -split ';' | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_) }
}
if ($parts -notcontains $InstallDir) {
    $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Ok "Added $InstallDir to your user PATH (open a new terminal to pick it up)."
}
else {
    Ok "$InstallDir is on your PATH"
}
# Update the current session's PATH independently — $env:Path merges machine + user + process
# scopes, so guard it separately to avoid a duplicate entry in this shell.
if (($env:Path -split ';') -notcontains $InstallDir) {
    $env:Path = "$env:Path;$InstallDir"
}

# --- preflight the runtime tools -------------------------------------------------------
# mpv + yt-dlp are required for playback/search; ffmpeg is needed for downloads.
$missing = @()
foreach ($t in 'mpv', 'yt-dlp', 'ffmpeg') {
    if (-not (Get-Command $t -ErrorAction SilentlyContinue)) { $missing += $t }
}
if ($missing.Count -gt 0) {
    $list = $missing -join ' '
    Warn "Missing runtime tools: $list - install with:  scoop install $list   (or winget install $list)"
}
else {
    Ok "Runtime tools present (mpv, yt-dlp, ffmpeg)"
}

# --- register the Windows media identity ------------------------------------------------
# Names the OS media flyout entry ("YtmTui" + icon instead of "Unknown app"). Idempotent;
# writes only HKCU\Software\Classes\AppUserModelId. Version-gated because on older builds
# an unknown subcommand falls through to launching the TUI, which must never happen inside
# this script (e.g. YTT_VERSION pinned to a pre-1.6 release).
$yttExe = Join-Path $InstallDir "$Bin.exe"
$supportsIdentity = $false
$verLine = & $yttExe --version 2>$null
if ("$verLine" -match '(\d+)\.(\d+)\.(\d+)') {
    $v = [Version]::new([int]$Matches[1], [int]$Matches[2], [int]$Matches[3])
    $supportsIdentity = $v -ge [Version]::new(1, 6, 0)
}
if ($supportsIdentity) {
    $iconArgs = @()
    $localIcon = Join-Path $InstallDir 'ytm-tui.ico'
    $repoIcon = Join-Path $ScriptDir 'assets\icons\ytm-tui.ico'
    # Prefer the icon installed next to the exe; a source checkout (--build path) falls
    # back to the repo asset so cargo-built installs get an icon too.
    if (-not (Test-Path $localIcon) -and (Test-Path $repoIcon)) {
        Copy-Item $repoIcon $localIcon -Force
    }
    if (Test-Path $localIcon) { $iconArgs = @('--icon', $localIcon) }
    & $yttExe register-media-identity @iconArgs *> $null
    if ($LASTEXITCODE -eq 0) {
        Ok "Media identity registered (Windows media flyout shows 'YtmTui')"
    }
    else {
        Warn "Media identity not registered (cosmetic only; flyout may show 'Unknown app')"
    }
}

Write-Host ""
if ($script:TrayInstalled) {
    Ok "Done. Start it with:  $Bin   Tray: $DesktopBin"
}
else {
    Ok "Done. Start it with:  $Bin"
}
