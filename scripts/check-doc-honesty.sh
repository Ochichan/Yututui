#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
fail() { echo "error: $*" >&2; exit 1; }
[ -f ARCHITECTURE.md ] || fail "ARCHITECTURE.md is missing"
# The enforceable doc must NOT cite volatile line numbers (foo.rs:NNN).
if grep -nE '[A-Za-z0-9_/]+\.rs:[0-9]+' ARCHITECTURE.md; then
  fail "ARCHITECTURE.md cites line numbers (foo.rs:NNN). Cite module paths + type names instead."
fi
# Load-bearing symbols it names must still exist (presence gate).
grep -q 'pub enum Msg'         src/app/types.rs  || fail "Msg moved/renamed"
grep -q 'pub enum Cmd'         src/app/types.rs  || fail "Cmd moved/renamed"
grep -q 'enum PersistCmd'      src/app/types.rs  || fail "PersistCmd missing (M3 regressed)"
grep -q 'struct HitMap'        src/app/mouse.rs  || fail "HitMap missing (M1 regressed)"
grep -q 'struct RenderBridges' src/app/state.rs  || fail "RenderBridges moved/renamed"
grep -q 'struct App'           src/app/mod.rs    || fail "App moved/renamed"
if grep -nEi 'production[- ]ready|works everywhere|all terminals|stable API|full Windows support' \
  README.md README.ko.md README.ja.md docs/index.html; then
  fail "public docs contain unsupported beta/terminal overclaims"
fi
echo "doc honesty ok"
