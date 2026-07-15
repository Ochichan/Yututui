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

# The player is a leaf transport. URL policy and delivery coordination must live in neutral
# modules so the transport cannot reach upward into provider API or recorder orchestration.
if matches=$(grep -RInE 'crate::(api|recorder)([^[:alnum:]_]|$)' src/player --include='*.rs' 2>/dev/null); then
  echo "error: player transport must not depend on api/recorder orchestration modules:" >&2
  echo "$matches" >&2
  fail=1
fi

# Media process lifetime is a tree boundary, not a direct-child handle. The long-lived audio mpv
# and detached video overlay can both launch yt-dlp/ffmpeg helpers; Unix process groups and Windows
# Job Objects must remain attached to their owners on close, cancellation, and drop.
grep -Fq 'pub proc: Option<crate::util::process_tree::OwnedProcessTree>' src/app/state.rs || {
  echo "error: Video must own an OwnedProcessTree, not a raw child" >&2
  fail=1
}

grep -Fq 'child_tree: ChildTreeGuard' src/player/mod.rs || {
  echo "error: Mpv must own a ChildTreeGuard" >&2
  fail=1
}

grep -Fq 'matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp)' \
  src/util/process.rs || {
  echo "error: Media children must stay in isolated Unix process groups" >&2
  fail=1
}

grep -Fq 'matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp)' \
  src/util/process_guard.rs || {
  echo "error: Media child trees must stay armed for Unix groups / Windows Job Objects" >&2
  fail=1
}

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

# C1b: owner delivery stays centralized and migrated actor inboxes cannot silently regress to
# unbounded memory growth. Inline test modules intentionally use raw channels for saturation
# fixtures, so inspect through the actual `#[cfg(test)] mod tests` boundary. A standalone
# test-only helper must not truncate later production code (notably the MPRIS implementation).
for file in \
  src/ai/mod.rs \
  src/artwork.rs \
  src/lyrics.rs \
  src/transfer/actor.rs \
  src/scrobble/actor.rs \
  src/scrobble/mod.rs \
  src/desktop/gateway.rs \
  src/player/mod.rs \
  src/player/video.rs \
  src/runtime/player_delivery.rs \
  src/media/artwork.rs \
  src/media/mpris.rs \
  src/media/smtc.rs \
  src/terminal_runtime/runner.rs; do
  production=$(awk '
    pending_cfg && /^mod tests[[:space:]]*\{/ { exit }
    pending_cfg { print "#[cfg(test)]"; pending_cfg=0 }
    /^#\[cfg\(test\)\]$/ { pending_cfg=1; next }
    { print }
    END { if (pending_cfg) print "#[cfg(test)]" }
  ' "$file")
  if matches=$(grep -nE 'unbounded_channel|UnboundedSender|UnboundedReceiver' <<<"$production"); then
    echo "error: migrated production actor inbox must stay bounded ($file):" >&2
    echo "$matches" >&2
    fail=1
  fi
  if matches=$(grep -nE 'let _ = .*try_send' <<<"$production"); then
    echo "error: migrated production delivery result must be observed ($file):" >&2
    echo "$matches" >&2
    fail=1
  fi
done

grep -q 'pub fn fetch(&self, video_id: String, source: ArtSource) -> DeliveryResult' \
  src/artwork.rs || {
  echo "error: terminal artwork admission must return a typed delivery result" >&2
  fail=1
}

for policy in TRANSFER_CONTROL_QUEUE SCROBBLE_CONTROL_QUEUE; do
  grep -q "$policy" src/util/backpressure.rs || {
    echo "error: reserved control queue policy is missing ($policy)" >&2
    fail=1
  }
done

grep -q 'bounded_channel(TRANSFER_CONTROL_QUEUE)' src/transfer/actor.rs || {
  echo "error: transfer cancellation must use its reserved control queue" >&2
  fail=1
}

grep -q 'bounded_channel(SCROBBLE_CONTROL_QUEUE)' src/scrobble/actor.rs || {
  echo "error: scrobble shutdown must use its reserved control queue" >&2
  fail=1
}

grep -q 'pub fn request(&self, key: String, query: ArtQuery) -> DeliveryResult' \
  src/media/artwork.rs || {
  echo "error: media artwork admission must return a typed delivery result" >&2
  fail=1
}

# Cache creation and eviction are durable mutations too. Keep them behind the same late-recovery
# revoke as store writes; raw filesystem calls would re-open a race between a successful artwork
# write and the following prune pass.
media_artwork_production=$(awk '
  pending_cfg && /^mod tests[[:space:]]*\{/ { exit }
  pending_cfg { print "#[cfg(test)]"; pending_cfg=0 }
  /^#\[cfg\(test\)\]$/ { pending_cfg=1; next }
  { print }
' src/media/artwork.rs)
if matches=$(grep -nE 'std::fs::(create_dir_all|remove_file)' <<<"$media_artwork_production"); then
  echo "error: media artwork cache mutation must use guarded safe_fs primitives:" >&2
  echo "$matches" >&2
  fail=1
fi

# Runtime persistence paths must resolve through the same override-aware roots protected by the
# process writer lease. Direct ProjectDirs lookups here can silently write outside YTM_*_DIR and
# let a read-only secondary or a second writer mutate an unlocked root.
for file in \
  src/terminal_runtime/runner.rs \
  src/terminal_runtime/art.rs \
  src/daemon/mod.rs \
  src/local/index.rs; do
  if matches=$(grep -n 'directories::ProjectDirs' "$file"); then
    echo "error: runtime persistence path bypasses crate::paths ($file):" >&2
    echo "$matches" >&2
    fail=1
  fi
done

for file in src/media/mpris.rs src/media/smtc.rs; do
  grep -q 'LatestMediaSender' "$file" || {
    echo "error: platform media snapshots must use the shared bounded delivery contract ($file)" >&2
    fail=1
  }
done

if grep -q 'fn store_latest' src/media/mpris.rs src/media/smtc.rs; then
  echo "error: platform media delivery/coalescing logic must not be duplicated" >&2
  fail=1
fi

for file in src/runtime.rs src/runtime/ingress.rs src/daemon/mod.rs src/daemon/events.rs; do
  if matches=$(grep -nE 'must_deliver_overflow|emit_(daemon_)?direct|emit_(daemon_)?coalesced' "$file"); then
    echo "error: owner event delivery bypasses the shared ingress ($file):" >&2
    echo "$matches" >&2
    fail=1
  fi
done

grep -q 'OwnerEventIngress' src/runtime/ingress.rs || {
  echo "error: terminal runtime must use the shared owner-event ingress" >&2
  fail=1
}

# The daemon's event taxonomy/ingress lives in events.rs (extracted from mod.rs).
grep -q 'OwnerEventIngress' src/daemon/events.rs || {
  echo "error: daemon runtime must use the shared owner-event ingress" >&2
  fail=1
}

# OS media command handlers may run synchronously on the owner thread (notably the macOS run-loop
# pump). They must use non-blocking typed admission and report rejection to the platform; a
# must-deliver/blocking callback path can deadlock the owner against its own saturated ingress.
grep -q 'pub type CommandSink = Arc<dyn Fn(MediaCommand) -> DeliveryResult' src/media/mod.rs || {
  echo "error: media command sinks must report typed delivery outcomes" >&2
  fail=1
}

if grep -qE 'emit_callback_observed\(&media_cmd|record_daemon_event\(&media_cmd' \
  src/terminal_runtime/runner.rs src/daemon/mod.rs; then
  echo "error: owner-reentrant media commands must not use blocking callback delivery" >&2
  fail=1
fi

grep -q 'Builder::new_multi_thread()' src/daemon/mod.rs || {
  echo "error: daemon callback fallback requires a multi-thread Tokio runtime" >&2
  fail=1
}

if grep -q 'biased;' src/player/ipc.rs; then
  echo "error: mpv IPC must fairly poll its sole command lane" >&2
  fail=1
fi

if [ -e src/runtime/must_deliver.rs ] || [ -e src/daemon/must_deliver.rs ]; then
  echo "error: duplicated must-deliver overflow modules must not return" >&2
  fail=1
fi

# C1c: reducer-owned player state may change only through typed admission controls. Transport
# recovery carries its complete ordered restore batch in `PlayerControl::Restart`; no raw Cmd
# variant or constructor may reopen a partial-admission path.
raw_player=$(grep -RInE 'Cmd::Player([^A-Za-z0-9_]|$)' src --include='*.rs' || true)
if [ -n "$raw_player" ]; then
  echo "error: raw player effects bypass typed admission controls:" >&2
  echo "$raw_player" >&2
  fail=1
fi

if grep -nE '^[[:space:]]*Player\(PlayerCmd\),' src/app/types.rs; then
  echo "error: Cmd must not expose a raw Player(PlayerCmd) variant" >&2
  fail=1
fi

grep -q 'restore: Vec<PlayerCmd>' src/app/player_intent.rs || {
  echo "error: transport recovery must carry one typed ordered restore batch" >&2
  fail=1
}

# C2: App keeps its flat fields on a reviewed allowlist. Extract the struct's field idents and
# diff against scripts/app-fields.allow; a new flat field must either join a per-domain sub-struct
# or be added to the allowlist on purpose in scripts/app-fields.allow.
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
[ "$(count_variants Cmd)" -le 33 ] || { echo "error: enum Cmd exceeds 33 wrappers." >&2; fail=1; }
for e in PlayerMsg AiMsg StreamingMsg PersistCmd; do
  grep -q "enum $e" src/app/*.rs || { echo "error: sub-enum $e missing (M3 regressed)" >&2; fail=1; }
done

# Recovery transport ownership crosses asynchronous load, correlation, and seek boundaries.
# Keep the known multi-field encodings collapsed into the typed state machines audited by the
# recovery-state budget; reintroducing one of these fields recreates unreachable combinations.
if matches=$(grep -nE \
  '^[[:space:]]*(restore_transport|force_ram_only|exact_completed|latest_seek_position|source_recovery)[[:space:]]*:[[:space:]]*(bool|Option<)' \
  src/player/ipc.rs src/player/ipc/*.rs src/daemon/engine/transport.rs 2>/dev/null); then
  echo "error: recovery control state must stay in typed variants, not bool/Option flags:" >&2
  echo "$matches" >&2
  fail=1
fi

if matches=$(grep -nE \
  '^[[:space:]]*resume[[:space:]]*:[[:space:]]*Option<.*LoadWithResume' \
  src/player/ipc/command_queue.rs 2>/dev/null); then
  echo "error: staged load resume ownership must use ResumeLoad:" >&2
  echo "$matches" >&2
  fail=1
fi

grep -Fq 'resume: ResumeCoordinator' src/player/ipc.rs || {
  echo "error: player IPC resume lifecycle must use ResumeCoordinator" >&2
  fail=1
}

grep -Fq 'resume: resume::ResumeLoad' src/player/ipc/command_queue.rs || {
  echo "error: validated load resume ownership must use ResumeLoad" >&2
  fail=1
}

grep -Fq 'mode: TransportRecoveryMode' src/daemon/engine/transport.rs || {
  echo "error: daemon transport recovery must use TransportRecoveryMode" >&2
  fail=1
}

if [ "$fail" -ne 0 ]; then
  exit "$fail"
fi

bash scripts/check-recovery-state-budget.sh
bash scripts/check-app-boundaries.sh

echo "architecture invariants ok"
