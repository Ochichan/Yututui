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

crossterm_tree_digest() {
  {
    while IFS= read -r file; do
      printf '%s  %s\n' "$(hash_file "$file")" "${file#crates/crossterm/}"
    done < <(
      find crates/crossterm -type f ! -path 'crates/crossterm/target/*' | LC_ALL=C sort
    )
  } | hash_stdin
}

actual_tree_digest=$(crossterm_tree_digest)
if [ "${1:-}" = "--print-tree-digest" ]; then
  echo "$actual_tree_digest"
  exit 0
fi

# Rebless only after reviewing an intentional vendor-base or local-patch change; see PATCHES.md.
expected_tree_digest='e1f1873d3cf9645783d1407285f17054374e490f485e43c4678cec0f9f521e48'
test "$actual_tree_digest" = "$expected_tree_digest" \
  || fail "vendored crossterm tree drifted (expected $expected_tree_digest, got $actual_tree_digest)"

grep -Fq 'crossterm = { path = "crates/crossterm" }' Cargo.toml \
  || fail "Cargo.toml no longer patches crossterm to crates/crossterm"

grep -Fq 'version = "0.29.0"' crates/crossterm/Cargo.toml \
  || fail "vendored crossterm base version changed; update crates/crossterm/PATCHES.md"

grep -Fq 'd8b9f2e4c67f833b660cdb0a3523065869fb35570177239812ed4c905aeff87b' \
  crates/crossterm/PATCHES.md \
  || fail "vendored crossterm archive checksum is missing from PATCHES.md"

grep -Fq '36d95b26a26e64b0f8c12edfe11f410a6d56a812' \
  crates/crossterm/.cargo_vcs_info.json \
  || fail "vendored crossterm upstream revision changed"
grep -Fq '36d95b26a26e64b0f8c12edfe11f410a6d56a812' \
  crates/crossterm/PATCHES.md \
  || fail "vendored crossterm upstream revision is missing from PATCHES.md"

parser=crates/crossterm/src/event/sys/unix/parse.rs
grep -Fq 'yututui patch' "$parser" \
  || fail "vendored crossterm patch marker is missing"
grep -Fq 'parse_csi_win32_input' "$parser" \
  || fail "vendored crossterm win32-input parser is missing"
grep -Fq 'test_parse_csi_win32_input_distinguishes_ctrl_backspace_and_ctrl_h' "$parser" \
  || fail "vendored crossterm Ctrl+Backspace regression test is missing"

grep -Fq 'OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY' \
  crates/crossterm/src/event/source/unix/input.rs \
  || fail "independent nonblocking terminal-input descriptor is missing"

grep -Fq 'pub enum CursorPositionProbe' crates/crossterm/src/cursor.rs \
  || fail "typed cursor-position probe result is missing"

grep -Fq 'pub use sys::{position, probe_position_with};' crates/crossterm/src/cursor.rs \
  || fail "writer-injected cursor-position probe is not publicly exported"
grep -Fq 'pub fn probe_position_with' crates/crossterm/src/cursor/sys/unix.rs \
  || fail "Unix writer-injected cursor-position probe is missing"
grep -Fq 'pub fn probe_position_with' crates/crossterm/src/cursor/sys/windows.rs \
  || fail "Windows writer-injected cursor-position probe is missing"

grep -Fq 'supports_keyboard_enhancement_with_timeout' crates/crossterm/src/terminal.rs \
  || fail "bounded writer-injected keyboard probe is missing"

for source in \
  crates/crossterm/src/event/source/unix/mio.rs \
  crates/crossterm/src/event/source/unix/tty.rs; do
  grep -Fq 'incomplete_paste_yields_after_one_drain_budget' "$source" \
    || fail "64 KiB input-drain yield regression is missing from $source"
  grep -Fq 'drain_budget_survives_would_block_between_fragments' "$source" \
    || fail "fragmented-readiness drain-budget regression is missing from $source"
  grep -Fq 'queued_focus_continuation_is_drained_before_stale_escape_expires' "$source" \
    || fail "queued-prefix expiry ordering regression is missing from $source"
done

if grep -Rqs 'Command::new("tput")' crates/crossterm/src; then
  fail "terminal size handling still launches an unbounded tput subprocess"
fi

crossterm_lock_block=$(
  awk '
    /^\[\[package\]\]$/ {
      if (capture) exit
      block = $0 ORS
      next
    }
    {
      block = block $0 ORS
      if ($0 == "name = \"crossterm\"") capture = 1
    }
    END {
      if (capture) printf "%s", block
    }
  ' Cargo.lock
)

test -n "$crossterm_lock_block" \
  || fail "Cargo.lock has no crossterm package"
if grep -Eq '^(source|checksum) = ' <<<"$crossterm_lock_block"; then
  fail "Cargo.lock still resolves crossterm from crates.io instead of the local path"
fi

test -f crates/crossterm/PATCHES.md \
  || fail "crates/crossterm/PATCHES.md is missing"

echo "crossterm patch invariants ok"
