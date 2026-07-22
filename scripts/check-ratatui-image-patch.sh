#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
  echo "error: $*" >&2
  exit 1
}

grep -Fq 'ratatui-image = { path = "crates/ratatui-image" }' Cargo.toml \
  || fail "Cargo.toml no longer patches ratatui-image to crates/ratatui-image"

grep -Fq 'version = "11.0.6"' crates/ratatui-image/Cargo.toml \
  || fail "vendored ratatui-image base version changed; update crates/ratatui-image/PATCHES.md"

grep -Rqs 'yututui patch' crates/ratatui-image/src \
  || fail "vendored ratatui-image patch markers are missing"

grep -Rqs 'next_redraw_tag' crates/ratatui-image/src/protocol* \
  || fail "Sixel/iTerm2 redraw-tag patch marker is missing"

grep -Rqs 'mark_rows_for_redraw' crates/ratatui-image/src/protocol/kitty.rs crates/ratatui-image/src/thread.rs \
  || fail "Kitty redraw damage patch marker is missing"

grep -Rqs 'new_with_z_index' crates/ratatui-image/src/protocol/kitty.rs crates/ratatui-image/src/picker.rs \
  || fail "Kitty z-index patch marker is missing"

grep -Fq 'KONSOLE_SIXEL_TUI_MIN_VERSION' crates/ratatui-image/src/picker.rs \
  || fail "Konsole Sixel version-gate patch marker is missing"

grep -Fq 'require_reported_cell_size_for_sixel' crates/ratatui-image/src/picker.rs \
  || fail "Konsole Sixel cell-size guard is missing"

grep -Fq 'pub enum RenderScale' crates/ratatui-image/src/lib.rs \
  || fail "native RenderScale patch is missing"

grep -Fq 'transmit_direct' crates/ratatui-image/src/protocol/kitty.rs \
  || fail "zoomed Kitty direct-placement patch is missing"

grep -Fq 'render_scale.clear_size' crates/ratatui-image/src/protocol/sixel.rs \
  || fail "DECDHL Sixel clear-geometry patch is missing"

grep -Fq 'NEXT_RESIZE_REQUEST_ID' crates/ratatui-image/src/thread.rs \
  || fail "threaded resize global-generation patch is missing"

test -f crates/ratatui-image/PATCHES.md \
  || fail "crates/ratatui-image/PATCHES.md is missing"

echo "ratatui-image patch invariants ok"
