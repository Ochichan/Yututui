# ytm-tui installer (Windows) — makes `ytm` runnable from any terminal, no manual setup.
#
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
#   powershell -ExecutionPolicy Bypass -File .\install.ps1 --build   # force a source build
#
# Uses a prebuilt dist\ytm.exe if present, otherwise builds from source with cargo.
# Adds the install dir to your user PATH and checks for the mpv / yt-dlp runtime tools.

$ErrorActionPreference = 'Stop'
$Bin = 'ytm'

# $PSScriptRoot is reliably set for script-file execution; fall back to the CWD when the script
# is run via -Command / Invoke-Expression (where $MyInvocation.MyCommand.Path is $null).
$ScriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
Set-Location $ScriptDir

function Info($m) { Write-Host "==> $m"   -ForegroundColor Cyan }
function Ok($m)   { Write-Host "OK  $m"    -ForegroundColor Green }
function Warn($m) { Write-Host "warn: $m"  -ForegroundColor Yellow }
function Die($m)  { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

$ForceBuild = $args -contains '--build'
$Prebuilt   = Join-Path $ScriptDir 'dist\ytm.exe'

# These drive the install directory below; on Server Core / some CI / redirected-profile
# environments they can be unset, which would silently produce a bogus root-relative path.
if (-not $env:LOCALAPPDATA) { Die '$env:LOCALAPPDATA is not set; cannot determine install directory.' }
if (-not $env:USERPROFILE)  { Die '$env:USERPROFILE is not set.' }

# dist\ is gitignored, so a fresh `git clone` won't have it -> fall through to cargo.
if (-not $ForceBuild -and (Test-Path $Prebuilt)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\ytm'
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $Dest = Join-Path $InstallDir "$Bin.exe"
    Copy-Item $Prebuilt $Dest -Force
    Ok "Installed prebuilt binary -> $Dest"
}
elseif (Get-Command cargo -ErrorAction SilentlyContinue) {
    Info "Building from source with cargo - this can take a few minutes the first time..."
    cargo install --path . --force
    if ($LASTEXITCODE -ne 0) { Die "cargo install failed." }
    # cargo installs into %USERPROFILE%\.cargo\bin, already on PATH via rustup.
    $InstallDir = Join-Path $env:USERPROFILE '.cargo\bin'
    Ok "Built and installed -> $(Join-Path $InstallDir "$Bin.exe")"
}
elseif ($ForceBuild) {
    Die "Rust (cargo) is not installed or not on PATH — required for --build.`n  Install Rust: https://rustup.rs  then re-run."
}
else {
    Die "No prebuilt dist\ytm.exe and Rust isn't installed.`n  Install Rust: https://rustup.rs  then re-run, or download a prebuilt binary from the project's Releases page."
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
$missing = @()
foreach ($t in 'mpv', 'yt-dlp') {
    if (-not (Get-Command $t -ErrorAction SilentlyContinue)) { $missing += $t }
}
if ($missing.Count -gt 0) {
    $list = $missing -join ' '
    Warn "Missing runtime tools: $list - install with:  scoop install $list   (or winget install $list)"
}
else {
    Ok "Runtime tools present (mpv, yt-dlp)"
}

Write-Host ""
Ok "Done. Start it with:  $Bin"
