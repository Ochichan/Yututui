#!/usr/bin/env bash
# Re-bless the module-size baseline consumed by scripts/check-file-size.sh.
#
# Rewrites scripts/size-baseline.tsv without ever raising an existing pin. Files above the
# global soft cap (1500) receive a pin when first accepted; existing sub-global pins remain so
# an intentional split cannot silently regrow on the next regeneration.
set -euo pipefail
cd "$(dirname "$0")/.."
GLOBAL_CAP=1500
BASELINE=scripts/size-baseline.tsv
previous=$(mktemp)
next=$(mktemp)
trap 'rm -f "$previous" "$next"' EXIT
cp "$BASELINE" "$previous"
{
  while IFS= read -r f; do
    [ -f "$f" ] || continue
    lines=$(wc -l < "$f" | tr -d '[:space:]')
    old=$(awk -v p="$f" -F'\t' '$2==p { print $1; exit }' "$previous")
    if [ -n "$old" ]; then
      [ "$old" -lt "$lines" ] && lines=$old
      printf '%s\t%s\n' "$lines" "$f"
    elif [ "$lines" -gt "$GLOBAL_CAP" ]; then
      printf '%s\t%s\n' "$lines" "$f"
    fi
  done < <(git ls-files --cached --others --exclude-standard -- src | grep -E '\.rs$' | sort -u)
} | sort -rn > "$next"
mv "$next" "$BASELINE"
echo "wrote $BASELINE ($(wc -l < "$BASELINE" | tr -d '[:space:]') entries; existing pins never increased)"
