#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

fields=(
  queue playback status search library_ui streaming ai overlays art downloads config
)

printf 'file\tfield\twrites\treferences\n'
while IFS= read -r file; do
  for field in "${fields[@]}"; do
    writes=$(rg -n --fixed-strings "self.${field}" "$file" | rg -c '(\.|)=|\.push|\.clear|\.take|\.insert|\.remove|\.retain|\.sort|\.swap|\.cycle|\.set|\.load|\.replace' || true)
    writes=${writes:-0}
    refs=$( (rg -n --fixed-strings "self.${field}" "$file" || true) | wc -l | tr -d ' ')
    refs=${refs:-0}
    if [ "$writes" != "0" ] || [ "$refs" != "0" ]; then
      printf '%s\t%s\t%s\t%s\n' "$file" "$field" "$writes" "$refs"
    fi
  done
done < <(
  {
    rg --files src/app | rg 'src/app/(.*_reducer|ai_reducer|library|media_reducer|now_playing_reducer|player|playlists_reducer|recorder_reducer|remote_reducer|scrobble_reducer|search|settings_reducer|streaming_reducer)\.rs$'
  } | sort
)
true
