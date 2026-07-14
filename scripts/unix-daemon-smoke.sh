#!/usr/bin/env bash
set -euo pipefail

target=""
profile="release"

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
    -h|--help)
      echo "Usage: scripts/unix-daemon-smoke.sh [--target <triple>] [--profile debug|release]"
      exit 0
      ;;
    *)
      echo "unix-daemon-smoke.sh: unknown argument $1" >&2
      exit 2
      ;;
  esac
done

case "$(uname -s)" in
  Darwin)
    # CI macOS runners have no GUI/login session, so the OS Now Playing session can't
    # attach and would wedge the daemon's event loop. Exercise the daemon headless; the
    # spawned daemon inherits this env var. (Linux MPRIS degrades gracefully, so it stays
    # enabled below and keeps that coverage.)
    export YTM_NO_MEDIA_SESSION=1
    ;;
  Linux) ;;
  *)
    echo "unix-daemon-smoke.sh must run on macOS or Linux" >&2
    exit 2
    ;;
esac

if ! command -v mpv >/dev/null 2>&1; then
  echo "mpv is required for the daemon smoke test" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for the daemon smoke test" >&2
  exit 1
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin_dir="$repo_root/target"
if [[ -n "$target" ]]; then
  bin_dir="$bin_dir/$target"
fi
bin_dir="$bin_dir/$profile"
ytt="$bin_dir/ytt"
if [[ ! -x "$ytt" ]]; then
  echo "missing executable: $ytt" >&2
  exit 1
fi

work_root="$(mktemp -d "/tmp/ytt-smoke.XXXXXX")"
old_home="${HOME:-}"
old_user="${USER:-}"
old_xdg_runtime="${XDG_RUNTIME_DIR:-}"
old_xdg_config="${XDG_CONFIG_HOME:-}"
old_xdg_data="${XDG_DATA_HOME:-}"
old_xdg_cache="${XDG_CACHE_HOME:-}"
old_mpv_extra="${YTM_MPV_EXTRA:-}"
smoke_wait_seconds="${YTM_SMOKE_WAIT_SECONDS:-30}"

cleanup() {
  local status=$?
  set +e
  if [[ $status -ne 0 ]]; then
    echo "unix daemon smoke failed; diagnostics from $work_root:" >&2
    find "$work_root" -maxdepth 5 -type f -print >&2
    local log_path
    log_path="$(find "$work_root" -path '*/logs/daemon.log*' -type f -print -quit 2>/dev/null)"
    if [[ -n "$log_path" ]]; then
      echo "--- $(basename "$log_path") ---" >&2
      tail -200 "$log_path" >&2
      echo "--- end $(basename "$log_path") ---" >&2
    fi
    if [[ -f "$work_root/mpv.log" ]]; then
      echo "--- mpv.log ---" >&2
      tail -200 "$work_root/mpv.log" >&2
      echo "--- end mpv.log ---" >&2
    fi
  fi
  "$ytt" daemon stop >/dev/null 2>&1
  export HOME="$old_home"
  export USER="$old_user"
  export XDG_RUNTIME_DIR="$old_xdg_runtime"
  export XDG_CONFIG_HOME="$old_xdg_config"
  export XDG_DATA_HOME="$old_xdg_data"
  export XDG_CACHE_HOME="$old_xdg_cache"
  export YTM_MPV_EXTRA="$old_mpv_extra"
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
export YTM_MPV_EXTRA="--ao=null --volume=0 --log-file=$work_root/mpv.log"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"

if [[ "$(uname -s)" == "Darwin" ]]; then
  data_dir="$HOME/Library/Application Support/yututui"
  config_dir="$HOME/Library/Application Support/yututui"
  cache_dir="$HOME/Library/Caches/yututui"
else
  data_dir="$XDG_DATA_HOME/yututui"
  config_dir="$XDG_CONFIG_HOME/yututui"
  cache_dir="$XDG_CACHE_HOME/yututui"
fi
mkdir -p "$data_dir" "$config_dir" "$cache_dir"

wav_one="$work_root/unix-smoke-one.wav"
wav_two="$work_root/unix-smoke-two.wav"
python3 - "$wav_one" "$wav_two" <<'PY'
import sys
import wave

for path in sys.argv[1:]:
    with wave.open(path, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(44100)
        wav.writeframes(b"\0\0" * 44100 * 20)
PY

cat >"$config_dir/config.json" <<'JSON'
{
  "volume": 0,
  "gapless": false,
  "speed": 1.0,
  "seek_seconds": 5,
  "repeat": "off"
}
JSON

python3 - "$cache_dir/session.json" "$wav_one" "$wav_two" <<'PY'
import json
import sys

session_path, wav_one, wav_two = sys.argv[1:]
songs = [
    {
        "video_id": "local:unix-smoke-one",
        "title": "Unix Smoke One",
        "artist": "yututui",
        "duration": "0:20",
        "local_path": wav_one,
    },
    {
        "video_id": "local:unix-smoke-two",
        "title": "Unix Smoke Two",
        "artist": "yututui",
        "duration": "0:20",
        "local_path": wav_two,
    },
]
session = {
    "last_mode": "normal",
    "normal_queue": {
        "songs": songs,
        "order": [0, 1],
        "cursor": 0,
        "shuffle": False,
        "repeat": "off",
    },
    "radio_queue": None,
}
with open(session_path, "w", encoding="utf-8") as f:
    json.dump(session, f)
PY

cat >"$data_dir/library.json" <<'JSON'
{
  "favorites": [],
  "history": [],
  "radio_favorites": [],
  "radios": []
}
JSON

wait_until() {
  local label="$1"
  local deadline=$((SECONDS + ${2:-10}))
  shift 2
  while (( SECONDS < deadline )); do
    if "$@"; then
      return 0
    fi
    sleep 0.2
  done
  echo "timed out waiting for $label" >&2
  return 1
}

daemon_status_raw() {
  "$ytt" daemon status --json 2>/dev/null
}

status_idle() {
  local raw
  raw="$(daemon_status_raw)" || return 1
  python3 -c '
import json, sys
doc = json.loads(sys.argv[1])
status = doc.get("status") or {}
ok = doc.get("ok") and status.get("owner_mode") == "daemon" and status.get("title") is None and status.get("paused") is True
sys.exit(0 if ok else 1)
' "$raw"
}

status_title_is() {
  local title="$1"
  local raw
  raw="$(daemon_status_raw)" || return 1
  python3 -c '
import json, sys
title = sys.argv[2]
doc = json.loads(sys.argv[1])
status = doc.get("status") or {}
sys.exit(0 if doc.get("ok") and status.get("title") == title else 1)
' "$raw" "$title"
}

status_title_paused_is() {
  local title="$1"
  local paused="$2"
  local raw
  raw="$(daemon_status_raw)" || return 1
  python3 -c '
import json, sys
title = sys.argv[2]
paused = sys.argv[3] == "true"
doc = json.loads(sys.argv[1])
status = doc.get("status") or {}
ok = doc.get("ok") and status.get("title") == title and status.get("paused") is paused
sys.exit(0 if ok else 1)
' "$raw" "$title" "$paused"
}

status_stopped() {
  ! "$ytt" daemon status --json >/dev/null 2>&1
}

"$ytt" daemon start >/dev/null
wait_until "idle daemon status" "$smoke_wait_seconds" status_idle

"$ytt" daemon start --resume >/dev/null
wait_until "resumed daemon playback" "$smoke_wait_seconds" status_title_is "Unix Smoke One"

"$ytt" -r pp >/dev/null
wait_until "remote pause" "$smoke_wait_seconds" status_title_paused_is "Unix Smoke One" true

"$ytt" -r pp >/dev/null
wait_until "remote resume" "$smoke_wait_seconds" status_title_paused_is "Unix Smoke One" false

"$ytt" -r next >/dev/null
wait_until "remote next" "$smoke_wait_seconds" status_title_is "Unix Smoke Two"

"$ytt" -r prev >/dev/null
wait_until "remote previous" "$smoke_wait_seconds" status_title_is "Unix Smoke One"

"$ytt" daemon stop >/dev/null
wait_until "daemon stop" "$smoke_wait_seconds" status_stopped

if ! find "$cache_dir/logs" -maxdepth 1 -name 'daemon.log*' -type f -print -quit >/dev/null 2>&1; then
  echo "daemon log was not created under $cache_dir/logs" >&2
  exit 1
fi

echo "Unix daemon smoke passed"
