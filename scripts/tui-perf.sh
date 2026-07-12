#!/usr/bin/env bash
set -euo pipefail

# Performance evidence must describe the normal product. Never inherit the optional hot-path
# instrumentation switch into the sampler, controller, or measured ytt process.
unset YTM_PERF

# Native Unix launcher for paired ytt TUI performance runs. Real TUI binaries always run inside
# a unique tmux server with a fake home and null audio, matching the repository verify contract.
# Cleanup is confined to the exact owner/mpv live identities recorded by the Rust sampler and
# completes before the unique tmux server may be stopped.

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
python_tool="$repo_dir/scripts/tui-perf.py"
scenarios="$repo_dir/scripts/tui-perf-scenarios.json"
scenario=""
baseline_source_root=""
candidate_source_root="$repo_dir"
output=""
seed_home=""
baseline_seed_home=""
candidate_seed_home=""

usage() {
  cat <<'EOF'
Usage: scripts/tui-perf.sh --scenario ID --output DIR [options]

Required source inputs:
  --baseline-source-root PATH  Clean worktree at candidate origin/main (required)
  --candidate-source-root PATH Exact clean candidate HEAD (defaults to this repo)

Playback scenarios:
  --seed-home DIR              Isolated HOME template (required for playback scenarios).
                               Persisted state must live under stores/{config,data,cache}.
                               Resumed Song.local_path must be {{TUI_PERF_PLAYLIST}}.
  --baseline-seed-home DIR     Baseline template; must hash-identically to candidate.
  --candidate-seed-home DIR    Candidate template; must hash-identically to baseline.

Harness options:
  --scenarios PATH             Override scenario JSON

The output directory retains every fake home, raw NDJSON, and paired report. Never point it at a
real profile. The script alternates AB/BA order and never uses process-name-wide termination.
EOF
}

while (($#)); do
  case "$1" in
    --scenario) scenario=${2:?}; shift 2 ;;
    --output) output=${2:?}; shift 2 ;;
    --baseline-source-root) baseline_source_root=${2:?}; shift 2 ;;
    --candidate-source-root) candidate_source_root=${2:?}; shift 2 ;;
    --seed-home) seed_home=${2:?}; shift 2 ;;
    --baseline-seed-home) baseline_seed_home=${2:?}; shift 2 ;;
    --candidate-seed-home) candidate_seed_home=${2:?}; shift 2 ;;
    --scenarios) scenarios=${2:?}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'tui-perf.sh: unknown argument %q\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$scenario" ]] || { echo "tui-perf.sh: --scenario is required" >&2; exit 2; }
[[ -n "$output" ]] || { echo "tui-perf.sh: --output is required" >&2; exit 2; }
[[ ! -e "$output" ]] || {
  echo "tui-perf.sh: --output must name a new path; existing evidence is never reused" >&2
  exit 2
}
[[ -n "$baseline_source_root" ]] || { echo "tui-perf.sh: --baseline-source-root is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "tui-perf.sh: python3 is required" >&2; exit 2; }
command -v git >/dev/null || { echo "tui-perf.sh: git is required for source identity" >&2; exit 2; }
[[ -d "$baseline_source_root" ]] || { echo "tui-perf.sh: baseline source root is not a directory" >&2; exit 2; }
[[ -d "$candidate_source_root" ]] || { echo "tui-perf.sh: candidate source root is not a directory" >&2; exit 2; }
output=$(python3 "$python_tool" path-preflight \
  --output-root "$output" \
  --protected-root "$baseline_source_root" \
  --protected-root "$candidate_source_root")

python3 "$python_tool" validate --scenarios "$scenarios" >/dev/null
scenario_hash=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field sha256)
pairs=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field pairs)
candidate_repeats=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field candidate_repeats)
geometry_count=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field geometry.length)
requires_mpv=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field requires_mpv)
traffic_profile=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field traffic_profile)
fixture_seconds=$(python3 "$python_tool" setting --scenarios "$scenarios" --field fixture.duration_s)
fixture_sample_rate=$(python3 "$python_tool" setting --scenarios "$scenarios" --field fixture.sample_rate_hz)
is_render=false
[[ "$scenario" == "render_and_interaction" ]] && is_render=true
baseline_seed_home=${baseline_seed_home:-$seed_home}
candidate_seed_home=${candidate_seed_home:-$seed_home}

if [[ "$requires_mpv" == true ]]; then
  [[ -d "$baseline_seed_home" ]] || { echo "tui-perf.sh: baseline seed home is required" >&2; exit 2; }
  [[ -d "$candidate_seed_home" ]] || { echo "tui-perf.sh: candidate seed home is required" >&2; exit 2; }
  python3 "$python_tool" path-preflight \
    --output-root "$output" \
    --protected-root "$baseline_seed_home" \
    --protected-root "$candidate_seed_home" >/dev/null
fi
if ! $is_render; then
  command -v tmux >/dev/null || { echo "tui-perf.sh: tmux is required for real TUI runs" >&2; exit 2; }
fi

mkdir -p "$output"
output=$(cd "$output" && pwd)
build_receipt="$output/build-receipt.json"
build_args=(
  build --scenarios "$scenarios" --scenario "$scenario"
  --baseline-root "$baseline_source_root" --candidate-root "$candidate_source_root"
  --output "$build_receipt"
)
build_target=$(mktemp -d "${TMPDIR:-/tmp}/ytt-perf-build.XXXXXX")
rmdir "$build_target"
build_args+=(--target-root "$build_target")
if ! python3 "$python_tool" "${build_args[@]}" >/dev/null; then
  [[ -z "$build_target" ]] || rm -rf -- "$build_target"
  exit 2
fi
if [[ -n "$build_target" ]]; then
  rm -rf -- "$build_target"
fi

receipt_field() {
  python3 "$python_tool" receipt --receipt "$build_receipt" --artifact "$1" --field path
}
if $is_render; then
  baseline_render=$(receipt_field baseline_render)
  candidate_render=$(receipt_field candidate_render)
else
  baseline_binary=$(receipt_field baseline_ytt)
  candidate_binary=$(receipt_field candidate_ytt)
  sampler=$(receipt_field sampler)
  controller=$(receipt_field controller)
fi

if [[ "$requires_mpv" == true ]]; then
  python3 "$python_tool" seed-contract \
    --scenarios "$scenarios" --scenario "$scenario" \
    --baseline-root "$baseline_seed_home" --candidate-root "$candidate_seed_home" \
    --snapshot "$output/seed-template" --output "$output/seed-contract.json" >/dev/null
  baseline_seed_home="$output/seed-template"
  candidate_seed_home="$output/seed-template"
fi

active_socket=""
active_identity=""
active_server_pid=""
active_server_identity=""
active_server_run_id=""
socket_root=$(mktemp -d "${TMPDIR:-/tmp}/ytt-perf-tmux.XXXXXX")
socket_counter=0

cleanup_active_run() {
  local cleanup_status=0
  if [[ -n "$active_identity" ]]; then
    if ! python3 "$python_tool" cleanup \
      --identity "$active_identity" \
      --timeout-secs 10; then
      echo "tui-perf.sh: exact process cleanup/revalidation failed; refusing to kill the tmux server" >&2
      cleanup_status=1
    fi
  fi
  if [[ -n "$active_socket" ]] && tmux -S "$active_socket" has-session 2>/dev/null; then
    if ((cleanup_status != 0)); then
      return "$cleanup_status"
    fi
    if ! tmux -S "$active_socket" kill-server; then
      echo "tui-perf.sh: failed to stop the isolated tmux server after exact process cleanup" >&2
      cleanup_status=1
    fi
  fi
  if ((cleanup_status == 0)); then
    active_socket=""
    active_identity=""
  fi
  return "$cleanup_status"
}

cleanup_server() {
  [[ -n "$active_server_pid" ]] || return 0
  if [[ -z "$active_server_identity" || -z "$active_server_run_id" ]]; then
    echo "tui-perf.sh: fixture server has no exact shutdown identity; refusing to signal its numeric PID" >&2
    return 1
  fi
  if ! python3 "$python_tool" stop-server \
    --identity "$active_server_identity" \
    --expected-run-id "$active_server_run_id" \
    --timeout-secs 10 >/dev/null; then
    echo "tui-perf.sh: exact authenticated fixture server shutdown failed; no PID signal was sent" >&2
    return 1
  fi
  if ! wait "$active_server_pid"; then
    echo "tui-perf.sh: fixture server exited unsuccessfully after exact shutdown" >&2
    return 1
  fi
  active_server_pid=""
  active_server_identity=""
  active_server_run_id=""
}

server_child_is_running() {
  local job_pid
  while IFS= read -r job_pid; do
    [[ "$job_pid" == "$active_server_pid" ]] && return 0
  done <<<"$(jobs -pr)"
  return 1
}

cleanup_on_exit() {
  local exit_status=$?
  local cleanup_status=0
  trap - EXIT
  # Once exact cleanup starts, a repeated terminal signal must not interrupt the
  # terminate -> wait -> revalidate sequence and strand a measured descendant.
  trap '' INT TERM
  cleanup_active_run || cleanup_status=$?
  if ! cleanup_server; then
    cleanup_status=1
  fi
  if ((cleanup_status == 0)); then
    rm -rf -- "$socket_root"
  elif ((exit_status == 0)); then
    exit_status=2
  fi
  exit "$exit_status"
}

trap cleanup_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fixture_file="$output/fixture/silence-${fixture_seconds}s.wav"
if [[ "$requires_mpv" == true ]]; then
  mkdir -p "$(dirname "$fixture_file")"
  if [[ ! -f "$fixture_file" ]]; then
    python3 "$python_tool" fixture \
      --output "$fixture_file" \
      --manifest "$output/fixture/manifest.json" \
      --seconds "$fixture_seconds" \
      --sample-rate "$fixture_sample_rate" >/dev/null
  fi
fi

manifest_args=(
  manifest
  --scenarios "$scenarios"
  --scenario "$scenario"
  --output "$output/host-manifest.json"
  --build-receipt "$build_receipt"
)
python3 "$python_tool" "${manifest_args[@]}" >/dev/null

json_array_to_csv() {
  python3 -c 'import json,sys; print(",".join(str(v) for v in json.loads(sys.argv[1])))' "$1"
}

run_render() {
  local role=$1 binary=$2 run_dir=$3 run_id=$4
  mkdir -p "$run_dir"
  TUI_PERF_SCENARIO_SHA256="$scenario_hash" \
  TUI_PERF_RUN_ID="$run_id" \
    "$binary" \
      --output "$run_dir/render.json" \
      --warmup "$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field warmup_draws)" \
      --batches "$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field batches)" \
      --draws "$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field draws_per_batch)"
}

run_process() {
  local role=$1 binary=$2 run_root=$3 width=$4 height=$5 label=$6 run_id=$7
  local role_seed_home=$candidate_seed_home
  [[ "$role" == baseline ]] && role_seed_home=$baseline_seed_home
  local run_dir="$run_root"
  if ((geometry_count > 1)); then
    run_dir="$run_root/geometry-${width}x${height}"
  fi
  local home="$run_dir/home"
  local runtime="$run_dir/runtime"
  local tmp="$run_dir/tmp"
  local config_store="$home/stores/config"
  local data_store="$home/stores/data"
  local cache_store="$home/stores/cache"
  local samples="$run_dir/samples.ndjson"
  local pid_file="$run_dir/ytt.pid"
  local identity_file="$run_dir/process-identity.json"
  local controller_ready_file="$run_dir/controller-ready.json"
  ((socket_counter += 1))
  local socket="$socket_root/s${socket_counter}.sock"
  mkdir -p "$home" "$runtime" "$tmp" "$config_store" "$data_store" "$cache_store"
  chmod 700 "$runtime"
  if [[ -n "$role_seed_home" ]]; then
    cp -R "$role_seed_home"/. "$home"/
  fi
  if [[ "$requires_mpv" == true ]]; then
    local ready_file="$run_dir/http-ready.json"
    local throttle outage_every outage_ms disconnect_every fixture_url shutdown_token
    rm -f -- "$ready_file"
    throttle=$(python3 "$python_tool" traffic --scenarios "$scenarios" --name "$traffic_profile" --field throttle_bps)
    outage_every=$(python3 "$python_tool" traffic --scenarios "$scenarios" --name "$traffic_profile" --field outage_every_bytes)
    outage_ms=$(python3 "$python_tool" traffic --scenarios "$scenarios" --name "$traffic_profile" --field outage_ms)
    disconnect_every=$(python3 "$python_tool" traffic --scenarios "$scenarios" --name "$traffic_profile" --field disconnect_every_bytes)
    shutdown_token=$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')
    active_server_identity=$ready_file
    active_server_run_id=$run_id
    python3 "$python_tool" serve \
      --file "$fixture_file" \
      --ready-file "$ready_file" \
      --request-log "$run_dir/http-requests.ndjson" \
      --run-id "$run_id" \
      "--shutdown-token=$shutdown_token" \
      --throttle-bps "$throttle" \
      --outage-every-bytes "$outage_every" \
      --outage-ms "$outage_ms" \
      --disconnect-every-bytes "$disconnect_every" \
      >"$run_dir/http-server.log" 2>&1 &
    active_server_pid=$!
    local server_deadline=$((SECONDS + 10))
    until [[ -s "$ready_file" ]]; do
      if ((SECONDS >= server_deadline)) || ! server_child_is_running; then
        if ! server_child_is_running; then
          wait "$active_server_pid" 2>/dev/null || true
          active_server_pid=""
          active_server_identity=""
          active_server_run_id=""
        fi
        echo "tui-perf.sh: fixture server failed for $label" >&2
        return 1
      fi
      sleep 0.05
    done
    fixture_url=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["url"])' "$ready_file")
    python3 "$python_tool" materialize \
      --root "$home" \
      --home "$home" \
      --fixture-url "$fixture_url" \
      --seed-label "$role" \
      --input-snapshot "$run_dir/materialized-inputs" \
      --manifest "$run_dir/materialize.json" >/dev/null
  fi
  python3 "$python_tool" launch-policy \
    --root "$home" \
    --output "$run_dir/launch-policy.json" >/dev/null
  local warmup sample interval require_arg=""
  warmup=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field warmup_s)
  sample=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field sample_s)
  interval=$(python3 "$python_tool" setting \
    --scenarios "$scenarios" --field sampling.interval_ms)
  [[ "$requires_mpv" == true ]] && require_arg="--require-silent-mpv"

  local -a isolated_env=(
    "PATH=$PATH"
    "HOME=$home"
    "XDG_CONFIG_HOME=$home/.config"
    "XDG_DATA_HOME=$home/.local/share"
    "XDG_CACHE_HOME=$home/.cache"
    "XDG_STATE_HOME=$home/.local/state"
    "XDG_RUNTIME_DIR=$runtime"
    "YTM_CONFIG_DIR=$config_store"
    "YTM_DATA_DIR=$data_store"
    "YTM_CACHE_DIR=$cache_store"
    "TMPDIR=$tmp"
    "TEMP=$tmp"
    "TMP=$tmp"
    "TERM=xterm-256color"
    "YTM_MPV_EXTRA=--ao=null --volume=0 --audio-display=no"
    "TUI_PERF_SCENARIO_SHA256=$scenario_hash"
    "TUI_PERF_RUN_ID=$run_id"
  )
  [[ -n "${LANG:-}" ]] && isolated_env+=("LANG=$LANG")
  [[ -n "${LC_ALL:-}" ]] && isolated_env+=("LC_ALL=$LC_ALL")
  [[ -n "${LC_CTYPE:-}" ]] && isolated_env+=("LC_CTYPE=$LC_CTYPE")
  mkdir -p "$home/.config" "$home/.local/share" "$home/.cache" "$home/.local/state"

  local -a sampler_cmd=(
    env -i "${isolated_env[@]}" "$sampler"
    --output "$samples"
    --pid-file "$pid_file"
    --identity-file "$identity_file"
    --binary "$binary"
    --warmup-secs "$warmup"
    --duration-secs "$sample"
    --interval-ms "$interval"
  )
  [[ -n "$require_arg" ]] && sampler_cmd+=("$require_arg")
  local controller_enabled
  controller_enabled=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field controller)
  # Non-controller runs do not need a discoverable primary endpoint. Controller runs omit
  # --new-instance so ytt publishes its descriptor inside this run's unique XDG_RUNTIME_DIR;
  # every other profile/runtime path is isolated above, so this cannot address the user's ytt.
  if [[ "$controller_enabled" != true ]]; then
    sampler_cmd+=(-- --new-instance)
  else
    sampler_cmd+=(--controller-ready-file "$controller_ready_file")
  fi
  local shell_command
  printf -v shell_command '%q ' "${sampler_cmd[@]}"
  rm -f -- "$pid_file" "$identity_file" "$controller_ready_file"
  active_socket=$socket
  active_identity=$identity_file
  tmux -S "$socket" new-session -d -x "$width" -y "$height" "$shell_command"

  local ready_deadline=$((SECONDS + 30))
  until [[ -s "$pid_file" ]]; do
    if ((SECONDS >= ready_deadline)) || ! tmux -S "$socket" has-session 2>/dev/null; then
      echo "tui-perf.sh: $label failed before publishing its PID" >&2
      return 1
    fi
    sleep 0.1
  done

  if [[ "$controller_enabled" == true ]]; then
    local load seeks_json seeks_csv pause_policy pause_hold_ms
    load=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field controller_load)
    seeks_json=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field seeks_s)
    seeks_csv=$(json_array_to_csv "$seeks_json")
    pause_policy=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field pause_policy)
    pause_hold_ms=$(python3 "$python_tool" scenario --scenarios "$scenarios" --id "$scenario" --field pause_hold_ms)
    local -a control_cmd=(
      env -i "${isolated_env[@]}" "$controller"
      --output "$run_dir/control.ndjson"
      --ready-file "$controller_ready_file"
      --wait-secs 45
      --observe-secs "$(python3 -c 'import sys; print(float(sys.argv[1])+float(sys.argv[2]))' "$warmup" "$sample")"
      --close-grace-secs 15
      --load "$load"
    )
    [[ -n "$seeks_csv" ]] && control_cmd+=(--seeks "$seeks_csv")
    if [[ "$pause_policy" == pause-resume ]]; then
      control_cmd+=(--pause-hold-ms "$pause_hold_ms")
    else
      control_cmd+=(--no-pause)
    fi
    "${control_cmd[@]}"
  fi

  local finish_deadline
  finish_deadline=$(python3 -c 'import math,sys; print(int(math.ceil(float(sys.argv[1])+float(sys.argv[2])+90)))' "$warmup" "$sample")
  finish_deadline=$((SECONDS + finish_deadline))
  while tmux -S "$socket" has-session 2>/dev/null; do
    if ((SECONDS >= finish_deadline)); then
      echo "tui-perf.sh: timed out waiting for $label" >&2
      return 1
    fi
    sleep 1
  done
  cleanup_active_run
  local -a check_args=(
    check --samples "$samples" --scenario-sha256 "$scenario_hash"
  )
  [[ "$requires_mpv" == true ]] && check_args+=(--require-silent-mpv)
  if [[ "$controller_enabled" == true ]]; then
    check_args+=(--control "$run_dir/control.ndjson" --require-observer-close)
  fi
  python3 "$python_tool" "${check_args[@]}" >/dev/null
  if [[ -n "$active_server_pid" ]]; then
    cleanup_server
  fi
}

run_process_geometries() {
  local role=$1 binary=$2 run_root=$3 label=$4 kind=$5 run_index=$6 root_run_id=$7
  local geometry_index width height geometry_dir geometry_run_id
  for ((geometry_index=0; geometry_index<geometry_count; geometry_index++)); do
    width=$(python3 "$python_tool" scenario \
      --scenarios "$scenarios" --id "$scenario" --field "geometry.$geometry_index.0")
    height=$(python3 "$python_tool" scenario \
      --scenarios "$scenarios" --id "$scenario" --field "geometry.$geometry_index.1")
    geometry_dir="$run_root"
    geometry_run_id=$root_run_id
    if ((geometry_count > 1)); then
      geometry_dir="$run_root/geometry-${width}x${height}"
      local -a geometry_start_args=(
        run-start
        --scenarios "$scenarios"
        --scenario "$scenario"
        --output "$geometry_dir/run-contract.json"
        --kind "$kind"
        --role "$role"
        --geometry-index "$geometry_index"
        --width "$width"
        --height "$height"
      )
      if [[ "$kind" == paired ]]; then
        geometry_start_args+=(--pair-index "$run_index")
      else
        geometry_start_args+=(--repeat-index "$run_index")
      fi
      geometry_run_id=$(python3 "$python_tool" "${geometry_start_args[@]}")
    fi
    run_process "$role" "$binary" "$run_root" "$width" "$height" \
      "$label geometry ${width}x${height}" "$geometry_run_id"
    if ((geometry_count > 1)); then
      python3 "$python_tool" run-finish \
        --contract "$geometry_dir/run-contract.json" >/dev/null
    fi
  done
}

baseline_runs=()
candidate_runs=()
for ((pair=1; pair<=pairs; pair++)); do
  if ((pair % 2 == 1)); then
    order=(baseline candidate)
  else
    order=(candidate baseline)
  fi
  for role in "${order[@]}"; do
    run_root="$output/pair-$(printf '%02d' "$pair")/$role"
    run_id=""
    root_contract=false
    if $is_render || ((geometry_count == 1)); then
      root_contract=true
      run_id=$(python3 "$python_tool" run-start \
        --scenarios "$scenarios" --scenario "$scenario" \
        --output "$run_root/run-contract.json" \
        --kind paired --role "$role" --pair-index "$pair")
    fi
    if $is_render; then
      [[ "$role" == baseline ]] && binary=$baseline_render || binary=$candidate_render
      run_render "$role" "$binary" "$run_root" "$run_id"
    else
      [[ "$role" == baseline ]] && binary=$baseline_binary || binary=$candidate_binary
      run_process_geometries "$role" "$binary" \
        "$run_root" "$role pair $pair" paired "$pair" "$run_id"
    fi
    if $root_contract; then
      python3 "$python_tool" run-finish --contract "$run_root/run-contract.json" >/dev/null
    fi
  done
  baseline_runs+=(--baseline-run "$output/pair-$(printf '%02d' "$pair")/baseline")
  candidate_runs+=(--candidate-run "$output/pair-$(printf '%02d' "$pair")/candidate")
done

candidate_repeat_runs=()
for ((repeat=1; repeat<=candidate_repeats; repeat++)); do
  repeat_dir="$output/candidate-repeat-$(printf '%02d' "$repeat")"
  run_id=""
  root_contract=false
  if $is_render || ((geometry_count == 1)); then
    root_contract=true
    run_id=$(python3 "$python_tool" run-start \
      --scenarios "$scenarios" --scenario "$scenario" \
      --output "$repeat_dir/run-contract.json" \
      --kind candidate_repeat --role candidate --repeat-index "$repeat")
  fi
  if $is_render; then
    run_render candidate "$candidate_render" "$repeat_dir" "$run_id"
  else
    run_process_geometries candidate "$candidate_binary" "$repeat_dir" \
      "candidate diagnostic repeat $repeat" candidate_repeat "$repeat" "$run_id"
  fi
  if $root_contract; then
    python3 "$python_tool" run-finish --contract "$repeat_dir/run-contract.json" >/dev/null
  fi
  candidate_repeat_runs+=(--candidate-repeat-run "$repeat_dir")
done

compare_args=(
  compare
  --scenarios "$scenarios"
  --scenario "$scenario"
  --host-manifest "$output/host-manifest.json"
  "${baseline_runs[@]}"
  "${candidate_runs[@]}"
)
# Bash 3.2 on macOS treats expansion of a declared-but-empty array as an unbound variable under
# `set -u`. Only expand the diagnostic-repeat array when the scenario actually requested entries.
if ((candidate_repeats > 0)); then
  compare_args+=("${candidate_repeat_runs[@]}")
fi
compare_args+=(
  --output-json "$output/report.json"
  --output-markdown "$output/report.md"
)
compare_status=0
python3 "$python_tool" "${compare_args[@]}" || compare_status=$?
python3 "$python_tool" create-checksums \
  --root "$output" \
  --output "$output/SHA256SUMS" >/dev/null
echo "Transport verification: python3 $python_tool verify-checksums --root $output --output $output/SHA256SUMS"
exit "$compare_status"
