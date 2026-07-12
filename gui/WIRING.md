# The Patch Bay — wiring handoff for follow-up agents

The frontend under `gui/src/` is built **to the finished spec** (docs/gui/05–07): every
screen, tab, overlay, and control exists and is styled. What separates it from "done" is
a known list of **wires** — protocol connections to the core. This file is the contract
for continuing that work. Read it before touching anything.

## The three tiers

### 1. Wired — live today, do not stub these

| Surface                            | How it's wired                                                                                                                                                                                                                                                                                                                            |
| ---------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `player` / `queue` topic rendering | `playback.svelte.ts` / `queue.svelte.ts` consume `player_snapshot` / `queue_snapshot` pushes (the B0 wire), incl. interpolation with the ported mini-player constants (`lib/time.ts` — do not re-derive)                                                                                                                                  |
| Transport commands                 | Exactly the surface `src/remote/proto/command.rs` accepts today: `toggle_pause` `next` `prev` `play` `enqueue` `seek_to` `seek_back` `seek_forward` `set_volume` `vol_up` `vol_down` `toggle_shuffle` `cycle_repeat` `queue_play` `queue_remove` `queue_play_if_revision` `queue_remove_if_revision` `streaming` `run_search` `play_tracks` `enqueue_tracks` `set_setting` `apply` `set_gemini_key` `reset_all_settings` `export_personal_data` `status` `resume_session` `quit` — every mutation has a correlated acknowledgement, a page-scoped `request_id`, a bounded timeout, and a visible busy/offline/error outcome. **Anything not in that enum is a Deferred v8 command (next section), not wired.** |
| Local themes                       | `lib/theme/local.ts` + `ThemeStore.applyLocal` — 5 GUI-owned skins (incl. Crimson Mono and Ember Wine), applied live, persisted in localStorage                                                                                                                                                                                           |
| Demo core                          | `lib/dev/democore.ts` — a stateful in-page core for `npm run dev` / browsers: transport, queue ops, rating, auto-advance, lyrics all actually behave                                                                                                                                                                                      |

**The Rust seam is wired end-to-end:** the desktop bridge (`src/desktop/bridge.rs`) decodes
the WebView envelopes and the gateway translates their names into `RemoteCommand` values,
binds every command/request's stable `request_id` to its optional `page_id`, and echoes that page on the
correlated reply so a replacement WebView cannot consume an older page's response. `Client.cmd()` resolves only after an explicit acknowledgement (or a typed
busy/offline/timeout/error result); callers with local in-flight state clear or roll it back on
failure, while the global toast surface reports every rejected mutation.

Topic declarations use a separate latest-desired-set lane rather than the bounded mutation
queue. Duplicate changes coalesce, changes made under pressure or while offline are retained,
and the current full set is reconciled after reconnect. A new `page_id` cycles page-owned topics
through unsubscribe/subscribe even when the set is unchanged, forcing fresh initial snapshots.
Mutation commands are not automatically retried because a disconnect after admission leaves
their outcome ambiguous. Legacy envelopes without `page_id` remain accepted.

Stable-ID outcome retention is a state-changing-command contract, not a blanket cache for every
command. `status` always executes again to return a fresh snapshot, and `run_search` executes
again because its immediate reply only acknowledges a completion push scoped to the current
session, `page_id`, and ticket. Replaying that acknowledgement after reconnect would point at a
dead session and leave the replacement page waiting for a result it can never receive.

### 1.5 Deferred v8 commands (registry id `core.v8-commands`)

The stores and views are finished and speak the final shapes, and the **demo core**
implements all of them. Server-side (feat/gui-wiring): every variant now EXISTS in
`src/remote/proto/command.rs` (the gateway no longer answers `bad_command`), and the
daemon dispatches most of them for real:

- **Live on a daemon owner**: `rate` · `queue_move` · `queue_clear_upcoming` ·
  `play_video` · `library_play` / `library_enqueue` / `library_remove` /
  `fetch_library_page` · `playlist_create` / `playlist_delete` / `playlist_play` /
  `playlist_add_tracks` / `playlist_remove_track` / `fetch_playlist_detail` ·
  `download` / `delete_download` · `ask_ai` · `fetch_why_gem` (v1 provenance:
  slot + empty reasons + null confidence; unknown tracks answer null) ·
  `lastfm_connect` / `listen_brainz_configure` / `account_set` (accounts topic:
  presence-only snapshots, one-shot auth-url/auth-failed events, secrets never on
  the wire) · `spotify_connect` / `transfer_list_spotify` / `transfer_start` /
  `transfer_cancel` (transfer topic; GUI imports land in the app's LOCAL playlist
  store — create-or-append by name, never the YouTube Music account)
- **Still `not_supported` (the last stream)**: `queue_remove_many` (no frontend
  sender yet) · `keymap_bind` / `keymap_unbind` / `keymap_reset_all` ·
  `theme_set_override` / `theme_clear_override` · `clear_romanization_cache`
- The standalone TUI owner answers `daemon_required` for the whole set (the GUI
  surface is daemon-only by design)

The registry entry `core.v8-commands` (capability `v8-commands`) carries the plan. The
main-screen entry points — rating, 🎬 video, queue drag-reorder / clear-upcoming, and the
DJ Gem composer — gate through `wip.gate('core.v8-commands')`, so a real core shows the
WipModal (with the agent brief) instead of a raw error toast; demo mode is always wired
(the demo core genuinely implements everything). Deeper surfaces (playlists CRUD, accounts
connect, transfer wizard, hotkey editing, downloads) still surface the plain toast until
the variants land. Server-side landing order: `RemoteCommand` variants + core dispatch +
parity tests (lockstep), regenerate ts-rs types, advertise the capability — every gate
then dissolves without a frontend release.

### 2. Pending — the patch-bay registry

`gui/src/lib/wiring/registry.ts` is the **single source of truth** for every feature whose
UI is finished but whose wire is not. Current entries (delete each as you wire it):

`core.v8-commands`

(`search.run`, `library.fetch`, `ai.chat`, `downloads.manage`, `radio.mode`,
`settings.apply`, `settings.animations`, `settings.theme-editor`, `settings.hotkeys`,
`help.keymap`, `library.playlists`, `settings.accounts`, `transfer.wizard`, `ai.whygem`,
`queue.reorder`, `i18n.catalog`, and `lyrics.live` are now wired — deleted from the
registry. `lyrics.live` rides the real B1 daemon wire: `PushEvent::LyricsSnapshot` +
`LyricLineModel` (generated types), published by `src/daemon/lyrics_host.rs`, retained
as the subscribe snapshot in `src/remote/publish.rs`, fetch gated on a live `lyrics`
subscriber. Daemon-owner only — a standalone TUI owner keeps its own lyrics panel and
pushes nothing on the topic. `artwork.live` needed **no new PushEvent**: art rides the
player snapshot (`TrackModel.artwork` ← `CoreView::artwork`, whose arrival re-pushes via
the fingerprint's `artwork_key`), the shell already serves `ytm://app/art/<key>`, and the
missing half was only that `MediaSession::publish` gated `request_artwork` behind the
platform-session gates — it now runs ahead of them, so disabled/not-yet-activated owners
(headless daemon, paused-at-rest restore) still populate the cache. The `artwork` Topic
enum slot stays reserved, unused.)

Each entry carries milestone, spec section, protocol surface, frontend seam, and notes.
In the running app, every pending surface shows either a **WireTag** chip (⚡ M2 · wiring
pending) or a **PendingSurface** panel; clicking opens the **WipModal**, whose "Copy agent
brief" button emits the exact marching orders for that feature — generated from the
registry by `agentBrief()`, so it cannot drift from this file or the spec.

### 3. Provisional — placeholder shapes to reconcile, not extend

- **Lyrics wire shape** (wired, `lyrics.live`): `PushEvent::LyricsSnapshot { video_id,
  lines: LyricLineModel[] }` — canonical generated types; the daemon publishes it
  (`src/daemon/lyrics_host.rs`), the demo core speaks the same shape.
- **Keyboard** (wired): the live keymap read model (`stores/keymap.svelte.ts`) drives the
  dispatcher (`lib/keyboard/{chord,dispatcher,actions,korean2set}.ts` + `App.svelte`),
  Settings→Hotkeys, and the Help overlay from one source. The demo core speaks a PROVISIONAL
  `keymap` block; the Korean 2-set table + chord format are self-consistent with the demo
  bindings until the Rust chord-fixture cross-test (05 §8.5) lands.
- **Settings tab values**: General / Playback / DJ Gem now bind the live `settings` read
  model via `stores/settings.svelte.ts` (model + pending overlay + dirty, docs/gui/05 §5.2);
  the demo core speaks the PROVISIONAL `settings_snapshot` + `apply {group,field,value}`
  shape (reconcile with ts-rs `SettingsModelV8`/`SettingChangeV8` §11.6/§13.3 when
  `settings-v8` lands). The Graphics tab's **Animations** and **Theme** blocks and the
  **Hotkeys** tab are now wired (theme rides `settings.theme` via `stores/theme.svelte.ts`,
  keymap rides `settings.keymap`); the **Accounts** tab is now wired too, on its own
  `accounts` topic via `stores/accounts.svelte.ts` (connect flows open the browser through
  win:openUrl), with the Spotify import wizard on the `transfer` topic
  (`stores/transfer.svelte.ts` + `views/wizards/SpotifyImport.svelte`).
- **Playlists** (wired): `stores/playlists.svelte.ts` mirrors the `playlists` topic + pulls a
  drill-down with `fetch_playlist_detail`; `views/library/PlaylistsPane.svelte` renders the
  list, detail, and the Create / Delete / Add-to-playlist dialogs. All PROVISIONAL shapes
  (`playlists_snapshot`, `PlaylistDetail`, `transfer_state`, `accounts_snapshot`,
  `accounts_auth_url`) live only in the demo core — reconcile with the M2/M4 core wires.
- **Local-theme precedence** (settled by `settings.theme-editor`): a chosen local skin layers
  over the pushed core theme and survives pushes until the user edits the core theme editor
  (picks a preset / edits or clears a role / toggles background-none), which hands control
  back to the core theme (`stores/theme.svelte.ts`).
- **Why-Gem** (wired, `ai.whygem`): `stores/whygem.svelte.ts` reads a PROVISIONAL
  `{ kind: 'why_gem_provenance', video_ids }` event on the `ai` topic (which rows are DJ-Gem
  autoplay picks → where the "why?" affordance shows) and fetches the explanation on open with
  `req fetch_why_gem { video_id }` → `{ slot, reasons, confidence }`. Both shapes live only in
  the demo core; reconcile with the M4 core wire + ts-rs types when they land.
- **Queue reorder** (wired, `queue.reorder`): `cmd queue_move { from, to, expected_rev }` is a
  v8 command the frozen `command.rs` + core dispatch must still add — the desktop seam forwards
  it the moment the variant exists (like the other deferred v8 commands). `lib/dnd/reorder.ts`
  holds the pure index/scroll math; `VirtualList.svelte` drives pointer-drag from any row's
  `[data-drag-handle]`.
- **i18n** (wired, `i18n.catalog`): frontend-owned flat catalog `src/i18n/{en,ko}.json` +
  reactive `t()` in `lib/i18n.svelte.ts`; the language rides `settings.ui.language` (App's
  `$effect` syncs it, live switch, no reload). Chrome only — romanized titles stay core-side
  (`TrackModel.display_*`), never romanized in the GUI. `tests/i18n.test.ts` pins en/ko key
  parity, placeholder alignment, and that every literal `t()` key exists.

## Conventions (enforced by `tests/wiring.test.ts`)

1. Every stubbed call site goes through `wip.gate(id)` / `wip.open(id)` and carries a
   greppable marker comment: `TODO(wire:<milestone>/<feature-id>)`.
2. Find a feature's seams: `rg "wire:M2/search.run" gui/src`.
3. Markers ↔ registry may not drift: an id used anywhere in `src/` must exist in the
   registry, and every registry id must be referenced somewhere in `src/`. The vitest
   fails otherwise — so deleting a registry entry forces you to clean up its call sites,
   and vice versa.
4. `capability` strings auto-dissolve stubs: `wip.gate(id)` returns true (real path) once
   the connected core advertises the capability in its hello/conn payload. The strings
   are provisional until the core defines them — keep both sides in sync.

## How to wire a feature (the standard loop)

1. Read the registry entry's `brief` (docs/gui/07 §N) and the store contract
   (docs/gui/05 §5). The WipModal's "Copy agent brief" gives you this as a prompt.
2. Create the store (`lib/stores/<x>.svelte.ts`) mirroring the read model; replace every
   marked call site with real `client.cmd/req/on` calls.
3. Teach the demo core (`lib/dev/democore.ts`) to answer the new commands/topics so
   `npm run dev` keeps exercising the feature end-to-end.
4. Delete the registry entry; run the tests — they force the marker cleanup.
5. Update this file and docs/gui/PROGRESS.md.

Gates (run in `gui/`): `npm run check && npm test && npm run build` — plus
`npm run lint` (prettier) which this rework brought to green; keep it there.

## Layout of the new frontend

```
src/i18n/           en.json · ko.json (flat keyed catalog; i18n.svelte.ts holds reactive t())
src/lib/wiring/     registry.ts (the patch bay) · wip.svelte.ts (gate + modal state)
src/lib/theme/      roles.ts (the 34 roles) · local.ts (GUI-owned skins)
src/lib/keyboard/   chord.ts · dispatcher.ts · actions.ts · korean2set.ts (live dispatcher)
src/lib/dnd/        reorder.ts (pure drag index/scroll math for queue.reorder)
src/lib/dev/        democore.ts (stateful fake core for browsers)
src/lib/stores/     connection · theme · ui · playback · queue · lyrics · whygem · toasts
src/lib/components/ Modal WipModal WireTag PendingSurface Toggle Kbd VirtualList
                    AlbumArt SeekBar VolumeBar TrackRow WhyGemPopover
src/views/          NowPlaying · SearchView · LibraryView · AiView · TransportBar ·
                    QueuePanel · settings/{SettingsView,+6 tabs,SettingRow,SettingSection} ·
                    overlays/{HelpOverlay,AboutCard}
```
