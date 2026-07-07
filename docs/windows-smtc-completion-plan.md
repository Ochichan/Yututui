# Windows SMTC Completion Plan

Goal: take the already-shipped Windows SMTC (System Media Transport Controls)
integration from "compiles and is believed to work" to "verified on real
Windows 10 + 11, correctly identified in the flyout, free of duplicate
sessions, and released" — the Windows counterpart of the macOS Control Center
integration that already demos correctly.

This document follows the repo's plan-doc convention
(`daemon-mode-completion-plan.md`, `desktop-tray-companion-plan.md`): numbered
sections, `[x]/[ ]` checklists updated as work lands, risks, and a manual QA
runbook. Research below was gathered 2026-07-03 by reading Microsoft Learn,
the Chromium/Firefox/mpv sources, and the souvlaki issue tracker; every
load-bearing claim carries its source. Claims we could not confirm are marked
UNCERTAIN.

---

## 0. Source And Repo Context

### 0.1 External references checked

Platform documentation:

- `ISystemMediaTransportControlsInterop::GetForWindow` — the only documented
  HWND requirement is "a top-level window that belongs to the calling
  process".
  <https://learn.microsoft.com/en-us/windows/win32/api/systemmediatransportcontrolsinterop/nf-systemmediatransportcontrolsinterop-isystemmediatransportcontrolsinterop-getforwindow>
- SMTC how-to guide — DisplayUpdater ordering (Type → properties → thumbnail →
  `Update()`), timeline requirements ("You must set MinSeekTime and
  MaxSeekTime in order for the PositionChangeRequest to be raised"), the ~5 s
  update cadence recommendation, and the PlaybackRate seeding requirement
  ("[PlaybackRateChangeRequested] will not be raised until after you have set
  a value for the PlaybackRate property at least one time").
  <https://learn.microsoft.com/en-us/windows/uwp/audio-video-camera/system-media-transport-controls>
- ButtonPressed threading ("not called from the UI thread").
  <https://learn.microsoft.com/en-us/windows/apps/develop/media-playback/system-media-transport-controls>
- `RandomAccessStreamReference.CreateFromUri` valid schemes are http, https,
  ms-appx, ms-appdata — `file://` is NOT valid, which is why we load artwork
  bytes into an `InMemoryRandomAccessStream` instead.
  <https://learn.microsoft.com/en-us/uwp/api/windows.storage.streams.randomaccessstreamreference.createfromuri>
- `GetConsoleWindow` — under ConPTY the console HWND is a fake
  `PseudoConsoleWindow` owned by another process (OpenConsole/conhost), so it
  must never be passed to `GetForWindow`.
  <https://learn.microsoft.com/en-us/windows/console/getconsolewindow>,
  <https://github.com/microsoft/terminal/discussions/17147>,
  firsthand failure: <https://github.com/Sinono3/souvlaki/issues/30>
- Consumer-side verification API (`Windows.Media.Control`):
  `GlobalSystemMediaTransportControlsSessionManager` — enumerate sessions,
  read metadata/status/timeline, send `TryPauseAsync` /
  `TrySkipNextAsync` / `TryChangePlaybackPositionAsync`. Interactive sessions
  only (fails as service/SYSTEM: <https://github.com/dotnet/runtime/issues/84293>).
  <https://learn.microsoft.com/en-us/uwp/api/windows.media.control.globalsystemmediatransportcontrolssessionmanager>
- AppUserModelID mechanics for unpackaged apps.
  <https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-setcurrentprocessexplicitappusermodelid>,
  <https://learn.microsoft.com/en-us/windows/win32/shell/appids>

Reference implementations read:

- Chromium `components/system_media_controls/win/system_media_controls_win.cc`
  — the load-bearing comment "`ClearAll()` unsets the type, if we don't set it
  again then the artist won't be displayed" (our `update_display` already does
  this); PNG thumbnail via `InMemoryRandomAccessStream` + `DataWriter`;
  10 ms self-debounce; timeline pushed event-driven with `PlaybackRate`.
  <https://raw.githubusercontent.com/chromium/chromium/main/components/system_media_controls/win/system_media_controls_win.cc>
- Firefox `widget/windows/WindowsSMTCProvider.cpp` — teardown ordering comment:
  modifying controls without a strictly sequential cleanup "would cause a
  problem where the SMTC wasn't clean up completely and show the executable
  name" (drives gap G6); hidden `Firefox-MediaKeys` window; PNG preference for
  thumbnails.
  <https://raw.githubusercontent.com/mozilla/gecko-dev/master/widget/windows/WindowsSMTCProvider.cpp>
- mpv `osdep/win32/smtc.cpp` + PR #14338 — mpv's own SMTC provider: dedicated
  thread, hidden `"mpv-smtc"` window, "Dummy window is used to allow SMTC to
  work also in audio only mode, where VO may not be created."; identity gap
  ("Unknown app" unless registered via `--register`); `--media-controls`
  option history (introduced v0.39.0 default `player`, later a bool default
  `yes` with `media-controls=no` in the built-in `[libmpv]` profile).
  <https://raw.githubusercontent.com/mpv-player/mpv/master/osdep/win32/smtc.cpp>,
  <https://github.com/mpv-player/mpv/pull/14338>,
  <https://raw.githubusercontent.com/mpv-player/mpv/master/etc/builtin.conf>
- souvlaki (not a dependency; surveyed as the ecosystem baseline) — issue #67
  confirms the unpackaged-exe identity default is "it displays no logo and
  'unknown app'"; #30 the GetConsoleWindow failure; #39 the `file://`
  StorageFile hang souvlaki users hit (we avoid it by design: bytes →
  in-memory stream). <https://github.com/Sinono3/souvlaki/issues/67>,
  <https://github.com/Sinono3/souvlaki/issues/30>,
  <https://github.com/Sinono3/souvlaki/issues/39>
- Rust TUI precedent: spotify-player and termusic both create their own
  hidden top-level window for SMTC. termusic's field lesson (verbatim): "Dont
  drop the window until termusic exists, as otherwise Media Controls will
  silently fail after dropping the handle" — drives a QA item on our settings
  toggle off→on cycle (we destroy AND fully recreate window+session, which is
  structurally different, but it must be exercised on hardware).
  <https://raw.githubusercontent.com/aome510/spotify-player/master/spotify_player/src/media_control.rs>,
  <https://raw.githubusercontent.com/tramhao/termusic/master/playback/src/mpris.rs>

Windows surface reality (what stock UI actually shows — drives the QA
checklist expectations):

- Win10 volume flyout banner: metadata + art + prev/play-pause/next. No seek
  bar (an entire third-party ecosystem exists to add one: ModernFlyouts,
  MediaFlyout).
- Win11 Quick Settings media card: prev/play-pause/next + artwork. Stock UI
  has NO seek bar and NO shuffle/repeat buttons as of late 2025 (FluentFlyout
  markets "Repeat All, Repeat One, Shuffle, and a seek slider" as its
  additions over stock — <https://fluentflyout.com/>). UNCERTAIN at the
  margins (absence claim from secondary sources); the runbook records what
  the box actually shows.
- Win11 24H2 lock screen: media card with progress bar + time labels
  (Insider 26120.1843 → KB5043145/KB5043178).
  <https://blogs.windows.com/windows-insider/2024/09/20/announcing-windows-11-insider-preview-build-26120-1843-dev-channel/>
- Seek/shuffle/repeat REQUESTS still arrive regardless of stock UI — from
  Phone Link, Bluetooth AVRCP, and third-party flyouts. Our handlers serve
  those.
- Media keys: with an SMTC session registered and `Playing` reported,
  hardware media keys and BT button events route to `ButtonPressed` globally.
  Arbitration between several sessions is undocumented ("the session the
  system believes the user would most likely want to control").

### 0.2 Current repo facts

The integration already exists end to end and shipped in v1.5.9
(`git ls-tree v1.5.9 src/media/smtc.rs` → present; introduced by commit
`c325351` "MacOS Control Center").

- `src/media/mod.rs` — platform-independent facade. `MediaSession::publish`
  diffs a `MediaSnapshot` into `MediaChanges` facets and forwards only what
  changed; inbound `MediaCommand`s flow through the normal reducer
  (`App::apply_media`, `src/app/media_reducer.rs`). Lazy activation
  (`EAGER=false` on Windows): no session until the first *playing* snapshot,
  so launching `ytt` never steals media keys.
- `src/media/smtc.rs` (~510 lines) — dedicated `smtc-worker` thread; invisible
  top-level window (never `GetConsoleWindow`, never `HWND_MESSAGE`);
  `RoInitialize(RO_INIT_MULTITHREADED)`; `GetForWindow` via
  `windows::core::factory`; handlers: ButtonPressed
  (Play/Pause/Stop/Next/Previous), PlaybackPositionChangeRequested,
  ShuffleEnabledChangeRequested, AutoRepeatModeChangeRequested; timeline with
  Start/End/MinSeek/MaxSeek/Position + a 5 s thread-timer refresh while
  playing (SMTC does not interpolate); thumbnail from cache-file bytes via
  `InMemoryRandomAccessStream` + `DataWriter` + `CreateFromStream`; teardown
  posts `WM_QUIT` → Closed → handler removal → `SetIsEnabled(false)` →
  `DestroyWindow`. The `SetTimer` id-capture subtlety is already handled
  correctly (system-assigned id is the one `KillTimer` receives).
- Snapshot builders exist TWICE (parity-tested): TUI
  `App::media_snapshot()` (`src/app/media_reducer.rs`) and daemon
  `Engine::media_snapshot()` (`src/daemon/engine.rs`). Both already populate
  `album` and `rate` — the gaps below are purely in the SMTC adapter.
- Artwork: `src/media/artwork.rs` normalizes all art (YouTube thumbnails,
  embedded tag art) to a center-cropped ≤512² JPEG q85 disk cache. No WebP
  ever reaches SMTC. Size guidance is undocumented upstream; Chromium targets
  150 px, mpv 240 px — 512² JPEG is safely above and renders fine (browsers
  pre-scale for hygiene, not correctness).
- mpv is spawned as an external process in THREE places: `src/player/mpv.rs`
  (`--no-video --no-terminal --idle=yes --no-config …`, main audio engine —
  TUI path), `src/daemon/engine.rs` `ensure_player` (daemon; NOTE: respawns
  mpv on every stop→play cycle), and `spawn_video_overlay` in
  `src/app/mod.rs` (the Shift+V video overlay; deliberately does NOT pass
  `--no-config`).
- Config: `media_controls` (default true) + Settings → Playback toggle;
  daemon honors `YTM_NO_MEDIA_SESSION` (whitelisted through the daemon env
  clear in `src/util/process.rs`).
- Known issue (documented in `scripts/windows-daemon-smoke.ps1`): SMTC init
  wedges the daemon event loop in non-interactive/DETACHED_PROCESS sessions
  (CI); the smoke sets `YTM_NO_MEDIA_SESSION=1`. SEPARATELY, the Windows
  daemon smoke still hangs deterministically in CI (~31 min) even WITH that
  guard — cause OPEN, may or may not be media-session related; it is
  `continue-on-error` in `build.yml` since v1.5.9. Debugging it needs a real
  Windows box (CI logs never flush). This plan includes running that smoke on
  hardware as a diagnostic step, but fixing it is NOT a blocker for this
  cycle unless the cause turns out to be SMTC.
- Windows QA conventions: `scripts/windows-tray-manual-qa.ps1` (+ its
  `verify-` companion) — parameterized paths, evidence directory, structured
  steps. The SMTC QA script (§5.2) follows the same shape.
- Packaging: scoop manifest (`packaging/scoop/yututui.json.tmpl`) `depends`
  on `extras/mpv` (so every scoop user has a current mpv — this is what makes
  the duplicate-session gap real), creates a Start-Menu shortcut only for
  `yututray.exe`, and scoop shortcuts carry no AUMID property. install.ps1
  creates no shortcuts. `build.rs` embeds only an icon resource (no
  VERSIONINFO). `yututray` sets AUMID `io.github.ochi.yututui.tray` for
  itself (`src/desktop/platform/windows.rs`), but the SMTC session lives in
  the `ytt.exe` process (TUI or daemon child), which sets no AUMID today.
- CI builds Windows natively on `windows-latest` (clippy `--all-targets` +
  `cargo test`, `.github/workflows/build.yml`). From macOS only
  `cargo clippy --target x86_64-pc-windows-msvc` (no link/run) is available
  locally, with Homebrew llvm-rc on PATH for the icon embed.

### 0.3 Empirical probe result (2026-07-03, mpv 0.41.0 on macOS)

- `mpv --media-controls=no --version` → exit 0 (option accepted — the option
  is no longer Windows-only).
- `mpv --bogus --version` → exit 1 (unknown options before `--version` fail).
- `mpv --version --bogus` → exit 0 (options AFTER `--version` are not
  validated).

Consequence: a capability probe must place the flag BEFORE `--version`, and
no `cfg(windows)` gate is needed — the probe answers per-installation, and
suppressing mpv's own media session is correct on macOS too (mpv would
otherwise also compete with our `MPNowPlayingInfoCenter` session).

---

## 1. Product Definition

### 1.1 One-sentence target

When `ytt` plays on Windows, exactly one media session — named "YuTuTui!" with
our icon — appears in every stock Windows media surface with correct
metadata, artwork, transport buttons, and timeline, controllable from media
keys, Bluetooth, the flyouts, and the lock screen; and this is proven by a
repeatable QA script on real Win10 + Win11 before release.

### 1.2 Expected behavior per surface

| Surface | Expectation |
| --- | --- |
| Win11 Quick Settings media card | Art, title, artist, prev/play-pause/next work. App name/icon = YuTuTui! after identity registration (§4). No seek/shuffle UI expected (stock limitation). |
| Win10 volume flyout banner | Art, title, artist, prev/play-pause/next. No seek bar (stock limitation). |
| Win11 24H2 lock screen | Media card with progress bar + time labels advancing (5 s timeline pushes), controls work. |
| Keyboard media keys / BT AVRCP | Route to ButtonPressed once a session exists (after first play — `EAGER=false`). Next/prev honored per queue caps. |
| Phone Link / third-party flyouts (FluentFlyout etc.) | Seek (PlaybackPositionChangeRequested), shuffle, repeat, rate round-trip through the reducer. |
| Live radio | Station + on-air metadata, no scrubber (timeline cleared), art absent by design. |
| Quit / settings toggle off | Session disappears immediately (no ghost "executable name" entry). |

### 1.3 Non-goals (this cycle)

- Fixing the non-interactive/CI SMTC init wedge itself (the
  `YTM_NO_MEDIA_SESSION` escape hatch stays; interactive sessions are the
  product surface). Diagnosing the separate CI smoke hang gets a runbook
  step, not a commitment.
- Chromium-style lock-state management (hide SMTC N seconds after pausing
  while locked, etc.) — privacy-driven browser behavior, not music-player
  behavior.
- A playback-rate UI anywhere in Windows stock surfaces (none exists); we
  only seed/serve the API.
- SMTC Like buttons (the API has no such button; macOS keeps that
  exclusive).
- Packaged (MSIX) identity.

---

## 2. Gap Fixes

All five gaps live in the Windows adapter or the mpv spawn path; the facade,
snapshot builders, reducers, and the macOS/Linux backends need no changes.
Fake gaps explicitly REJECTED during adversarial review, recorded here so
they are not re-excavated: the `SetTimer` id capture (already correct); a
stale-thumbnail-on-same-path-rewrite case (unreachable: `diff()` gates
artwork on path equality and the cache writes each key once via
temp+rename); live-radio metadata churn causing a full display rebuild per
on-air song (minutes apart — acceptable; QA observation only).

### 2.1 G1 — Album title never reaches SMTC

`update_display` in `src/media/smtc.rs` sets Title and Artist but never
`MusicProperties.SetAlbumTitle`, although `MediaTrack.album` is populated by
both snapshot builders and the MPRIS backend already publishes it
(`xesam:album`). Fix: set AlbumTitle when `album` is `Some` and non-empty.
Stock flyouts show title/artist only, but album surfaces through GSMTC
consumers (Phone Link, our probe) and possibly lock-screen layouts.
Regression risk: none — additive property on a Windows-only path.

- [x] `SetAlbumTitle` in `update_display` (skip when absent/empty)

### 2.2 G4 — PlaybackRate never seeded

`SystemMediaTransportControls.PlaybackRate` is never set. Two consequences
(guide-documented): `PlaybackRateChangeRequested` will never fire, and
consumers that extrapolate position between our 5 s timeline pushes
(Phone Link, AVRCP displays, third-party flyouts; Chromium/Firefox always
push rate for exactly this reason) assume rate 1.0 — wrong when the user
changes mpv speed. The snapshot already carries `rate` (mpv `speed`, clamped
to a nonzero range) and `diff()` already flags `options` on rate change.

Fix, two parts:

- Set `smtc.SetPlaybackRate(self.snapshot.rate)` in `apply_inner`'s
  `changes.options || changes.track` branch — a session property, NOT part of
  `push_timeline` (avoids 5 s-tick churn). First publish seeds it via
  `MediaChanges::all()`.
- Register `PlaybackRateChangeRequested` → `MediaCommand::SetRate` (the
  reducer already implements SetRate incl. the MPRIS 0.0→pause rule) — and
  add the matching arm in teardown's name-keyed handler removal. The removal
  match has a silent `_ => Ok(())` fallthrough, so a forgotten arm leaks the
  handler with no error; this is called out to reviewers.

- [x] `SetPlaybackRate` on options/track changes
- [x] `PlaybackRateChangeRequested` handler + token + teardown arm

### 2.3 G6 — Teardown lacks ClearAll

Current teardown: Closed → remove handlers → `SetIsEnabled(false)` →
`DestroyWindow`. Firefox's field-tested cleanup adds metadata clearing and
warns (verbatim, WindowsSMTCProvider.cpp): an incompletely-sequenced cleanup
"would cause a problem where the SMTC wasn't clean up completely and show the
executable name." Fix: `DisplayUpdater().ClearAll()` + `Update()` AFTER
`SetPlaybackStatus(Closed)` and BEFORE `SetIsEnabled(false)` (a disabled
control may reject updater calls — order matters). All on the worker thread,
already strictly sequential.

- [x] ClearAll + Update in teardown, ordered before disable

### 2.4 G3 — mpv registers its own competing SMTC session

See §3.

### 2.5 G2 — "Unknown app" identity

See §4.

---

## 3. mpv Duplicate-Session Defense (G3)

Problem: mpv ≥ 0.39.0 ships its own SMTC provider that explicitly works in
audio-only/no-video mode (its own hidden window + thread), enabled by default
for `mpv.exe` CLI — which is exactly how all three of our spawn sites run it.
Result on any scoop install (scoop `depends` guarantees current mpv): TWO
sessions in the flyout — ours (correct) and mpv's (stream-URL garbage
metadata) — plus undocumented media-key arbitration between them. The same
now applies to macOS (mpv's Now Playing vs our `MPNowPlayingInfoCenter`):
the option is accepted on mpv 0.41 macOS (§0.3).

Design:

- **Capability probe, not version parsing.** Run once per process:
  `mpv --no-config --media-controls=no --version`; exit 0 ⇒ the option is
  supported. Version-string parsing breaks on git builds; the probe is exact.
  Argument order is load-bearing (§0.3): the flag must precede `--version`,
  otherwise mpv exits 0 without validating it and the probe lies.
- **Cache in a process-wide `OnceLock<bool>`** in `src/player/mpv.rs`. The
  daemon respawns mpv on every stop→play cycle (`ensure_player`), so a
  per-spawn probe would double process launches; OnceLock makes it exactly
  one extra ~10 ms `mpv --version` per app run, off the hot path (first
  playback spawn, itself inside `tokio::spawn`).
- **Insert `--media-controls=no` BEFORE the `YTM_MPV_EXTRA` block** in the
  arg list. mpv's last-option-wins rule then gives users a documented
  override: `YTM_MPV_EXTRA=--media-controls=yes`.
- **All three spawn sites**: `src/player/mpv.rs` (TUI player),
  `src/daemon/engine.rs` path reuses the same spawn builder, and
  `spawn_video_overlay` (`src/app/mod.rs`) — the overlay would otherwise
  register a duplicate whenever Shift+V is used.
- **No platform gate.** The probe self-answers per installation; passing
  the flag where supported is correct on every OS, and mpv < 0.39 simply
  never gets the flag (and has no SMTC to suppress anyway).

Rejected alternatives: try-spawn-then-retry-without-flag (failure only
observable through the IPC connect-retry window, ambiguous with every other
spawn failure); IPC `set_property media-controls` after connect (runtime
mutability unverifiable from macOS; init-time provider); do-nothing
(guaranteed duplicate for every scoop user).

- [x] OnceLock capability probe helper in `src/player/mpv.rs`
- [x] Flag applied in main spawn (before `YTM_MPV_EXTRA`)
- [x] Flag applied in `spawn_video_overlay`
- [x] README override note (`YTM_MPV_EXTRA=--media-controls=yes`)

## 4. App Identity (G2)

Problem: an unpackaged exe with no registered identity shows as
**"Unknown app" with no icon** in the media flyout (souvlaki #67; mpv PR
review observed the same, including getting stuck with a stale other-app
icon). `ytt.exe` — the process that owns the SMTC session in both TUI and
daemon modes — has no AUMID, no VERSIONINFO, and no Start-Menu shortcut.

Design (three explicit parts + a fallback decision step):

1. **Process AUMID.** Call
   `SetCurrentProcessExplicitAppUserModelID("io.github.ochi.yututui")` early
   in `ytt`'s Windows startup (before any UI/session exists, per the API
   contract). New constant — deliberately NOT reusing the tray's
   `io.github.ochi.yututui.tray`: in tray mode the SMTC session lives in the
   ytt.exe daemon child, not the tray process, so reuse buys nothing and
   risks taskbar-grouping confusion. The daemon single-instance guard means
   at most one ytt SMTC session exists, so TUI/daemon/desktop coexistence
   has no AUMID conflict.
2. **VERSIONINFO resource.** Extend `build.rs`'s generated `.rc` with a
   VERSIONINFO block: FILEVERSION/PRODUCTVERSION generated from
   `CARGO_PKG_VERSION` (never hardcoded), FileDescription "YuTuTui!",
   ProductName "yututui". One crate-wide `.res` is linked into BOTH binaries,
   so the strings stay binary-neutral. This fixes Task-Manager naming and
   any identity fallback that reads the exe's version strings.
3. **Opt-in identity registration (mpv `--register` precedent).** A hidden
   idempotent `ytt` maintenance command (doctor family) writes
   `HKCU\Software\Classes\AppUserModelId\io.github.ochi.yututui` with
   `DisplayName` = "YuTuTui!" and `IconUri` → the installed `yututui.ico`.
   Invoked from `install.ps1` and a new scoop `post_install`. Explicitly NOT
   auto-run at ytt startup: daemon/SSH/CI invocations must not silently write
   registry keys, and the smoke scripts' profile isolation must stay intact.
   Uninstall leaving the HKCU key behind is accepted and documented (it is
   inert).
4. **Ship the .ico.** `IconUri` needs a real file on disk; today the icon
   exists only as an embedded resource. Add `assets/icons/yututui.ico` to the
   Windows release zip (build.yml packaging step) so scoop installs carry it.

Residual uncertainty, resolved empirically in Phase 3: no Microsoft doc
guarantees which store the flyout reads for unpackaged-app identity (the
AppUserModelId registry key is documented for toast senders; the
Start-Menu-shortcut route is what Chrome/Spotify effectively use). The QA
runbook has an explicit decision step: if the registry registration alone
does not fix the flyout name/icon, measure the fallback — a Start-Menu
`.lnk` carrying the `System.AppUserModel.ID` property (PowerShell snippet in
the QA script) — and adopt whichever works before release.

- [x] AUMID const + `SetCurrentProcessExplicitAppUserModelID` in ytt startup (Windows)
- [x] VERSIONINFO in build.rs from `CARGO_PKG_VERSION`
- [x] Idempotent registration command (doctor family)
- [x] install.ps1 step + scoop `post_install`
- [x] .ico shipped in Windows zip
- [x] Phase-3 decision: registry-only vs .lnk fallback → **adopt the .lnk**
      (see below + `windows-smtc-qa-results-2026-07-04.md`)

**Phase-3 decision (on-hardware, Win11 26200, 2026-07-04):** registry-only is
**insufficient** — the flyout stayed "Unknown app" even after a sign-out/in. A
Start-Menu shortcut carrying `System.AppUserModel.ID` = `io.github.ochi.yututui`
(PKEY_AppUserModel_ID {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3},5), created via
IShellLinkW + IPropertyStore, flipped the flyout to "YuTuTui!" + icon
**immediately, no logout**. **DONE (windows-gui branch, 2026-07-04):**
`register-media-identity` (`src/media/identity.rs::write_start_menu_shortcut`)
now writes the stamped shortcut alongside the HKCU key (kept for toast
identity), idempotent, with an AUMID readback guard; verified on-hardware via
independent Shell-COM readback of the freshly created `.lnk`. Full results +
implementation notes in `windows-smtc-qa-results-2026-07-04.md`.

## 5. Verification Tooling

### 5.1 `examples/smtc-probe.rs` — GSMTC consumer probe

A Windows-only example binary that reads back what the OS actually sees —
turning "look at the flyout" into machine-checkable JSON, and driving
commands INTO our session the same way Phone Link does.

- Dependency isolation: `windows` crate feature `Media_Control` added under
  `[target.'cfg(windows)'.dev-dependencies]` ONLY — dev-deps do not
  participate in `cargo build --bin`, so the release binaries' feature set
  is untouched.
- Cross-platform compile safety: CI runs `clippy --all-targets` on
  mac/linux runners too, so the example needs a `#[cfg(not(windows))]`
  stub `main`.
- Functions: `list` (default) dumps every GSMTC session as JSON —
  `SourceAppUserModelId`, title/artist/album, playback status, timeline
  (start/end/position/last-updated), playback rate, thumbnail presence;
  `pause|play|next|prev|seek <secs>` sends the corresponding
  `Try*Async` at the yututui session (matched by AUMID, else exe name).
- Used by the QA script for round-trip assertions:
  probe sends pause → `ytt -r status --json` must report paused → probe
  `list` must show Paused status; probe seek → status position matches.

- [x] example + dev-deps wiring + non-Windows stub

### 5.2 `scripts/windows-smtc-manual-qa.ps1` — the QA harness

Follows `windows-tray-manual-qa.ps1` conventions: `-Target/-Profile/
-YttPath/-EvidenceDir` params, strict mode, evidence directory with
timestamped transcript + probe JSON snapshots + screenshot prompts,
numbered steps with PASS/FAIL prompts for the manual items.

Machine-verified per gap (fails loudly, evidence saved):

| Gap | Assertion |
| --- | --- |
| G3 | While ytt plays, GSMTC session count attributable to us/mpv == 1; plus a standalone capability-probe step (`mpv --no-config --media-controls=no --version` exit code). |
| G2 | Our session's `SourceAppUserModelId` == `io.github.ochi.yututui`. |
| G1/G4 | Probe JSON shows the expected album string and playback rate ≠ null (and == mpv speed after a speed change). |
| G6 | After `ytt` quits, the session is gone from the probe list. |
| Round-trip | probe pause/next/seek ↔ `ytt -r status --json` agreement. |

Manual visual checklist (each with an evidence screenshot):

1. First play → session appears (and NOT before first play — `EAGER=false`).
2. Flyout name + icon (the G2 decision step: registry-only first, `.lnk`
   fallback measured if needed).
3. Artwork correct; changes on track change; rapid next-next-next settles on
   the final track's art/metadata.
4. Win11 lock screen (24H2): card + progress bar advancing; controls work.
5. Keyboard media keys; Bluetooth headset play/pause/next.
6. Settings → Playback → OS media controls OFF → entry disappears; ON →
   next play re-registers (window+session recreate — termusic risk point).
7. Queue end: no ghost/empty entry (record observed behavior; if a dead
   card lingers, file it — candidate fix: Closed status or disable on idle).
8. Live radio: station + on-air text, no scrubber.
9. Sleep → resume: session intact or cleanly re-established.
10. Task-Manager kill: no permanent ghost entry (record shell behavior).
11. Coexistence: Chrome playing YouTube simultaneously — both sessions
    listed, key routing follows the active one.
12. Scenarios ×3: `ytt` standalone TUI; `ytt` daemon headless (spawned
    detached from a shortcut/Run, still an interactive session); daemon +
    `yututray` tray.
13. Both terminals where relevant: Windows Terminal AND classic conhost.
14. Diagnostic (non-blocking): run `scripts/windows-daemon-smoke.ps1` on the
    box — it hangs in CI even with `YTM_NO_MEDIA_SESSION=1` (cause OPEN,
    see §0.2); capture where it stops locally. Fixing is out of scope unless
    the cause is SMTC.

- [x] script written (machine assertions + checklist + evidence)
- [ ] `verify-windows-smtc-manual-qa.ps1` companion (self-test) — only if
  cheap; the tray QA precedent has one

## 6. Manual QA Runbook (Phase 3)

Hardware matrix (user decision: BOTH OSes; Win10 best-effort since it is
past EOL — not a release blocker):

| Box | Priority | Terminals | Scenarios |
| --- | --- | --- | --- |
| Windows 11 (23H2/24H2, note build) | Release blocker | Windows Terminal + conhost | TUI / daemon / daemon+tray |
| Windows 10 22H2 | Best effort | Windows Terminal + conhost | TUI / daemon |

Procedure per box:

1. Build or copy `target\x86_64-pc-windows-msvc\release\{ytt.exe,
   yututray.exe}` + `examples\smtc-probe.exe`, or unpack the CI zip.
2. `scoop install extras/mpv main/yt-dlp` (or verify versions; record
   `mpv --version`).
3. Run `scripts/windows-smtc-manual-qa.ps1` → follow prompts → collect the
   evidence directory.
4. G2 decision step (§4) on the first box.
5. File every FAIL as a fix-loop item; re-run the affected steps after each
   fix (fixes re-pass Phase-1 gates first).

Exit criteria: all machine assertions PASS on Win11; manual checklist items
PASS or consciously waived with a note; evidence directories archived.

## 7. Release (Phase 4)

- Version pins — SIX places: `Cargo.toml` `version`, `Cargo.lock`
  (`cargo update -p yututui --precise <ver>`), `flake.nix` ×3 pins,
  `gui/package.json` (+ its lock) per the flake "keep in sync" comment.
- Full local gates (fmt, clippy native + `--features desktop` +
  cross-target, tests incl. daemon parity, `cargo deny`/`audit`,
  `check-architecture.sh`, `check-ratatui-image-patch.sh`), then push to
  `main` first (CI full matrix runs without releasing), then tag `v*`.
- CI packaging must now include `yututui.ico` in the Windows zip (§4.4) —
  verify in the artifact before publishing.
- The daemon smokes stay `continue-on-error` (pre-existing; §0.2). The
  registration command is opt-in, so CI stays hermetic;
  `windows-daemon-smoke.ps1` keeps `YTM_NO_MEDIA_SESSION=1`.
- README (en/ko/ja): the feature is already documented; add one line each
  for the `YTM_MPV_EXTRA=--media-controls=yes` override and the identity
  registration step. Post-release: scoop autoupdate → install on a clean
  box → registration → final flyout identity check.

- [ ] Version bump ×6 + gates + tag
- [ ] Zip contains .ico (CI artifact check)
- [ ] README notes (en/ko/ja)
- [ ] Post-release scoop-path identity check

## 8. Risks / Open Questions

1. **G2 mechanism is undocumented practice.** The registry AppUserModelId
   route may not rename the flyout entry; mitigation is the explicit Phase-3
   decision step with the `.lnk` fallback (§4). Worst case both fail →
   "Unknown app" persists → document and open an upstream question; all
   functional behavior still works.
2. **Win10 hardware/time.** Best-effort only; Win11 gates the release.
3. **CI Windows daemon smoke hang (pre-existing, cause OPEN).** Runbook
   step 14 gathers on-hardware data; fixing is a separate work item unless
   SMTC turns out to be the cause.
4. **mpv option drift.** `--media-controls` semantics changed once already
   (choice → bool); the capability probe is robust to that, and the
   `YTM_MPV_EXTRA` override is the pressure valve.
5. **Settings-toggle cycle.** termusic observed silent SMTC death after
   window destruction; our full teardown+recreate is structurally different
   but only hardware can prove it (checklist item 6).
6. **Interactive-session-only verification.** The probe and SMTC both
   require an interactive desktop — QA cannot move to CI later; it stays a
   scripted manual runbook by design.

## 9. Rollout Checklist

- [x] Phase 0 — this document
- [x] Phase 1 — G1 album title
- [x] Phase 1 — G4 playback rate (seed + handler + teardown arm)
- [x] Phase 1 — G6 teardown ClearAll
- [x] Phase 1 — G3 probe + three spawn sites + README note
- [x] Phase 1 — G2 AUMID + VERSIONINFO + register command + installer/scoop
      + zip icon
- [x] Phase 1 — gates green (fmt, clippy native/desktop/cross, tests)
- [x] Phase 2 — smtc-probe example
- [x] Phase 2 — windows-smtc-manual-qa.ps1
- [ ] Phase 3 — Win11 QA pass (blocker) + G2 decision recorded
- [ ] Phase 3 — Win10 QA pass (best effort)
- [ ] Phase 3 — fix loop drained; re-verified
- [ ] Phase 4 — release (version ×6, gates, tag, zip icon check, README)
- [ ] Phase 4 — post-release scoop identity check

(Phase 1/2 boxes above are ticked as the corresponding commits land; boxes
left unticked at doc-commit time track the remaining work.)
