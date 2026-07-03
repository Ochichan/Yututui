#!/usr/bin/env bash
# CI drift gate (docs/gui/04 §5): regenerating the ts-rs bindings must produce no diff.
# A protocol change that isn't reflected in committed TS fails here.
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/gen-gui-types.sh >/dev/null

if ! git diff --exit-code -- gui/src/generated/ >/dev/null 2>&1; then
  echo "error: gui/src/generated/ is out of date." >&2
  echo "       run scripts/gen-gui-types.sh and commit the result." >&2
  git --no-pager diff --stat -- gui/src/generated/ >&2 || true
  exit 1
fi
echo "gui generated types up to date"
