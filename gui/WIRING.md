# The Patch Bay — wiring handoff for follow-up agents

The frontend under `gui/src/` is built **to the finished spec** (docs/gui/05–07): every
screen, tab, overlay, and control exists and is styled. What separates it from "done" is
a known list of **wires** — protocol connections to the core. This file is the contract
for continuing that work. Read it before touching anything.

## The three tiers

### 1. Wired — live today, do not stub these

| Surface | How it's wired |
|---|---|
| `player` / `queue` topic rendering | `playback.svelte.ts` / `queue.svelte.ts` consume `player_snapshot` / `queue_snapshot` pushes (the B0 wire), incl. interpolation with the ported mini-player constants (`lib/time.ts` — do not re-derive) |
| Transport commands | `toggle_pause` `next` `prev` `seek_to` `set_volume` `toggle_shuffle` `cycle_repeat` `streaming` (v7-frozen) + `rate` `queue_play` `queue_remove_many` `queue_clear_upcoming` `play_video` (v8) — sent fire-and-forget per protocol |
| Local themes | `lib/theme/local.ts` + `ThemeStore.applyLocal` — 5 GUI-owned skins (incl. Crimson Mono and Ember Wine), applied live, persisted in localStorage |
| Demo core | `lib/dev/democore.ts` — a stateful in-page core for `npm run dev` / browsers: transport, queue ops, rating, auto-advance, lyrics all actually behave |

**The single Rust seam that makes the wired tier real end-to-end:** the desktop bridge
(`src/desktop/bridge.rs`) currently only echoes `req ping`. Forwarding `cmd` envelopes to
the gateway's session (name → `RemoteCommand`, snake_case tags already match) and fanning
topic pushes back as `event` envelopes is the M1 shell work — the frontend sends/consumes
the right shapes today.

### 2. Pending — the patch-bay registry

`gui/src/lib/wiring/registry.ts` is the **single source of truth** for every feature whose
UI is finished but whose wire is not. Current entries (delete each as you wire it):

`library.playlists` · `queue.reorder` · `settings.hotkeys` · `settings.theme-editor` ·
`settings.accounts` · `ai.whygem` · `transfer.wizard` · `help.keymap` · `lyrics.live` ·
`artwork.live` · `i18n.catalog`

(`search.run`, `library.fetch`, `ai.chat`, `downloads.manage`, `radio.mode`,
`settings.apply`, and `settings.animations` are now wired — deleted from the registry.)

Each entry carries milestone, spec section, protocol surface, frontend seam, and notes.
In the running app, every pending surface shows either a **WireTag** chip (⚡ M2 · wiring
pending) or a **PendingSurface** panel; clicking opens the **WipModal**, whose "Copy agent
brief" button emits the exact marching orders for that feature — generated from the
registry by `agentBrief()`, so it cannot drift from this file or the spec.

### 3. Provisional — placeholder shapes to reconcile, not extend

- **Lyrics wire shape** `{ kind: 'lyrics_snapshot', lines: [{ ms, text }] }` in
  `lyrics.svelte.ts` — only the demo core speaks it. Align with the real B1 topic + ts-rs
  types when they exist.
- **Keyboard**: `lib/keyboard/provisional.ts` is a hardcoded shortcut table (executed by
  `App.svelte`, displayed by Help and Settings→Hotkeys — one source, three views). The M3
  dispatcher replaces all of it.
- **Settings tab values**: General / Playback / DJ Gem now bind the live `settings` read
  model via `stores/settings.svelte.ts` (model + pending overlay + dirty, docs/gui/05 §5.2);
  the demo core speaks the PROVISIONAL `settings_snapshot` + `apply {group,field,value}`
  shape (reconcile with ts-rs `SettingsModelV8`/`SettingChangeV8` §11.6/§13.3 when
  `settings-v8` lands). The Graphics tab's **Animations** block is now wired (master/fps/
  pause + the 25 effect flags bind `settings.animations`; runtime in `stores/anim.svelte.ts`);
  theme-editor, Hotkeys, and Accounts still render defaults under their own wires.
- **Local-theme precedence**: a chosen local skin currently wins over the boot theme;
  reconcile with core-side theme state when `settings.theme-editor` lands (promote the
  local skins into core presets, or keep them as GUI-side overlays — decide then).

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
src/lib/wiring/     registry.ts (the patch bay) · wip.svelte.ts (gate + modal state)
src/lib/theme/      roles.ts (the 34 roles) · local.ts (GUI-owned skins)
src/lib/keyboard/   provisional.ts (temporary shortcut table)
src/lib/dev/        democore.ts (stateful fake core for browsers)
src/lib/stores/     connection · theme · ui · playback · queue · lyrics · toasts
src/lib/components/ Modal WipModal WireTag PendingSurface Toggle Kbd VirtualList
                    AlbumArt SeekBar VolumeBar TrackRow
src/views/          NowPlaying · SearchView · LibraryView · AiView · TransportBar ·
                    QueuePanel · settings/{SettingsView,+6 tabs,SettingRow,SettingSection} ·
                    overlays/{HelpOverlay,AboutCard}
```
