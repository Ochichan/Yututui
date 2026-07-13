#!/usr/bin/env bash
# Build and probe the supported mpv 0.32 baseline into a new target-local prefix.

set -euo pipefail

readonly OFFICIAL_REPOSITORY="https://github.com/mpv-player/mpv.git"
readonly PINNED_COMMIT="70b991749df389bcc0a4e145b5687233a03b4ed7"
readonly COMPAT_UPSTREAM_COMMIT="1805681aaba22aa19a27ecfdb639c983d91f83e6"
readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly PROBE_TOOL="$SCRIPT_DIR/probe-mpv-032.py"
readonly PERF_TOOL="$SCRIPT_DIR/tui-perf.py"
readonly COMPAT_PATCH="$SCRIPT_DIR/mpv-032-m-option-value-zero-init.patch"

output_root=""
jobs=""
python_command="python3"
waf_python_command=""
git_command="git"
cc_command="cc"
cxx_command="c++"
waf_file=""
cflags="-DNDEBUG"
cppflags=""
ldflags=""
pkg_config_command="pkg-config"
ffmpeg_prefix=""
ffmpeg_source_archive=""
ffmpeg_configure_log=""
ffmpeg_build_log=""
ffmpeg_install_log=""
declare -a configure_args=()
declare -a runtime_dlls=()

usage() {
    cat <<'EOF'
Usage: scripts/build-mpv-032.sh --output-root PATH [options]

Builds official mpv commit 70b991749df389bcc0a4e145b5687233a03b4ed7 into
PATH/install, records the exact build inputs, and runs the IPC compatibility probe.
The caller must provide the platform's mpv build dependencies; nothing is installed globally.

Options:
  --output-root PATH       New directory that will contain source, prefix, logs, and evidence.
  --jobs N                 Parallel build jobs (default: detected logical CPU count).
  --python COMMAND         Native/runtime Python used by manifest and probe (default: python3).
  --waf-python COMMAND     Python executable with distutils used by waf (default: --python).
  --git COMMAND            Git executable used for source acquisition (default: git).
  --cc COMMAND             C compiler executable (default: cc).
  --cxx COMMAND            C++ compiler executable (default: c++).
  --waf-file PATH          Prebuilt waf 2.0.9 or 2.0.27 script for offline/current Python hosts.
  --cflags VALUE           Exact C compiler flags (default: -DNDEBUG).
  --cppflags VALUE         Exact C preprocessor flags (default: empty).
  --ldflags VALUE          Exact linker flags (default: empty).
  --pkg-config COMMAND     Exact pkg-config executable for FFmpeg verification.
  --ffmpeg-prefix PATH     Prepared FFmpeg 4.4.8 dependency prefix.
  --ffmpeg-source-archive PATH
                           FFmpeg 4.4.8 source archive used for the prefix.
  --ffmpeg-configure-log PATH
                           Configure log from the prepared FFmpeg build.
  --ffmpeg-build-log PATH  Build log from the prepared FFmpeg build.
  --ffmpeg-install-log PATH
                           Install log from the prepared FFmpeg build.
  --runtime-dll PATH       Runtime DLL copied beside mpv.exe; repeat as needed.
  --configure-arg VALUE    Extra waf configure argument; repeat as needed.
  -h, --help               Show this help.
EOF
}

while (($# > 0)); do
    case "$1" in
        --output-root)
            (($# >= 2)) || { echo "--output-root requires a value" >&2; exit 2; }
            output_root=$2
            shift 2
            ;;
        --jobs)
            (($# >= 2)) || { echo "--jobs requires a value" >&2; exit 2; }
            jobs=$2
            shift 2
            ;;
        --python)
            (($# >= 2)) || { echo "--python requires a value" >&2; exit 2; }
            python_command=$2
            shift 2
            ;;
        --waf-python)
            (($# >= 2)) || { echo "--waf-python requires a value" >&2; exit 2; }
            waf_python_command=$2
            shift 2
            ;;
        --git)
            (($# >= 2)) || { echo "--git requires a value" >&2; exit 2; }
            git_command=$2
            shift 2
            ;;
        --cc)
            (($# >= 2)) || { echo "--cc requires a value" >&2; exit 2; }
            cc_command=$2
            shift 2
            ;;
        --cxx)
            (($# >= 2)) || { echo "--cxx requires a value" >&2; exit 2; }
            cxx_command=$2
            shift 2
            ;;
        --waf-file)
            (($# >= 2)) || { echo "--waf-file requires a value" >&2; exit 2; }
            waf_file=$2
            shift 2
            ;;
        --cflags)
            (($# >= 2)) || { echo "--cflags requires a value" >&2; exit 2; }
            cflags=$2
            shift 2
            ;;
        --cppflags)
            (($# >= 2)) || { echo "--cppflags requires a value" >&2; exit 2; }
            cppflags=$2
            shift 2
            ;;
        --ldflags)
            (($# >= 2)) || { echo "--ldflags requires a value" >&2; exit 2; }
            ldflags=$2
            shift 2
            ;;
        --pkg-config)
            (($# >= 2)) || { echo "--pkg-config requires a value" >&2; exit 2; }
            pkg_config_command=$2
            shift 2
            ;;
        --ffmpeg-prefix)
            (($# >= 2)) || { echo "--ffmpeg-prefix requires a value" >&2; exit 2; }
            ffmpeg_prefix=$2
            shift 2
            ;;
        --ffmpeg-source-archive)
            (($# >= 2)) || { echo "--ffmpeg-source-archive requires a value" >&2; exit 2; }
            ffmpeg_source_archive=$2
            shift 2
            ;;
        --ffmpeg-configure-log)
            (($# >= 2)) || { echo "--ffmpeg-configure-log requires a value" >&2; exit 2; }
            ffmpeg_configure_log=$2
            shift 2
            ;;
        --ffmpeg-build-log)
            (($# >= 2)) || { echo "--ffmpeg-build-log requires a value" >&2; exit 2; }
            ffmpeg_build_log=$2
            shift 2
            ;;
        --ffmpeg-install-log)
            (($# >= 2)) || { echo "--ffmpeg-install-log requires a value" >&2; exit 2; }
            ffmpeg_install_log=$2
            shift 2
            ;;
        --runtime-dll)
            (($# >= 2)) || { echo "--runtime-dll requires a value" >&2; exit 2; }
            runtime_dlls+=("$2")
            shift 2
            ;;
        --configure-arg)
            (($# >= 2)) || { echo "--configure-arg requires a value" >&2; exit 2; }
            configure_args+=("$2")
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

[[ -n "$output_root" ]] || { echo "--output-root is required" >&2; exit 2; }
[[ -n "$ffmpeg_prefix" ]] || { echo "--ffmpeg-prefix is required" >&2; exit 2; }
[[ -n "$ffmpeg_source_archive" ]] || { echo "--ffmpeg-source-archive is required" >&2; exit 2; }
[[ -n "$ffmpeg_configure_log" ]] || { echo "--ffmpeg-configure-log is required" >&2; exit 2; }
[[ -n "$ffmpeg_build_log" ]] || { echo "--ffmpeg-build-log is required" >&2; exit 2; }
[[ -n "$ffmpeg_install_log" ]] || { echo "--ffmpeg-install-log is required" >&2; exit 2; }
[[ ! -e "$output_root" && ! -L "$output_root" ]] || {
    echo "output root must not already exist: $output_root" >&2
    exit 2
}
[[ -f "$PROBE_TOOL" ]] || { echo "missing probe tool: $PROBE_TOOL" >&2; exit 2; }
[[ -f "$PERF_TOOL" ]] || { echo "missing performance tool: $PERF_TOOL" >&2; exit 2; }
[[ -f "$COMPAT_PATCH" && ! -L "$COMPAT_PATCH" ]] || {
    echo "compatibility patch must be an existing regular non-symlink file: $COMPAT_PATCH" >&2
    exit 2
}

if ! python_executable=$(command -v -- "$python_command"); then
    if [[ "$python_command" == "python3" ]]; then
        python_command="python"
        python_executable=$(command -v -- "$python_command") || {
            echo "python3 and python executables are unavailable" >&2
            exit 2
        }
    else
        echo "python executable is unavailable: $python_command" >&2
        exit 2
    fi
fi
waf_python_command=${waf_python_command:-$python_command}
waf_python_executable=$(command -v -- "$waf_python_command") || {
    echo "waf Python executable is unavailable: $waf_python_command" >&2
    exit 2
}
git_executable=$(command -v -- "$git_command") || {
    echo "git executable is unavailable: $git_command" >&2
    exit 2
}
cc_executable=$(command -v -- "$cc_command") || {
    echo "C compiler executable is unavailable: $cc_command" >&2
    exit 2
}
cxx_executable=$(command -v -- "$cxx_command") || {
    echo "C++ compiler executable is unavailable: $cxx_command" >&2
    exit 2
}
pkg_config_executable=$(command -v -- "$pkg_config_command") || {
    echo "pkg-config executable is unavailable: $pkg_config_command" >&2
    exit 2
}
if [[ -n "$waf_file" ]]; then
    [[ -f "$waf_file" && ! -L "$waf_file" ]] || {
        echo "waf file must be an existing regular non-symlink file: $waf_file" >&2
        exit 2
    }
    waf_file="$(cd -- "$(dirname -- "$waf_file")" && pwd -P)/$(basename -- "$waf_file")"
fi
for runtime_dll in "${runtime_dlls[@]}"; do
    [[ -f "$runtime_dll" && ! -L "$runtime_dll" ]] || {
        echo "runtime DLL must be an existing regular non-symlink file: $runtime_dll" >&2
        exit 2
    }
done
[[ -f "$python_executable" && -x "$python_executable" ]] || {
    echo "selected Python command is not an executable file: $python_executable" >&2
    exit 2
}
[[ -f "$waf_python_executable" && -x "$waf_python_executable" ]] || {
    echo "selected waf Python command is not an executable file: $waf_python_executable" >&2
    exit 2
}
[[ -f "$git_executable" && -x "$git_executable" ]] || {
    echo "selected Git command is not an executable file: $git_executable" >&2
    exit 2
}
[[ -f "$cc_executable" && -x "$cc_executable" ]] || {
    echo "selected C compiler command is not an executable file: $cc_executable" >&2
    exit 2
}
[[ -f "$cxx_executable" && -x "$cxx_executable" ]] || {
    echo "selected C++ compiler command is not an executable file: $cxx_executable" >&2
    exit 2
}
[[ -f "$pkg_config_executable" && -x "$pkg_config_executable" ]] || {
    echo "selected pkg-config command is not an executable file: $pkg_config_executable" >&2
    exit 2
}
python_executable="$(cd -- "$(dirname -- "$python_executable")" && pwd -P)/$(basename -- "$python_executable")"
waf_python_executable="$(cd -- "$(dirname -- "$waf_python_executable")" && pwd -P)/$(basename -- "$waf_python_executable")"
git_executable="$(cd -- "$(dirname -- "$git_executable")" && pwd -P)/$(basename -- "$git_executable")"
cc_executable="$(cd -- "$(dirname -- "$cc_executable")" && pwd -P)/$(basename -- "$cc_executable")"
cxx_executable="$(cd -- "$(dirname -- "$cxx_executable")" && pwd -P)/$(basename -- "$cxx_executable")"
pkg_config_executable="$(cd -- "$(dirname -- "$pkg_config_executable")" && pwd -P)/$(basename -- "$pkg_config_executable")"
[[ -d "$ffmpeg_prefix" && ! -L "$ffmpeg_prefix" ]] || {
    echo "FFmpeg prefix must be an existing non-symlink directory: $ffmpeg_prefix" >&2
    exit 2
}
ffmpeg_prefix="$(cd -- "$ffmpeg_prefix" && pwd -P)"
for ffmpeg_input in \
    "$ffmpeg_source_archive" \
    "$ffmpeg_configure_log" \
    "$ffmpeg_build_log" \
    "$ffmpeg_install_log"; do
    [[ -f "$ffmpeg_input" && ! -L "$ffmpeg_input" ]] || {
        echo "FFmpeg provenance input must be a regular non-symlink file: $ffmpeg_input" >&2
        exit 2
    }
done
ffmpeg_source_archive="$(cd -- "$(dirname -- "$ffmpeg_source_archive")" && pwd -P)/$(basename -- "$ffmpeg_source_archive")"
ffmpeg_configure_log="$(cd -- "$(dirname -- "$ffmpeg_configure_log")" && pwd -P)/$(basename -- "$ffmpeg_configure_log")"
ffmpeg_build_log="$(cd -- "$(dirname -- "$ffmpeg_build_log")" && pwd -P)/$(basename -- "$ffmpeg_build_log")"
ffmpeg_install_log="$(cd -- "$(dirname -- "$ffmpeg_install_log")" && pwd -P)/$(basename -- "$ffmpeg_install_log")"
output_parent=$(dirname -- "$output_root")
[[ -d "$output_parent" ]] || {
    echo "output root parent must already exist: $output_parent" >&2
    exit 2
}
output_root="$(cd -- "$output_parent" && pwd -P)/$(basename -- "$output_root")"

if [[ -z "$jobs" ]]; then
    if command -v nproc >/dev/null 2>&1; then
        jobs=$(nproc)
    elif command -v sysctl >/dev/null 2>&1; then
        jobs=$(sysctl -n hw.logicalcpu)
    else
        jobs=1
    fi
fi
[[ "$jobs" =~ ^[1-9][0-9]*$ ]] || { echo "--jobs must be a positive integer" >&2; exit 2; }
[[ " $cflags $cppflags " =~ [[:space:]]-DNDEBUG[[:space:]] ]] || {
    echo "the supported mpv 0.32 build requires -DNDEBUG in CFLAGS or CPPFLAGS" >&2
    exit 2
}

umask 077
mkdir -p -- "$output_root/logs"
readonly source_root="$output_root/source"
readonly install_prefix="$output_root/install"
readonly clone_log="$output_root/logs/source.log"
readonly bootstrap_log="$output_root/logs/bootstrap.log"
readonly configure_log="$output_root/logs/configure.log"
readonly build_log="$output_root/logs/build.log"
readonly install_log="$output_root/logs/install.log"
readonly build_manifest="$output_root/build-manifest.json"
readonly probe_result="$output_root/probe.json"
readonly selection_manifest="$output_root/selection-manifest.json"

{
    "$git_executable" init -q "$source_root"
    "$git_executable" -C "$source_root" remote add origin "$OFFICIAL_REPOSITORY"
    "$git_executable" -C "$source_root" fetch --depth=1 origin "$PINNED_COMMIT"
    "$git_executable" -c advice.detachedHead=false -C "$source_root" \
        checkout -q --detach FETCH_HEAD
    test "$("$git_executable" -C "$source_root" rev-parse HEAD)" = "$PINNED_COMMIT"
    "$git_executable" -C "$source_root" apply --check --whitespace=error-all "$COMPAT_PATCH"
    "$git_executable" -C "$source_root" apply --whitespace=error-all "$COMPAT_PATCH"
    "$git_executable" -C "$source_root" diff --check
} >"$clone_log" 2>&1

export LC_ALL=C
export LANG=C
export PYTHONHASHSEED=0
export CC="$cc_executable"
export CXX="$cxx_executable"
export CFLAGS="$cflags"
export CPPFLAGS="$cppflags"
export LDFLAGS="$ldflags"
export PKG_CONFIG_PATH="$ffmpeg_prefix/lib/pkgconfig"
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH=$("$git_executable" -C "$source_root" show -s --format=%ct HEAD)

if [[ -n "$waf_file" ]]; then
    cp -- "$waf_file" "$source_root/waf"
    chmod 0700 "$source_root/waf"
    printf 'Using caller-supplied waf file: %s\n' "$waf_file" >"$bootstrap_log"
else
    (
        cd -- "$source_root"
        "$waf_python_executable" ./bootstrap.py
    ) >"$bootstrap_log" 2>&1
fi
waf_version=$("$waf_python_executable" "$source_root/waf" --version 2>>"$bootstrap_log")
case "$waf_version" in
    "waf 2.0.9 "*|"waf 2.0.27 "*) ;;
    *)
        echo "supported waf versions are 2.0.9 and 2.0.27, observed: $waf_version" >&2
        exit 2
        ;;
esac
printf 'Verified waf version: %s\n' "$waf_version" >>"$bootstrap_log"

declare -a configure_command=(
    "$waf_python_executable"
    "$source_root/waf"
    configure
    "--prefix=$install_prefix"
)
if ((${#configure_args[@]} > 0)); then
    configure_command+=("${configure_args[@]}")
fi
(
    cd -- "$source_root"
    "${configure_command[@]}"
) >"$configure_log" 2>&1
(
    cd -- "$source_root"
    "$waf_python_executable" ./waf build "-j$jobs"
) >"$build_log" 2>&1
(
    cd -- "$source_root"
    "$waf_python_executable" ./waf install
) >"$install_log" 2>&1

if [[ -f "$install_prefix/bin/mpv.exe" ]]; then
    mpv_binary="$install_prefix/bin/mpv.exe"
elif [[ -f "$install_prefix/bin/mpv" ]]; then
    mpv_binary="$install_prefix/bin/mpv"
else
    echo "target-local mpv executable was not installed under $install_prefix/bin" >&2
    exit 1
fi

declare -A staged_runtime_dll_names=()
for runtime_dll in "${runtime_dlls[@]}"; do
    runtime_name=$(basename -- "$runtime_dll")
    runtime_name_key=${runtime_name,,}
    [[ "$runtime_name_key" == *.dll ]] || {
        echo "runtime dependency is not a DLL: $runtime_dll" >&2
        exit 2
    }
    [[ -z "${staged_runtime_dll_names[$runtime_name_key]+present}" ]] || {
        echo "duplicate runtime DLL basename: $runtime_name" >&2
        exit 2
    }
    staged_runtime_dll_names[$runtime_name_key]=1
    cp -- "$runtime_dll" "$(dirname -- "$mpv_binary")/$runtime_name"
done

declare -a manifest_command=(
    "$python_executable"
    "$PROBE_TOOL"
    manifest
    --source "$source_root"
    --prefix "$install_prefix"
    --binary "$mpv_binary"
    --output "$build_manifest"
    --clone-log "$clone_log"
    --bootstrap-log "$bootstrap_log"
    --configure-log "$configure_log"
    --build-log "$build_log"
    --install-log "$install_log"
    --git "$git_executable"
    --python "$python_executable"
    --waf-python "$waf_python_executable"
    --cc "$cc_executable"
    --cxx "$cxx_executable"
    --jobs "$jobs"
    "--cflags=$cflags"
    "--cppflags=$cppflags"
    "--ldflags=$ldflags"
    --pkg-config "$pkg_config_executable"
    --ffmpeg-prefix "$ffmpeg_prefix"
    --ffmpeg-source-archive "$ffmpeg_source_archive"
    --ffmpeg-configure-log "$ffmpeg_configure_log"
    --ffmpeg-build-log "$ffmpeg_build_log"
    --ffmpeg-install-log "$ffmpeg_install_log"
    --compat-patch "$COMPAT_PATCH"
    --compat-upstream-commit "$COMPAT_UPSTREAM_COMMIT"
)
if [[ -n "$waf_file" ]]; then
    manifest_command+=(--waf-seed "$waf_file")
fi
if ((${#configure_args[@]} > 0)); then
    for configure_arg in "${configure_args[@]}"; do
        manifest_command+=("--configure-arg=$configure_arg")
    done
fi
if ((${#runtime_dlls[@]} > 0)); then
    for runtime_dll in "${runtime_dlls[@]}"; do
        manifest_command+=(--runtime-dll "$runtime_dll")
    done
fi
"${manifest_command[@]}"

"$python_executable" "$PROBE_TOOL" probe \
    --build-manifest "$build_manifest" \
    --binary "$mpv_binary" \
    --output "$probe_result"

"$python_executable" "$PERF_TOOL" create-mpv-selection \
    --target-root "$output_root" \
    --binary "$mpv_binary" \
    --build-manifest "$build_manifest" \
    --probe-manifest "$probe_result" \
    --output "$selection_manifest"

printf 'mpv 0.32 build manifest: %s\n' "$build_manifest"
printf 'mpv 0.32 compatibility probe: %s\n' "$probe_result"
printf 'mpv 0.32 selection manifest: %s\n' "$selection_manifest"
ship_evidence_eligible=$(
    "$python_executable" "$PERF_TOOL" mpv-selection \
        --manifest "$selection_manifest" \
        --field ship_evidence_eligible
)
if [[ "$ship_evidence_eligible" != "true" ]]; then
    warning_count=$(
        "$python_executable" "$PERF_TOOL" mpv-selection \
            --manifest "$selection_manifest" \
            --field provenance.warnings.count
    )
    echo "diagnostic-only mpv 0.32 cell: recorded warning count $warning_count" >&2
    exit 3
fi
