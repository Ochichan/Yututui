#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
  echo "error: $*" >&2
  exit 1
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

hash_stdin() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
}

ratatui_image_tree_digest() {
  {
    while IFS= read -r file; do
      printf '%s  %s\n' "$(hash_file "$file")" "${file#crates/ratatui-image/}"
    done < <(
      find crates/ratatui-image -type f ! -path 'crates/ratatui-image/target/*' | LC_ALL=C sort
    )
  } | hash_stdin
}

actual_tree_digest=$(ratatui_image_tree_digest)
if [ "${1:-}" = "--print-tree-digest" ]; then
  echo "$actual_tree_digest"
  exit 0
fi

# Rebless only after reviewing an intentional vendor-base or local-patch change; see PATCHES.md.
expected_tree_digest='ad192c53d3e82db10f57fa77c139452df5b7ac3a6e731fba2eea0da3427718c0'
test "$actual_tree_digest" = "$expected_tree_digest" \
  || fail "vendored ratatui-image tree drifted (expected $expected_tree_digest, got $actual_tree_digest)"

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

grep -Fq 'from_query_stdio_with_options_and_writer' crates/ratatui-image/src/picker.rs \
  || fail "bounded terminal-query writer seam is missing"

grep -Fq 'OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY' \
  crates/ratatui-image/src/picker.rs \
  || fail "independent nonblocking terminal-query descriptor is missing"

grep -Fq 'terminal capability query output timed out' crates/ratatui-image/src/picker.rs \
  || fail "terminal-query output deadline is missing"

grep -Fq 'bounded_query_writer_times_out_when_pty_reader_stalls' \
  crates/ratatui-image/src/picker.rs \
  || fail "terminal-query PTY deadline regression test is missing"

grep -Fq 'fn from_query_stdio_with_options_and_writer_until' \
  crates/ratatui-image/src/picker.rs \
  || fail "terminal-query shared absolute deadline is missing"

grep -Fq 'MAX_PENDING_INPUT_DRAIN_BYTES' crates/ratatui-image/src/picker.rs \
  || fail "terminal-query pending-input drain cap is missing"

grep -Fq 'PENDING_INPUT_DRAIN_RESERVE' crates/ratatui-image/src/picker.rs \
  || fail "terminal-query pending-input cleanup reserve is missing"

grep -Fq 'OptionalActions::Now, &termios' crates/ratatui-image/src/picker.rs \
  || fail "terminal-query raw mode can still wait on an output drain"

if grep -Fq '.args(["set", "-p", "allow-passthrough", "on"])' \
  crates/ratatui-image/src/picker.rs; then
  fail "terminal capability detection still launches an unbounded tmux mutation"
fi

test -f crates/ratatui-image/PATCHES.md \
  || fail "crates/ratatui-image/PATCHES.md is missing"

echo "ratatui-image patch invariants ok"
