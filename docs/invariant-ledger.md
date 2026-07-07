# Invariant Ledger

Status: initial ledger, updated 2026-07-07. Durable invariant comments use:

```rust
// INVARIANT(ID): short claim.
```

Only comments that assert a long-lived rule should be tagged. Ordinary
explanatory comments should stay untagged.

| ID | Claim | Source Location | Current Enforcement | Target Enforcement | Owner | Status |
|---|---|---|---|---|---|---|
| `EVT-POLICY-001` | Every owner-loop event has an explicit delivery policy. | `src/runtime.rs`, `src/daemon/mod.rs` | `runtime_event_policy_covers_representative_events`, `daemon_event_policy_covers_representative_events` | Exhaustive policy mapping and saturation tests. | runtime | automated |
| `EVT-REMOTE-001` | Remote commands rejected by a saturated owner lane return `server_busy`, not a timeout. | `src/remote/server.rs` | `one_shot_reports_server_busy_when_owner_rejects` | Owner-lane saturation tests for TUI and daemon. | remote | automated |
| `EVT-REPEAT-001` | Key releases and non-navigation repeats are filtered before reducer dispatch. | `src/event.rs` | `event` module tests | Keep tests; add matrix smoke for IME. | input | automated |
| `PLAY-EPOCH-001` | Every playback position discontinuity bumps `position_epoch` through a named helper. | `src/app/mod.rs`, `src/app/player.rs`, `src/app/media_reducer.rs`, `src/daemon/engine.rs` | `check-app-boundaries.sh`, app tests, daemon parity tests | Direct writes forbidden outside helper definitions and test fixtures. | playback | automated |
| `MODE-REPEAT-001` | Music streaming and repeat are mutually exclusive. | `src/app/player.rs`, `src/daemon/engine.rs` | app tests and `src/daemon/parity_tests.rs` | Entry-path tests for key, settings, remote, media, and AI paths. | playback | automated |
| `ART-MASK-001` | `art_overlay_mask` bits are named, unique, and within `u16`. | `src/app/artwork.rs` | `art_overlay_mask_tracks_each_popup_independently`, `art_overlay_mask_bits_are_unique_and_fit_u16` | Boundary script keeps bit constants centralized. | artwork | automated |
| `REMOTE-V7-001` | v7 one-shot wire shape parses forever. | `src/remote/proto/freeze.rs` | freeze tests | Keep as canonical protocol freeze. | remote | automated |
| `PUBLISH-QUEUE-001` | Cursor moves emit player snapshots, not queue snapshots. | `src/remote/publish.rs` | publisher tests | Keep as canonical. | remote | automated |
| `ZOOM-ART-001` | Native pixel art hides while text zoom is active. | `src/app/tests.rs`, `src/zoom.rs` | app zoom/art tests | Keep linked from comments when touched. | ui | automated |
| `CJK-WIDTH-001` | CJK truncation never splits wide characters. | `src/ui/text.rs` | text module tests | Add terminal matrix smoke evidence. | ui | automated |

## Enforcement Rules

- `scripts/check-invariant-ledger.sh` checks every `INVARIANT(ID)` tag against
  this ledger.
- Entries with `Status` set to `automated` must name a Rust test, shell gate, or
  parity test in `Current Enforcement`.
- Entries that cannot be automated must name a manual runbook and should not be
  used to justify broad release claims.
