#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required for the recovery-state budget check" >&2
  exit 2
fi

python3 scripts/check-recovery-state-budget.py --self-test
python3 scripts/check-recovery-state-budget.py \
  --root . \
  --manifest scripts/recovery-state-budget.tsv
