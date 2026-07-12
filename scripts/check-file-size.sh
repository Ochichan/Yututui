#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
BASELINE=scripts/size-baseline.tsv
GLOBAL_CAP=1500
GRACE=25
fail=0
while IFS= read -r f; do
  [ -f "$f" ] || continue
  lines=$(wc -l < "$f")
  cap=$(awk -v p="$f" -F'\t' '$2==p{print $1}' "$BASELINE" 2>/dev/null)
  cap=${cap:-$GLOBAL_CAP}
  if [ "$lines" -gt $((cap + GRACE)) ]; then
    echo "error: $f is $lines lines (cap $cap, +$GRACE grace). Split it, or re-bless: scripts/size-baseline.sh" >&2
    fail=1
  fi
done < <(git ls-files --cached --others --exclude-standard -- src | grep -E '\.rs$' | sort -u)
[ "$fail" = 0 ] && echo "file-size ratchet ok"
exit "$fail"
