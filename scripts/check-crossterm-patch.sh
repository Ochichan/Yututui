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
expected_tree_digest='03d5709a34cca0364907b63b3e59327a51b72540d2762033cb7e930e00f26be8'
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
