#!/usr/bin/env bash
# Re-bless the module-size baseline consumed by scripts/check-file-size.sh.
#
# Rewrites scripts/size-baseline.tsv with one "<lines>\t<path>" row for every tracked
# src/**/*.rs file whose line count exceeds the global soft cap (1500). These are the
# legit giants (and any deliberately-large module) the ratchet grandfathers; a file not
# listed here is held to the 1500 (+grace) global cap. Run this only after an intentional
# split shrinks a file (to lower its pin) or when a new large module is knowingly accepted;
# then commit the regenerated TSV.
set -euo pipefail
cd "$(dirname "$0")/.."
GLOBAL_CAP=1500
{
  while IFS= read -r f; do
    [ -f "$f" ] || continue
    lines=$(wc -l < "$f" | tr -d '[:space:]')
    if [ "$lines" -gt "$GLOBAL_CAP" ]; then
      printf '%s\t%s\n' "$lines" "$f"
    fi
  done < <(git ls-files -- src | grep -E '\.rs$')
} | sort -rn > scripts/size-baseline.tsv
echo "wrote scripts/size-baseline.tsv ($(wc -l < scripts/size-baseline.tsv | tr -d '[:space:]') entries)"
