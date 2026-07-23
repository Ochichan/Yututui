param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [ValidateSet("debug", "release")]
    [string]$Profile = "release",
    [string]$YttPath = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $YttPath) {
    $YttPath = Join-Path $RepoRoot "target\$Target\$Profile\ytt.exe"
}
if (-not (Test-Path -LiteralPath $YttPath -PathType Leaf)) {
    throw "missing executable: $YttPath"
}

$WorkRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("ytt-release-cli-smoke-" + [Guid]::NewGuid().ToString("N"))
$OldEnv = @{}

function Save-Env {
    param([string]$Name)
    $OldEnv[$Name] = [Environment]::GetEnvironmentVariable($Name, "Process")
}

function Set-Env {
    param(
        [string]$Name,
        [AllowNull()]
        [string]$Value
    )
    [Environment]::SetEnvironmentVariable($Name, $Value, "Process")
}

function Restore-Env {
    foreach ($name in $OldEnv.Keys) {
        [Environment]::SetEnvironmentVariable($name, $OldEnv[$name], "Process")
    }
}

function Invoke-YttCapture {
    param([string[]]$Arguments)

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $YttPath
    foreach ($arg in $Arguments) {
        [void]$psi.ArgumentList.Add($arg)
    }
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true

    $proc = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    [void]$proc.Start()
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()

    return [pscustomobject]@{
        ExitCode = $proc.ExitCode
        Stdout = $stdout
        Stderr = $stderr
    }
}

function Assert-Contains {
    param(
        [string]$Text,
        [string]$Needle,
        [string]$Label
    )
    if (-not $Text.Contains($Needle)) {
        throw "$Label did not contain '$Needle': $Text"
    }
}

try {
    New-Item -ItemType Directory -Force -Path $WorkRoot | Out-Null

    foreach ($name in @("USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "YTM_TOOLS_DIR", "YTM_YTDLP", "YTM_MPV")) {
        Save-Env $name
    }

    $profileRoot = Join-Path $WorkRoot "profile"
    $roamingRoot = Join-Path $WorkRoot "roaming"
    $localRoot = Join-Path $WorkRoot "local"
    $tempRoot = Join-Path $WorkRoot "tmp"
    $toolsRoot = Join-Path $WorkRoot "tools"
    New-Item -ItemType Directory -Force -Path $profileRoot, $roamingRoot, $localRoot, $tempRoot, $toolsRoot | Out-Null

    Set-Env "USERPROFILE" $profileRoot
    Set-Env "APPDATA" $roamingRoot
    Set-Env "LOCALAPPDATA" $localRoot
    Set-Env "TEMP" $tempRoot
    Set-Env "TMP" $tempRoot
    Set-Env "YTM_TOOLS_DIR" $toolsRoot
    Set-Env "YTM_YTDLP" $null
    Set-Env "YTM_MPV" $null

    $version = Invoke-YttCapture @("--version")
    if ($version.ExitCode -ne 0 -or -not $version.Stdout.StartsWith("ytt ")) {
        throw "unexpected ytt --version result (exit $($version.ExitCode)): $($version.Stdout)$($version.Stderr)"
    }

    $help = Invoke-YttCapture @("--help")
    if ($help.ExitCode -ne 0) {
        throw "ytt --help failed with exit $($help.ExitCode): $($help.Stderr)"
    }
    Assert-Contains $help.Stdout "Usage: ytt [OPTIONS]" "ytt --help"
    Assert-Contains $help.Stdout "ytt doctor terminal --json" "ytt --help"

    $doctor = Invoke-YttCapture @("doctor", "terminal", "--json")
    if ($doctor.ExitCode -ne 0) {
        throw "ytt doctor terminal --json failed with exit $($doctor.ExitCode): $($doctor.Stderr)"
    }
    $doc = $doctor.Stdout | ConvertFrom-Json
    if (-not ($doc.image_protocol -is [string]) -or -not ($doc.zoom_mode -is [string])) {
        throw "doctor JSON missing image_protocol or zoom_mode: $($doctor.Stdout)"
    }
    if ($null -ne $doc.mouse_capture_configured) {
        throw "doctor JSON must report mouse_capture_configured=null: $($doctor.Stdout)"
    }
    if ($doc.mouse_capture_source -ne "not_loaded_by_read_only_diagnostic") {
        throw "doctor JSON did not report the read-only mouse_capture_source: $($doctor.Stdout)"
    }

    $status = Invoke-YttCapture @("daemon", "status")
    if ($status.ExitCode -ne 1 -or -not $status.Stderr.Contains("ytt daemon:")) {
        throw "daemon status should fail cleanly without a daemon (exit $($status.ExitCode)): $($status.Stdout)$($status.Stderr)"
    }

    $jsonStatus = Invoke-YttCapture @("daemon", "status", "--json")
    if ($jsonStatus.ExitCode -ne 1 -or $jsonStatus.Stdout.Trim().Length -ne 0 -or -not $jsonStatus.Stderr.Contains("ytt daemon:")) {
        throw "daemon status --json should fail without partial JSON (exit $($jsonStatus.ExitCode)): $($jsonStatus.Stdout)$($jsonStatus.Stderr)"
    }

    Write-Host "Release CLI smoke passed: $YttPath"
} finally {
    Restore-Env
    Remove-Item -LiteralPath $WorkRoot -Recurse -Force -ErrorAction SilentlyContinue
}
