#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "error: $*" >&2; exit 1; }
if [ -f ARCHITECTURE.md ]; then
  # If present, the enforceable doc must NOT cite volatile line numbers (foo.rs:NNN).
  if grep -nE '[A-Za-z0-9_/]+\.rs:[0-9]+' ARCHITECTURE.md; then
    fail "ARCHITECTURE.md cites line numbers (foo.rs:NNN). Cite module paths + type names instead."
  fi
fi
# Load-bearing architecture symbols must still exist (presence gate).
grep -q 'pub enum Msg'         src/app/types.rs  || fail "Msg moved/renamed"
grep -q 'pub enum Cmd'         src/app/types.rs  || fail "Cmd moved/renamed"
grep -q 'enum PersistCmd'      src/app/types.rs  || fail "PersistCmd missing (M3 regressed)"
grep -q 'struct HitMap'        src/app/mouse.rs  || fail "HitMap missing (M1 regressed)"
grep -q 'struct RenderBridges' src/app/state.rs  || fail "RenderBridges moved/renamed"
grep -q 'struct App'           src/app/mod.rs    || fail "App moved/renamed"
public_docs=()
for doc in README.md README.ko.md README.ja.md docs/index.html; do
  [ -f "$doc" ] && public_docs+=("$doc")
done

# While the crate is publish = false, no user-facing text may suggest the bare
# crates.io install form (`cargo install yututui`) — it fails; only --git works.
if grep -q '^publish = false' Cargo.toml &&
  grep -rnE 'cargo install yututui([^-]|$)' src "${public_docs[@]}"; then
  fail "crates.io install command suggested while Cargo.toml has publish = false (use --git)"
fi

if [ "${#public_docs[@]}" -gt 0 ] &&
  grep -nEi 'production[- ]ready|works everywhere|all terminals|stable API|full Windows support' \
    "${public_docs[@]}"; then
  fail "public docs contain unsupported beta/terminal overclaims"
fi
echo "doc honesty ok"
