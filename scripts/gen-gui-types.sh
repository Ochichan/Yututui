#!/usr/bin/env bash
# Regenerate the ts-rs protocol bindings the GUI is typed against (docs/gui/05 §6).
# The generated files under gui/src/generated/ are committed and drift-gated by
# scripts/check-gui-types.sh — npm must never need cargo to build.
set -euo pipefail
cd "$(dirname "$0")/.."

# ts-rs resolves each type's `export_to` relative to TS_RS_EXPORT_DIR (default ./bindings);
# point it at the repo root so `export_to = "gui/src/generated/protocol/"` lands in-tree.
TS_RS_EXPORT_DIR="$PWD" cargo test --features ts-export --lib export_bindings "$@"
