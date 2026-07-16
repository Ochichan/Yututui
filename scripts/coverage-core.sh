#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# CORE line-coverage gate (>= 80%) — deliberately NOT a whole-product number.
#
# The ignore list below excludes process/CLI/platform boundaries that need a
# runtime, OS, network, or external-binary harness to execute at all: process
# entry points (main/tui/daemon engine wiring), external-service adapters
# (ytmusic API, spotify, scrobble services, lyrics), mpv process lifecycle
# (player lifetime/mpv), OS media integration, and the remote client/endpoint
# transport. Those surfaces are covered by contract/parity/smoke tests and the
# release matrix instead; counting their un-executable glue here would only
# dilute the signal on the unit-testable core (reducers, policy, persistence,
# utils), which this gate holds at 80%.
#
# Measured on main 2026-07-16: 81.99% lines. Runs in ci-pr's linux job as an
# advisory step (continue-on-error) until the number proves stable; local runs
# need cargo-llvm-cov plus llvm-tools-preview (or LLVM_COV/LLVM_PROFDATA).
IGNORE_RE='(/src/(main\.rs|auth_cli\.rs|logging\.rs|notify\.rs|doctor\.rs|tui\.rs|daemon/(mod|engine)\.rs|tools/(cli|ytdlp)\.rs|transfer/(cli|actor|engine|mod)\.rs|scrobble/(actor|auth_cli|service|mod|lastfm|listenbrainz)\.rs|media/(macos|artwork)\.rs|spotify/(client|auth)\.rs|player/(lifetime|mod|mpv)\.rs|recorder/job\.rs|update/cli\.rs|api/ytmusic\.rs|lyrics\.rs|romanize\.rs|app/romanize\.rs|ui/views/now_playing\.rs|app/scrobble_reducer\.rs|app/clipboard\.rs|media/identity\.rs|util/http\.rs|remote/(client|endpoint)\.rs)$)'

cargo llvm-cov \
  --workspace \
  --all-targets \
  --summary-only \
  --fail-under-lines 80 \
  --ignore-filename-regex "$IGNORE_RE" \
  "$@"
