#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

actor_files=()
while IFS= read -r file; do actor_files+=("$file"); done < <(find src/player src/api src/ai src/remote -name '*.rs' -print)
actor_files+=(src/artwork.rs src/lyrics.rs src/download.rs src/resolver.rs)

# C1: the boundary forbids not just app::Msg but the whole reducer message/command surface —
# top-level Msg/Cmd and the M3 sub-enums (PlayerMsg/AiMsg/StreamingMsg/…, PersistCmd/…). Leaf
# actors must stay behind the RuntimeEvent seam and never name a reducer message/command type.
if matches=$(grep -nE 'crate::app::(Msg|Cmd|[A-Za-z]+Msg|[A-Za-z]+Cmd)|use crate::app::.*Msg|UnboundedSender<Msg>|UnboundedReceiver<Msg>' "${actor_files[@]}" 2>/dev/null); then
  echo "error: leaf actors must emit domain events, not app::Msg/Cmd:" >&2
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

# C2: App keeps its flat fields on a reviewed allowlist. Extract the struct's field idents and
# diff against scripts/app-fields.allow; a new flat field must either join a per-domain sub-struct
# or be added to the allowlist on purpose (re-bless by re-running the same awk — see ARCHITECTURE.md).
tmp=$(mktemp)
awk '/^pub struct App \{/{f=1;next} f&&/^\}/{exit}
     f&&/^ *(pub(\([^)]*\))? +)?[a-z_]+ *:/ {gsub(/^ *(pub(\([^)]*\))? +)?/,""); sub(/ *:.*/,""); print}' \
  src/app/mod.rs | sort -u > "$tmp"
if extra=$(comm -13 scripts/app-fields.allow "$tmp"); [ -n "$extra" ]; then
  echo "error: new flat App field(s) not in scripts/app-fields.allow — group them into a sub-struct or add intentionally:" >&2
  echo "$extra" >&2; fail=1
fi
rm -f "$tmp"

# C3: the Msg/Cmd wrapper enums stay small and the M3 sub-enums stay present, so a large domain
# can't be re-flattened back into the top-level enums. Ceilings sit just above the current counts.
count_variants() { awk -v e="$1" '$0 ~ "^pub enum "e" \\{"{f=1;next} f&&/^\}/{exit} f&&/^    [A-Z]/{c++} END{print c+0}' src/app/types.rs; }
[ "$(count_variants Msg)" -le 45 ] || { echo "error: enum Msg exceeds 45 wrappers — new flat cross-domain variant? bucket it." >&2; fail=1; }
[ "$(count_variants Cmd)" -le 32 ] || { echo "error: enum Cmd exceeds 32 wrappers." >&2; fail=1; }
for e in PlayerMsg AiMsg StreamingMsg PersistCmd; do
  grep -q "enum $e" src/app/*.rs || { echo "error: sub-enum $e missing (M3 regressed)" >&2; fail=1; }
done

if [ "$fail" -ne 0 ]; then
  exit "$fail"
fi

echo "architecture invariants ok"
