#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

ledger="docs/invariant-ledger.md"
[ -f "$ledger" ] || {
  echo "error: invariant ledger missing: $ledger" >&2
  exit 1
}

ids=$(find src scripts docs -type f \
  ! -path 'docs/event-policy-reducer-invariants-terminal-beta-plan.md' \
  ! -path 'docs/invariant-ledger.md' \
  -exec grep -hoE 'INVARIANT\([A-Z0-9-]+\)' {} + \
  | sed -E 's/.*INVARIANT\(([A-Z0-9-]+)\).*/\1/' \
  | sort -u)

for id in $ids; do
  if ! grep -qE "^\| \`${id}\` \|" "$ledger"; then
    echo "error: invariant tag $id is missing from $ledger" >&2
    exit 1
  fi
done

awk -F'|' '
  index($0, "| `") == 1 {
    id=$2; gsub(/^[[:space:]]+|[[:space:]]+$/, "", id)
    enforcement=$5; gsub(/^[[:space:]]+|[[:space:]]+$/, "", enforcement)
    status=$8; gsub(/^[[:space:]]+|[[:space:]]+$/, "", status)
    if (status == "automated" && (enforcement == "" || enforcement == "TBD" || enforcement == "None")) {
      print "error: " id " is automated but has no current enforcement" > "/dev/stderr"
      exit 1
    }
  }
' "$ledger"

echo "invariant ledger ok"
