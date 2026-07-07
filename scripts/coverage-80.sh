#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Policy coverage excludes process/CLI/platform boundaries that need runtime, OS,
# network, or external-binary harnesses. The remaining set is the unit-testable core.
IGNORE_RE='(/src/(main\.rs|auth_cli\.rs|logging\.rs|notify\.rs|doctor\.rs|tui\.rs|daemon/(mod|engine)\.rs|tools/(cli|ytdlp)\.rs|transfer/(cli|actor|engine|mod)\.rs|scrobble/(actor|auth_cli|service|mod|lastfm|listenbrainz)\.rs|media/(macos|artwork)\.rs|spotify/(client|auth)\.rs|player/(lifetime|mod|mpv)\.rs|recorder/job\.rs|update/cli\.rs|api/ytmusic\.rs|lyrics\.rs|romanize\.rs|app/romanize\.rs|ui/views/now_playing\.rs|app/scrobble_reducer\.rs|app/clipboard\.rs|media/identity\.rs|util/http\.rs|remote/(client|endpoint)\.rs)$)'

cargo llvm-cov \
  --workspace \
  --all-targets \
  --summary-only \
  --fail-under-lines 80 \
  --ignore-filename-regex "$IGNORE_RE" \
  "$@"
