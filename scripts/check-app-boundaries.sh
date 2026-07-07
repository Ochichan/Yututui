#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

fail() {
  echo "error: $*" >&2
  exit 1
}

if [ "${1:-}" = "--self-test" ]; then
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  mkdir -p "$tmp/src/app"
  cat > "$tmp/src/app/bad.rs" <<'BAD'
fn bad(app: &mut App) {
    app.playback.position_epoch = app.playback.position_epoch.wrapping_add(1);
}
BAD
  if grep -nE 'position_epoch[[:space:]]*=|position_epoch\.wrapping_add\(1\)' "$tmp/src/app/bad.rs" >/dev/null; then
    echo "app boundary self-test ok"
    exit 0
  fi
  fail "self-test failed to detect planted position_epoch write"
fi

# INVARIANT(PLAY-EPOCH-001): position discontinuities must go through named helpers.
position_hits=$(grep -RInE 'position_epoch[[:space:]]*=|position_epoch\.wrapping_add\(1\)' src/app src/daemon src/media \
  | grep -Ev 'src/app/mod.rs:[0-9]+:.*fn bump_position_epoch' \
  | grep -Ev 'src/app/mod.rs:[0-9]+:.*self\.playback\.position_epoch = self\.playback\.position_epoch\.wrapping_add\(1\)' \
  | grep -Ev 'src/daemon/engine.rs:[0-9]+:.*fn bump_position_epoch' \
  | grep -Ev 'src/daemon/engine.rs:[0-9]+:.*self\.playback\.position_epoch = self\.playback\.position_epoch\.wrapping_add\(1\)' \
  | grep -Ev 'src/daemon/parity_tests.rs:[0-9]+:.*position_epoch = 0' \
  | grep -Ev 'src/media/mod.rs:[0-9]+:.*position_epoch \+=' \
  || true)
if [ -n "$position_hits" ]; then
  printf '%s\n' "$position_hits" >&2
  fail "direct position_epoch writes are only allowed in App/daemon helper definitions and explicit test fixtures"
fi

# INVARIANT(ART-MASK-001): overlay mask bit definitions stay centralized.
mask_hits=$(grep -RInE '1u16[[:space:]]*<<|1[[:space:]]*<<[[:space:]]*(0|1|2|3|4|5|6|7|8|9|10|11|12|13|14|15)' src/app \
  | grep -E 'overlay|popup|art_mask|art_overlay_mask' \
  | grep -Ev 'src/app/artwork.rs' \
  | grep -Ev 'src/app/tests.rs' \
  || true)
if [ -n "$mask_hits" ]; then
  printf '%s\n' "$mask_hits" >&2
  fail "art overlay mask bit constants must stay centralized in src/app/artwork.rs"
fi

echo "app boundaries ok"
