# Update Notification and mpv Overlay Keymap Plan

## Purpose

Implement the two missing user-facing behaviors without widening the release surface:

1. A newer YuTuTui! release should produce an actual desktop notification, not only an About-card notice, nav-brand dot, and transient status line.
2. The external mpv music-video overlay should be controllable while the mpv window has focus, using YuTuTui!'s expected previous/next/pause keys, and those overlay keys should be visible and remappable through the same keybinding and cheat-sheet system as the TUI.

This is a planning document only. It does not modify source behavior.

## Current Code Observations

### Update awareness

- `src/update/mod.rs` owns release awareness. It detects install method, resolves the latest GitHub release, compares it with `env!("CARGO_PKG_VERSION")`, and emits `UpdateEvent::Checked(UpdateStatus)`.
- `src/main.rs` starts the check with `update::spawn_update_check(...)` after startup and maps it through `RuntimeEvent::Update`.
- `src/runtime.rs` maps `RuntimeEvent::Update(UpdateEvent::Checked(status))` to `Msg::UpdateChecked(status)`.
- `src/app/mod.rs` handles `Msg::UpdateChecked(status)` by:
  - setting the transient TUI status only when `status.available && status.first_seen`;
  - storing `self.overlays.update_status = Some(status)`;
  - not returning `Cmd::DesktopNotify`.
- `src/notify.rs` and the main loop already support `Cmd::DesktopNotify { title, body }`. The main loop handles it directly before `RuntimeHandles::dispatch`, which is the right place because OSC notification output writes to the terminal's stdout.
- `src/update/mod.rs` persists `<data>/update.json` with `latest_tag` and `toasted_tag`. It sets `toasted_tag` before calling `emit(UpdateEvent::Checked(...))`, so a failed/dropped/unnoticed app event can still suppress future first-seen notifications for the same tag.
- The notifier is best-effort and can be blocked by terminal/tmux behavior, but the primary missing link is earlier: update handling never creates `Cmd::DesktopNotify`.

### mpv video overlay

- `src/app/player.rs::open_video_overlay` spawns a separate mpv window for the current YouTube video and sends `Cmd::VideoConnect` when an IPC socket path exists.
- `src/app/mod.rs::spawn_video_overlay` intentionally omits `--no-config` for the video overlay, so the user's mpv configuration still applies. It adds `--input-ipc-server=...` and `--keep-open=yes` when IPC is available.
- `src/player/video.rs` connects to the overlay mpv and currently:
  - observes `eof-reached`;
  - binds `>` to `script-message ytt-video-next`;
  - binds `<` to `script-message ytt-video-prev`;
  - maps those client messages to `VideoEvent::Next` and `VideoEvent::Prev`;
  - accepts only `VideoCmd::Load(url)`.
- `src/app/player.rs::on_video_overlay_event` already maps `VideoEvent::Next` and `VideoEvent::Prev` to queue movement plus `Cmd::VideoLoad(...)`.
- Pause is not integrated in the video overlay protocol. mpv's default `SPACE`/`p` pause likely works inside mpv itself, but YuTuTui does not observe overlay pause or route it through app state.
- `src/player/proto.rs` already has the JSON helpers needed for pause toggling: `cmd_cycle("pause", request_id)` and `cmd_observe(id, "pause")`.
- `RuntimeEvent::Video { .. }` currently uses `EventPolicy::DropIfStale`. For user-triggered overlay key events such as next/prev/pause/close, that should be reconsidered because these are control events, not telemetry.

### Keybindings and cheat sheet

- `src/keymap.rs` is the TUI source of truth for actions, contexts, default bindings, conflict checks, config serialization, and display labels.
- `src/ui/views/help.rs` builds the `?` cheat sheet from `keymap::groups()`, so a new default keybinding context automatically appears in help.
- `src/ui/views/settings.rs` builds Settings -> Hotkeys from `keymap::editable_entries()`, so a new default keybinding context automatically appears in the Hotkeys tab.
- The current keymap model is one chord per `(KeyContext, Action)`. It does not support multiple default aliases for the same action.
- Existing contexts include `Player`, `NowPlaying`, `Common`, `Global`, `Library`, `Playlists`, `Queue`, `SearchInput`, `SearchResults`, `Settings`, `AiInput`, and `AiSuggestions`.
- Player defaults already use `,`/`.` for previous/next and `Space` for play/pause.
- The keymap does not currently know how to serialize a `Chord` into mpv's `input.conf` key-name syntax. mpv accepts literal keys, named special keys, and modifiers like `Ctrl+`/`Alt+`/`Shift+`, but not every crossterm `KeyCode` is meaningful in mpv.

### GUI/WebView side effect

- The embedded GUI has a provisional keymap store in `gui/src/lib/stores/keymap.svelte.ts`.
- Rust `src/remote/publish.rs` currently publishes `KeymapSettingsModel { bindings, actions: Vec::new() }`, so the GUI does not yet receive the full Rust keymap action catalog.
- The primary target for this plan is the TUI. If the Rust keymap model is expanded, a follow-up should reconcile GUI keymap context lists and demo defaults so the embedded frontend does not drift.

## Constraints

- Do not touch release surfaces: `.github/workflows/**`, `packaging/**`, tags, publishing, or lockfiles.
- Do not launch `ytt` or `cargo run` directly. Runtime verification must use `.claude/skills/verify` because raw launches can play real audio and write real config.
- Playback/overlay changes are in a risk zone. Before implementation, re-read `.claude/harness/risk-map.md` and keep changes scoped.
- The canonical gate is `~/.fable-harness/bin/run-gates .`. Native equivalent is:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- Warnings count as failures.

## Implementation Plan

## Implementation Status

- [x] Phase 1: connect update checks to desktop notifications.
- [x] Phase 2: make update "first seen" bookkeeping less fragile.
- [ ] Phase 3: add a first-class mpv overlay key context.
- [ ] Phase 4: convert YuTuTui chords into mpv key names.
- [ ] Phase 5: install remapped keys into the overlay mpv.
- [ ] Phase 6: implement overlay pause and state sync.
- [ ] Phase 7: documentation and user-facing copy.

### Phase 1: connect update checks to desktop notifications

Files:

- `src/app/mod.rs`
- `src/app/types.rs` only if the `Cmd::DesktopNotify` docs are generalized
- `src/app/tests/...` for reducer coverage

Steps:

1. In `Msg::UpdateChecked(status)`, keep the existing persistent state assignment:
   - `self.overlays.update_status = Some(status.clone_or_moved)`
   - About card and nav-brand dot behavior must remain unchanged.
2. When `status.available && status.first_seen`, return a `Cmd::DesktopNotify`.
3. Reuse `crate::update::update_instructions(status.method)` to build a short body:
   - Prefer `command` when present.
   - Fall back to `note`.
   - Include the latest version display.
4. Keep the existing TUI status line. This preserves the terminal fallback even when native/OSC notification delivery fails.
5. Update comments that currently imply "toast" means only status-line text. Use explicit terms:
   - `status toast` for in-TUI status text;
   - `desktop notification` for `Cmd::DesktopNotify`.

Expected reducer behavior:

- New release, first sighting:
  - status line updated;
  - `Cmd::DesktopNotify` emitted;
  - `overlays.update_status` stored.
- New release, not first sighting:
  - no one-time notification;
  - persistent About/nav state still stored.
- No newer release:
  - no notification;
  - update status stored so About can accurately say nothing new, if it ever reads it.

Tests:

- Add an app reducer test that constructs `UpdateStatus { available: true, first_seen: true, ... }` and asserts a `Cmd::DesktopNotify` is returned.
- Add a matching test for `first_seen: false` and/or `available: false` asserting no desktop notification.
- Assert `app.overlays.update_status` is populated in all `Msg::UpdateChecked` cases.

### Phase 2: make update "first seen" bookkeeping less fragile

This can be a second PR if the first PR needs to stay small.

Problem:

- `spawn_update_check` writes `toasted_tag = latest` before the app reducer has accepted the event or returned `Cmd::DesktopNotify`.
- That means a transient app/runtime failure can suppress the one-time notification forever for the same tag.

Preferred design:

1. Rename the concept internally from `toasted_tag` to either:
   - `notified_tag` if the durable meaning is "desktop/status notification requested"; or
   - `seen_tag` if the durable meaning is only "the app accepted this update event".
2. Move the durable mark out of the background resolver's pre-emit path.
3. Add a reducer side-effect command, for example:
   - `Cmd::Persist(PersistCmd::UpdateSeen { tag })`, if the persist actor should own it; or
   - a new update actor command, if update state remains private to `src/update`.
4. Let `Msg::UpdateChecked` emit that command after it has updated app state and queued the desktop notification.
5. Keep old `toasted_tag` deserialization compatible:
   - old JSON files should migrate without resetting notifications unexpectedly;
   - new writes can use the renamed field, or keep the field name but update comments if renaming is too invasive.

Small fallback:

- If moving persistence is too much for the first pass, leave persistence where it is and document the limitation in tests/comments. The Phase 1 desktop notification is still the main user-visible fix.

Tests:

- Unit-test update state migration if the field is renamed.
- Test first-seen calculation with a temporary data dir or pure helper.
- If a new persist command is added, test that `Msg::UpdateChecked(first_seen=true)` returns both `DesktopNotify` and the update-seen persistence command.

### Phase 3: add a first-class mpv overlay key context

Files:

- `src/keymap.rs`
- `src/ui/views/help.rs` only if extra fixed rows or context ordering need custom handling
- `src/ui/views/settings.rs` only if layout needs special grouping/tabs
- `src/settings.rs` and `src/app/settings_reducer.rs` only if Hotkeys tab navigation or rows need special behavior
- `src/app/tests/settings_ui.rs`, `src/app/tests/keymap_conflicts.rs`
- `gui/src/lib/stores/keymap.svelte.ts` and GUI tests if the embedded GUI is included in the same PR

Recommended TUI model:

1. Add `KeyContext::MpvOverlay`.
2. Add context metadata:
   - id: `mpv_overlay`
   - English title: `mpv video overlay`
   - Korean title: `mpv 영상 창`
3. Add overlay-specific actions. Prefer new action variants where the behavior differs from the Player action:
   - `VideoTogglePause`
   - `VideoNext`
   - `VideoPrev`
   - `VideoClose`
   - `VideoToggleFullscreen`
   - `VideoToggleMute`
4. Use context-specific labels:
   - `Video play / pause`
   - `Next video`
   - `Previous video`
   - `Close video`
   - `Fullscreen`
   - `Mute / unmute`
5. Add default bindings:
   - `Space` -> `VideoTogglePause`
   - `.` -> `VideoNext`
   - `,` -> `VideoPrev`
   - `q` -> `VideoClose`
   - `f` -> `VideoToggleFullscreen`
   - `m` -> `VideoToggleMute`

Alias decision:

- Current mpv overlay hard-codes `<` and `>` for next/prev.
- mpv defaults also use `p` as a pause alias.
- The current keymap cannot bind multiple chords to one action.
- To keep scope controlled, make the primary displayed/remappable defaults the YuTuTui keys (`Space`, `.`, `,`, `q`, `f`, `m`) and keep `<`, `>`, and `p` as optional compatibility aliases only if they can be represented cleanly.

Two acceptable alias strategies:

1. Minimal strategy:
   - Only primary overlay actions are remappable.
   - Keep `<`, `>`, and `p` as fixed compatibility keybinds inside `src/player/video.rs`.
   - Show them in help only as non-editable "mpv compatibility aliases" if that does not clutter the sheet.
2. Complete strategy:
   - Extend the keymap to support multiple chords per action.
   - Update config serialization, conflict checks, Settings -> Hotkeys UI, reset behavior, GUI store, and tests.
   - This is a larger keymap project and should be split from the mpv control PR unless explicitly requested.

Recommendation:

- Start with the minimal strategy for implementation reliability.
- Document fixed compatibility aliases in README/help if retained.
- Do not introduce multi-chord keymap support as a drive-by change.

Hotkeys "mpv tab" interpretation:

- The current TUI Settings screen has one `Hotkeys` tab, with contexts rendered as group headers.
- Adding `KeyContext::MpvOverlay` gives users a clear mpv group inside the existing Hotkeys tab and automatically keeps the cheat sheet in sync.
- A separate top-level Settings tab named `mpv` would require changing `SettingsTab::ALL`, navigation, rendering, tests, and likely cramped tab layout. Use a top-level tab only if the product direction is to split hotkeys into multiple pages.

### Phase 4: convert YuTuTui chords into mpv key names

Files:

- `src/keymap.rs` or a new small module near `src/player/video.rs`
- `src/player/video.rs`

Need:

- The TUI stores `Chord { KeyCode, KeyModifiers }`.
- mpv `keybind` expects mpv `input.conf` key-name syntax.
- Implement a constrained conversion helper, for example:
  - `keymap::chord_to_mpv_input(chord) -> Option<String>`

Supported initial mapping:

- Plain printable `KeyCode::Char(c)`:
  - `Space` -> `SPACE`
  - punctuation like `.`, `,`, `<`, `>` -> literal string
  - ASCII letters preserve case where meaningful
- Named keys:
  - `Esc` -> `ESC`
  - arrows -> `LEFT`, `RIGHT`, `UP`, `DOWN`
  - `Enter` -> `ENTER`
  - `Tab` -> `TAB`
  - `Backspace` -> `BS` or `BACKSPACE` after verifying mpv accepts the chosen spelling
  - `Delete` -> `DEL`
  - `Home`, `End`, `PageUp`, `PageDown`
  - `F1` through `F12`
- Modifiers:
  - `Ctrl+`, `Alt+`, `Shift+` for keys mpv can name.

Unsupported keys:

- Media keys, pure modifier keys, and terminal-only keys should be rejected for `MpvOverlay` bindings.
- The Settings capture path should show a clear status/modal instead of accepting a binding that cannot be installed in mpv.

Tests:

- Unit-test default overlay bindings convert to the exact mpv key names sent over IPC.
- Unit-test unsupported crossterm keys return `None`.
- Unit-test Korean jamo normalization still happens before conversion because `Chord::from(KeyEvent)` already normalizes it.

### Phase 5: install remapped keys into the overlay mpv

Files:

- `src/app/types.rs`
- `src/app/player.rs`
- `src/runtime.rs`
- `src/player/video.rs`
- `src/player/proto.rs`

Data flow:

1. `App::open_video_overlay` computes the current mpv overlay bindings from `self.keymap`.
2. Extend `Cmd::VideoConnect` to include the binding list:
   - `Vec<VideoKeyBinding>` or a small `VideoOverlayBindings` struct.
3. `RuntimeHandles::dispatch` passes those bindings into `player::video::connect(...)`.
4. `player::video::run(...)` writes `keybind` commands for each supported key.

Binding command messages:

- `VideoTogglePause` -> `script-message ytt-video-toggle-pause`
- `VideoNext` -> `script-message ytt-video-next`
- `VideoPrev` -> `script-message ytt-video-prev`
- `VideoClose` -> `script-message ytt-video-close`
- `VideoToggleFullscreen` -> either `cycle fullscreen` directly or `script-message ytt-video-toggle-fullscreen`
- `VideoToggleMute` -> either `cycle mute` directly or `script-message ytt-video-toggle-mute`

Preferred rule:

- Route actions that affect YuTuTui state through `script-message`.
- Direct mpv-only actions can use direct mpv commands, but routing them through YuTuTui keeps the event surface consistent and makes status messages/test coverage easier.

Live rebind behavior:

- Settings key edits commit on `close_settings`.
- If the overlay is open when the keymap changes, send a new command such as `Cmd::VideoBindKeys(bindings)`.
- `VideoCmd::BindKeys(bindings)` should overwrite yututui-owned bindings.

Caveat:

- mpv `keybind` overwrites a key but does not automatically restore the user's previous command when a key is no longer yututui-owned.
- To avoid stale bindings after live rebinds, either:
  - track the previously installed yututui-owned mpv keys and bind old keys to an inert command or a pass-through only if mpv supports the intended behavior; or
  - defer rebind changes until the next overlay spawn and say so in the Settings status line.

Recommendation:

- First implementation: apply remapped keys on the next overlay open.
- Optional follow-up: support live rebinding after verifying safe old-key removal semantics against mpv.

### Phase 6: implement overlay pause and state sync

Files:

- `src/player/video.rs`
- `src/player/proto.rs`
- `src/app/types.rs`
- `src/runtime.rs`
- `src/app/player.rs`
- `src/runtime.rs` event policy

Protocol additions:

- `VideoEvent::TogglePause`
- `VideoEvent::Paused(bool)`
- `VideoEvent::Close` or reuse `Quit` carefully
- Optional:
  - `VideoEvent::ToggleFullscreen`
  - `VideoEvent::ToggleMute`

- `VideoCmd::CyclePause`
- Optional:
  - `VideoCmd::CycleFullscreen`
  - `VideoCmd::CycleMute`

Connection setup:

- Observe pause:
  - `proto::cmd_observe(<id>, "pause")`
- Add keybinds:
  - `script-message ytt-video-toggle-pause`
  - `script-message ytt-video-next`
  - `script-message ytt-video-prev`
  - `script-message ytt-video-close`

Reducer behavior:

- `VideoEvent::TogglePause`:
  - return `Cmd::VideoTogglePause` or `Cmd::Video(VideoCmd::CyclePause)` depending on final command shape.
- `VideoEvent::Paused(true)`:
  - set a short info status such as `Video paused`;
  - mark dirty.
- `VideoEvent::Paused(false)`:
  - set `Video playing`;
  - mark dirty.
- Do not mutate the underlying audio engine pause state from overlay pause changes. The audio engine is intentionally pinned paused while the video overlay owns playback.
- `VideoClose` should use the same close/resume path as manual `v` close, so audio resumes only if `video.paused_audio` is still true.

Runtime event policy:

- Change `RuntimeEvent::Video` policy to distinguish control from telemetry:
  - `Next`, `Prev`, `TogglePause`, `Close`, `Quit`, `Eof`, and `Failed` should be `MustDeliver { lane: Control }` or another non-dropping policy.
  - `Paused(bool)` can be coalesced/latest or best-effort if the queue is full.
- Keep reducer-side generation checks. They are still the correctness boundary for stale overlay windows.

Tests:

- `src/player/video.rs`:
  - `client-message ["ytt-video-toggle-pause"] -> VideoEvent::TogglePause`
  - `property-change pause true -> VideoEvent::Paused(true)`
  - `property-change pause false -> VideoEvent::Paused(false)`
  - new close/fullscreen/mute client messages if implemented.
- `src/app/tests/video.rs`:
  - overlay toggle pause emits the video pause command;
  - pause property updates status without resuming audio;
  - close event follows existing `finish_video_overlay` semantics;
  - next/prev still advance queue and load the landed video.
- `src/runtime.rs` tests if event policy tests exist; otherwise add focused tests only if local pattern supports them.

### Phase 7: documentation and user-facing copy

Files:

- `README.md`
- `README.ko.md`
- `README.ja.md` only if the project maintains parity there for user-facing key docs
- `docs/index.html` only if the website feature tour is kept current manually
- `src/ui/views/help.rs` only if fixed compatibility aliases are listed

Updates:

- README video section:
  - mention that the mpv window has its own remappable overlay controls.
- Essential keys:
  - keep `,` / `.` as previous/next.
  - clarify that the same defaults apply in the mpv overlay when it has focus.
- Troubleshooting:
  - note that OS desktop notifications are best-effort and may require terminal/tmux support.
- Settings text:
  - ensure the Hotkeys tab group title clearly says mpv.

## PR Breakdown

Recommended sequence:

1. `update-desktop-notify`
   - add desktop notify command on first-seen updates;
   - add reducer tests;
   - update comments.
2. `update-seen-persistence`
   - move/rename `toasted_tag` bookkeeping if desired;
   - add migration/state tests.
3. `mpv-overlay-key-context`
   - add `MpvOverlay` key context, overlay action metadata, defaults, conversion helper, and Settings/help tests;
   - no mpv runtime behavior change yet except maybe binding list construction tests.
4. `mpv-overlay-controls`
   - pass bindings to video IPC;
   - add pause/close/fullscreen/mute protocol;
   - adjust runtime event policy;
   - add video IPC and reducer tests.
5. `docs-and-gui-followup`
   - update README/site docs;
   - reconcile embedded GUI keymap context/action lists if included.

## Verification Plan

Fast focused checks during implementation:

- `cargo test -p yututui keymap`
- `cargo test -p yututui video`
- `cargo test -p yututui update`
- `cargo test -p yututui settings_key`

Exact test names may need adjustment after inspecting `cargo test -- --list`.

Final gate:

- `~/.fable-harness/bin/run-gates .`

Runtime verification:

- For TUI rendering/keybinding behavior, use the project verify skill only.
- Do not run `cargo run`, `ytt`, or raw mpv manually as part of agent verification.

Manual/debug scenario for update checks:

- Use a temporary data dir and debug override in a debug build:
  - `YTM_DATA_DIR="$(mktemp -d)"`
  - `YTM_UPDATE_FORCE=v999.0.0`
  - `YTM_INSTALL_METHOD=unknown`
- The implementation should not rely on the real latest GitHub release being newer than the local build.

## Open Questions

1. Should `<`, `>`, and `p` remain fixed compatibility aliases, or should the project invest in multi-chord keymap support?
2. Should overlay key changes take effect immediately for an already-open mpv window, or only on the next overlay open?
3. Should `VideoToggleFullscreen` and `VideoToggleMute` be first-class YuTuTui actions now, or should the first pass only cover previous/next/pause/close?
4. Should update notification persistence be fixed in the same PR as desktop notification emission, or split to reduce risk?
5. Should the embedded GUI keymap model be reconciled in the same milestone, despite `gui/` not being the canonical TUI gate?

## References

- [mpv manual](https://mpv.io/manual/stable/): JSON IPC is the supported local-control path for external programs using `--input-ipc-server`.
- [mpv manual](https://mpv.io/manual/stable/): `keybind`, `script-message`, and `observe_property` are command-interface primitives available over JSON IPC.
- [mpv manual](https://mpv.io/manual/stable/): input bindings use `input.conf` key-name syntax; key names can be literal characters, named keys, and modifier combinations.
