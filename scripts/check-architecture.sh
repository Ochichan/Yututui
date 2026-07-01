#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

actor_files=()
while IFS= read -r file; do actor_files+=("$file"); done < <(find src/player src/api src/ai src/remote -name '*.rs' -print)
actor_files+=(src/artwork.rs src/lyrics.rs src/download.rs src/resolver.rs)

if matches=$(grep -nE 'crate::app::Msg|use crate::app::.*Msg|UnboundedSender<Msg>|UnboundedReceiver<Msg>' "${actor_files[@]}" 2>/dev/null); then
  echo "error: leaf actors must emit domain events, not app::Msg:" >&2
  echo "$matches" >&2
  fail=1
fi

if matches=$(grep -nE 'unbounded_channel|UnboundedSender|UnboundedReceiver' src/download.rs src/resolver.rs 2>/dev/null); then
  echo "error: download/resolver queues must stay bounded/coalesced:" >&2
  echo "$matches" >&2
  fail=1
fi

grep -q 'pub enum RuntimeEvent' src/runtime.rs || {
  echo "error: RuntimeEvent adapter is missing" >&2
  fail=1
}

grep -q 'pub struct RuntimeHandles' src/runtime.rs || {
  echo "error: RuntimeHandles effect dispatcher is missing" >&2
  fail=1
}

grep -q 'pub enum QueuePolicy' src/util/backpressure.rs || {
  echo "error: backpressure policy type is missing" >&2
  fail=1
}

grep -q 'DOWNLOAD_QUEUE' src/util/backpressure.rs || {
  echo "error: download queue policy is missing" >&2
  fail=1
}

grep -q 'RESOLVER_QUEUE' src/util/backpressure.rs || {
  echo "error: resolver queue policy is missing" >&2
  fail=1
}

if [ "$fail" -ne 0 ]; then
  exit "$fail"
fi

echo "architecture invariants ok"
