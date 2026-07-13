<#
.SYNOPSIS
Builds and probes the pinned mpv 0.32 baseline through an MSYS2 Bash toolchain.

.DESCRIPTION
The build is installed only below OutputRoot. The script does not install dependencies or write to
global prefixes. Supply a prepared MSYS2 environment with the mpv build dependencies available.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $OutputRoot,
    [string] $Bash = "C:\msys64\usr\bin\bash.exe",
    [ValidateRange(1, 1024)] [int] $Jobs = [Environment]::ProcessorCount,
    [ValidateSet("UCRT64", "MINGW64", "CLANG64")] [string] $MsysSystem = "UCRT64",
    [string] $Python = "",
    [string] $WafPython = "",
    [string] $WafFile = "",
    [string] $CFlags = "-DNDEBUG",
    [string] $CppFlags = "",
    [string] $LdFlags = "",
    [string] $FfmpegPrefix = "",
    [string] $FfmpegSourceArchive = "",
    [string] $FfmpegConfigureLog = "",
    [string] $FfmpegBuildLog = "",
    [string] $FfmpegInstallLog = "",
    [string[]] $ConfigureArg = @()
)

$ErrorActionPreference = "Stop"
$buildScript = Join-Path $PSScriptRoot "build-mpv-032.sh"
if (-not (Test-Path -LiteralPath $Bash -PathType Leaf)) {
    throw "MSYS2 Bash is unavailable: $Bash"
}
if (-not (Test-Path -LiteralPath $buildScript -PathType Leaf)) {
    throw "mpv build script is unavailable: $buildScript"
}
if ([string]::IsNullOrEmpty($Python)) {
    $Python = Get-Command python.exe -All -ErrorAction SilentlyContinue |
        Where-Object { $_.Source -notlike "*\\WindowsApps\\*" } |
        Select-Object -First 1 -ExpandProperty Source
}
if ([string]::IsNullOrEmpty($Python) -or -not (Test-Path -LiteralPath $Python -PathType Leaf)) {
    throw "native Windows Python is unavailable; pass -Python explicitly"
}
$pythonOsName = & $Python -c "import os; print(os.name)"
if ($LASTEXITCODE -ne 0 -or ($pythonOsName | Select-Object -Last 1).Trim() -ne "nt") {
    throw "-Python must select a native Windows Python executable"
}
if ([string]::IsNullOrEmpty($WafPython)) {
    $bashBin = Split-Path -Parent $Bash
    $WafPython = @(
        (Join-Path $bashBin "python3.12.exe"),
        (Join-Path $bashBin "python3.exe")
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
}
if ([string]::IsNullOrEmpty($WafPython) -or -not (Test-Path -LiteralPath $WafPython -PathType Leaf)) {
    throw "MSYS2 waf Python is unavailable; pass -WafPython explicitly"
}
$wafPythonCheck = & $WafPython -c "from distutils.version import StrictVersion; print('ok')"
if ($LASTEXITCODE -ne 0 -or ($wafPythonCheck | Select-Object -Last 1).Trim() -ne "ok") {
    throw "-WafPython must provide distutils.version for pinned mpv waf"
}
$ffmpegInputs = @{
    FfmpegPrefix = $FfmpegPrefix
    FfmpegSourceArchive = $FfmpegSourceArchive
    FfmpegConfigureLog = $FfmpegConfigureLog
    FfmpegBuildLog = $FfmpegBuildLog
    FfmpegInstallLog = $FfmpegInstallLog
}
foreach ($entry in $ffmpegInputs.GetEnumerator()) {
    if ([string]::IsNullOrEmpty($entry.Value)) {
        throw "-$($entry.Key) is required for explicit FFmpeg dependency provenance"
    }
}
if (-not (Test-Path -LiteralPath $FfmpegPrefix -PathType Container)) {
    throw "FFmpeg prefix is unavailable: $FfmpegPrefix"
}
foreach ($path in @(
    $FfmpegSourceArchive,
    $FfmpegConfigureLog,
    $FfmpegBuildLog,
    $FfmpegInstallLog
)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "FFmpeg provenance input is unavailable: $path"
    }
}
if (Test-Path -LiteralPath $OutputRoot) {
    throw "output root must not already exist: $OutputRoot"
}

# Values are positional parameters to Bash, not interpolated into the command string. This keeps
# Windows paths and configure arguments data-only even when they contain whitespace or metacharacters.
$bashCommand = @'
set -euo pipefail
case "$MSYSTEM" in
    UCRT64) toolchain_root=/ucrt64 ;;
    MINGW64) toolchain_root=/mingw64 ;;
    CLANG64) toolchain_root=/clang64 ;;
    *) echo "unsupported MSYS2 toolchain: $MSYSTEM" >&2; exit 2 ;;
esac
export PATH="$toolchain_root/bin:/usr/local/bin:/usr/bin:/bin"
runtime_dlls=(
    "$toolchain_root/bin/libwinpthread-1.dll"
    "$toolchain_root/bin/zlib1.dll"
)
pkg_config="$toolchain_root/bin/pkg-config.exe"
cc="$toolchain_root/bin/gcc.exe"
cxx="$toolchain_root/bin/g++.exe"
for compiler in "$cc" "$cxx"; do
    [[ -f "$compiler" && ! -L "$compiler" && -x "$compiler" ]] || {
        echo "required UCRT compiler is unavailable: $compiler" >&2
        exit 2
    }
done
[[ -f "$pkg_config" && ! -L "$pkg_config" && -x "$pkg_config" ]] || {
    echo "required UCRT pkg-config is unavailable: $pkg_config" >&2
    exit 2
}
for runtime_dll in "${runtime_dlls[@]}"; do
    [[ -f "$runtime_dll" && ! -L "$runtime_dll" ]] || {
        echo "required UCRT runtime DLL is unavailable: $runtime_dll" >&2
        exit 2
    }
done
build_script=$(cygpath -u -- "$1")
output_root=$(cygpath -u -- "$2")
jobs=$3
native_python=$(cygpath -u -- "$4")
waf_python=$(cygpath -u -- "$5")
shift 5
arguments=(
    --output-root "$output_root"
    --jobs "$jobs"
    --python "$native_python"
    --waf-python "$waf_python"
    --cc "$cc"
    --cxx "$cxx"
)
waf_argument=$1
shift
if [[ "$waf_argument" != "-" ]]; then
    arguments+=(--waf-file "$(cygpath -u -- "$waf_argument")")
fi
cflags_argument=$1
cppflags_argument=$2
ldflags_argument=$3
shift 3
[[ "$cflags_argument" == "-" ]] && cflags_argument=""
[[ "$cppflags_argument" == "-" ]] && cppflags_argument=""
[[ "$ldflags_argument" == "-" ]] && ldflags_argument=""
arguments+=(
    --cflags "$cflags_argument"
    --cppflags "$cppflags_argument"
    --ldflags "$ldflags_argument"
)
ffmpeg_prefix=$(cygpath -u -- "$1")
ffmpeg_source_archive=$(cygpath -u -- "$2")
ffmpeg_configure_log=$(cygpath -u -- "$3")
ffmpeg_build_log=$(cygpath -u -- "$4")
ffmpeg_install_log=$(cygpath -u -- "$5")
shift 5
arguments+=(
    --pkg-config "$pkg_config"
    --ffmpeg-prefix "$ffmpeg_prefix"
    --ffmpeg-source-archive "$ffmpeg_source_archive"
    --ffmpeg-configure-log "$ffmpeg_configure_log"
    --ffmpeg-build-log "$ffmpeg_build_log"
    --ffmpeg-install-log "$ffmpeg_install_log"
)
for runtime_dll in "${runtime_dlls[@]}"; do
    arguments+=(--runtime-dll "$runtime_dll")
done
for configure_arg in "$@"; do
    arguments+=(--configure-arg "$configure_arg")
done
exec "$build_script" "${arguments[@]}"
'@
$bashCommandBase64 = [Convert]::ToBase64String(
    [Text.Encoding]::UTF8.GetBytes($bashCommand)
)
$bashLoader = 'set -o pipefail; printf %s "$YTT_MPV_032_BASH_COMMAND_B64" | /usr/bin/base64 --decode | /usr/bin/bash -s -- "$@"'

$oldMsysSystem = [Environment]::GetEnvironmentVariable("MSYSTEM", "Process")
$oldChereInvoking = [Environment]::GetEnvironmentVariable("CHERE_INVOKING", "Process")
$oldBashCommandBase64 = [Environment]::GetEnvironmentVariable(
    "YTT_MPV_032_BASH_COMMAND_B64", "Process"
)
try {
    [Environment]::SetEnvironmentVariable("MSYSTEM", $MsysSystem, "Process")
    [Environment]::SetEnvironmentVariable("CHERE_INVOKING", "1", "Process")
    [Environment]::SetEnvironmentVariable(
        "YTT_MPV_032_BASH_COMMAND_B64", $bashCommandBase64, "Process"
    )
    # Windows PowerShell 5.1 drops empty native-command arguments. Keep the optional waf-file
    # slot deterministic so the first build flag can never be mistaken for a waf path. Empty
    # build flags use a sentinel and are restored by the data-only Bash loader.
    $wafArgument = if ([string]::IsNullOrEmpty($WafFile)) { "-" } else { $WafFile }
    $cflagsArgument = if ([string]::IsNullOrEmpty($CFlags)) { "-" } else { $CFlags }
    $cppflagsArgument = if ([string]::IsNullOrEmpty($CppFlags)) { "-" } else { $CppFlags }
    $ldflagsArgument = if ([string]::IsNullOrEmpty($LdFlags)) { "-" } else { $LdFlags }
    $savedErrorActionPreference = $ErrorActionPreference
    try {
        # Windows PowerShell 5.1 surfaces every native stderr line as a non-terminating
        # NativeCommandError. Preserve that evidence while using the process exit code as
        # the execution result; warning-bearing cells are classified separately as non-ship.
        $ErrorActionPreference = "Continue"
        & $Bash --noprofile --norc -lc $bashLoader mpv-032-loader `
            $buildScript $OutputRoot ([string] $Jobs) $Python $WafPython $wafArgument `
            $cflagsArgument $cppflagsArgument $ldflagsArgument `
            $FfmpegPrefix $FfmpegSourceArchive $FfmpegConfigureLog `
            $FfmpegBuildLog $FfmpegInstallLog @ConfigureArg
        $bashExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedErrorActionPreference
    }
    if ($bashExitCode -ne 0) {
        [Console]::Error.WriteLine(
            "pinned mpv 0.32 build/probe finished with exit code $bashExitCode"
        )
        exit $bashExitCode
    }
} finally {
    [Environment]::SetEnvironmentVariable("MSYSTEM", $oldMsysSystem, "Process")
    [Environment]::SetEnvironmentVariable("CHERE_INVOKING", $oldChereInvoking, "Process")
    [Environment]::SetEnvironmentVariable(
        "YTT_MPV_032_BASH_COMMAND_B64", $oldBashCommandBase64, "Process"
    )
}
