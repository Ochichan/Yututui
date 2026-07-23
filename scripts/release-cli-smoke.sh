#!/usr/bin/env bash
set -euo pipefail

target=""
profile="release"
ytt=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      target="${2:?missing value for --target}"
      shift 2
      ;;
    --profile)
      profile="${2:?missing value for --profile}"
      shift 2
      ;;
    --ytt)
      ytt="${2:?missing value for --ytt}"
      shift 2
      ;;
    -h|--help)
      echo "Usage: scripts/release-cli-smoke.sh [--target <triple>] [--profile debug|release] [--ytt <path>]"
      exit 0
      ;;
    *)
      echo "release-cli-smoke.sh: unknown argument $1" >&2
      exit 2
      ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ -z "$ytt" ]]; then
  bin_dir="$repo_root/target"
  if [[ -n "$target" ]]; then
    bin_dir="$bin_dir/$target"
  fi
  ytt="$bin_dir/$profile/ytt"
fi
if [[ ! -x "$ytt" ]]; then
  echo "missing executable: $ytt" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for the release CLI smoke test" >&2
  exit 1
fi

work_root="$(mktemp -d "${TMPDIR:-/tmp}/ytt-release-cli-smoke.XXXXXX")"
old_home="${HOME:-}"
old_user="${USER:-}"
old_xdg_runtime="${XDG_RUNTIME_DIR:-}"
old_xdg_config="${XDG_CONFIG_HOME:-}"
old_xdg_data="${XDG_DATA_HOME:-}"
old_xdg_cache="${XDG_CACHE_HOME:-}"
old_tools_dir="${YTM_TOOLS_DIR:-}"
old_ytdlp="${YTM_YTDLP:-}"
old_mpv="${YTM_MPV:-}"

cleanup() {
  local status=$?
  export HOME="$old_home"
  export USER="$old_user"
  export XDG_RUNTIME_DIR="$old_xdg_runtime"
  export XDG_CONFIG_HOME="$old_xdg_config"
  export XDG_DATA_HOME="$old_xdg_data"
  export XDG_CACHE_HOME="$old_xdg_cache"
  export YTM_TOOLS_DIR="$old_tools_dir"
  export YTM_YTDLP="$old_ytdlp"
  export YTM_MPV="$old_mpv"
  rm -rf "$work_root"
  exit "$status"
}
trap cleanup EXIT

export HOME="$work_root/home"
export USER="yttsmoke"
export XDG_RUNTIME_DIR="$work_root/runtime"
export XDG_CONFIG_HOME="$work_root/xdg-config"
export XDG_DATA_HOME="$work_root/xdg-data"
export XDG_CACHE_HOME="$work_root/xdg-cache"
export YTM_TOOLS_DIR="$work_root/tools"
unset YTM_YTDLP
unset YTM_MPV
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$YTM_TOOLS_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

stdout_file="$work_root/stdout.txt"
stderr_file="$work_root/stderr.txt"
run_ytt() {
  : >"$stdout_file"
  : >"$stderr_file"
  set +e
  "$ytt" "$@" >"$stdout_file" 2>"$stderr_file"
  rc=$?
  set -e
}

run_ytt --version
if [[ "$rc" -ne 0 ]] || ! grep -qE '^ytt ' "$stdout_file"; then
  echo "unexpected ytt --version result (exit $rc)" >&2
  cat "$stdout_file" >&2
  cat "$stderr_file" >&2
  exit 1
fi

run_ytt --help
if [[ "$rc" -ne 0 ]] ||
  ! grep -q 'Usage: ytt \[OPTIONS\]' "$stdout_file" ||
  ! grep -q 'ytt doctor terminal --json' "$stdout_file"; then
  echo "unexpected ytt --help result (exit $rc)" >&2
  cat "$stdout_file" >&2
  cat "$stderr_file" >&2
  exit 1
fi

run_ytt doctor terminal --json
if [[ "$rc" -ne 0 ]]; then
  echo "ytt doctor terminal --json failed (exit $rc)" >&2
  cat "$stderr_file" >&2
  exit 1
fi
python3 - "$stdout_file" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    doc = json.load(f)
if not isinstance(doc.get("image_protocol"), str):
    raise SystemExit("doctor JSON missing image_protocol")
if not isinstance(doc.get("zoom_mode"), str):
    raise SystemExit("doctor JSON missing zoom_mode")
if doc.get("mouse_capture_configured", False) is not None:
    raise SystemExit("doctor JSON must report mouse_capture_configured=null")
if doc.get("mouse_capture_source") != "not_loaded_by_read_only_diagnostic":
    raise SystemExit("doctor JSON did not report the read-only mouse_capture_source")
PY

run_ytt daemon status
if [[ "$rc" -ne 1 ]] || ! grep -q 'ytt daemon:' "$stderr_file"; then
  echo "daemon status should fail cleanly without a daemon (exit $rc)" >&2
  cat "$stdout_file" >&2
  cat "$stderr_file" >&2
  exit 1
fi

run_ytt daemon status --json
if [[ "$rc" -ne 1 ]] ||
  [[ -s "$stdout_file" ]] ||
  ! grep -q 'ytt daemon:' "$stderr_file"; then
  echo "daemon status --json should fail without partial JSON (exit $rc)" >&2
  cat "$stdout_file" >&2
  cat "$stderr_file" >&2
  exit 1
fi

echo "Release CLI smoke passed: $ytt"
