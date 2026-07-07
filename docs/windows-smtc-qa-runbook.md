# Windows SMTC — On-Hardware QA Runbook

Step-by-step manual for verifying the Windows media-session integration on a
real Windows 10/11 box. This is the operational companion to
`docs/windows-smtc-completion-plan.md` (background, gap list, design
decisions live there — this file is the one you follow at the machine).

Budget: **~40 minutes** on the first box (Win11), ~25 on the second (Win10).
Everything writes evidence into a timestamped folder under `target\` so a
failed run can be reported without re-doing it.

Verdict semantics: **Win11 passing gates the release. Win10 is best-effort**
(post-EOL) — record failures, don't block on them.

---

## 0. What you are verifying (30-second version)

`ytt` publishes an SMTC media session (hidden window + WinRT, `src/media/smtc.rs`).
Six things were never confirmed on hardware:

| # | Claim | Verified by |
| --- | --- | --- |
| 1 | Session appears with correct metadata + artwork; album (G1) and playback rate (G4) included | probe JSON (automated) |
| 2 | Exactly ONE session — mpv's own SMTC is suppressed (G3) | probe JSON (automated) |
| 3 | Flyout shows **YuTuTui!** + icon, not "Unknown app" (G2) | your eyes + decision step §5 |
| 4 | OS-side controls round-trip: pause/next/seek from GSMTC → ytt actually obeys | probe commands (automated) |
| 5 | Teardown is clean: quit / settings-toggle / daemon-stop remove the entry (G6) | probe JSON (automated) + eyes |
| 6 | Surfaces behave: media keys, Bluetooth, lock screen, live radio, coexistence with a browser | your eyes |

## 1. Machine prep

- Interactive desktop session required (physical or RDP). **Not ssh** — the
  GSMTC API and SMTC itself both refuse non-interactive sessions.
- Audio output device active. Optional but valuable: a Bluetooth headset
  with play/pause/next buttons.
- Windows version noted (`winver`): Win11 23H2/24H2 or Win10 22H2. The
  lock-screen progress bar only exists on Win11 24H2 (KB5043145+).
- Runtime tools (scoop path, mirrors what users get):

```powershell
scoop install extras/mpv main/yt-dlp
mpv --version   # record it; >= 0.39 is the interesting case for G3
```

- Repo checkout at commit `716e9e1` or later, Rust toolchain + VS Build
  Tools (same as any Windows dev build of this repo).

## 2. Build

Build with the explicit target triple — the QA script's default paths point
at `target\x86_64-pc-windows-msvc\release\`:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --example smtc-probe --target x86_64-pc-windows-msvc
```

Sanity check the probe before anything plays — with Spotify/a browser
playing something it must list that session; with nothing playing it prints
an empty list:

```powershell
target\x86_64-pc-windows-msvc\release\examples\smtc-probe.exe
```

If it errors immediately, you are in a non-interactive session — stop here.

## 3. Run the QA harness

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows-smtc-manual-qa.ps1
```

(Useful switches: `-SkipDaemon` for a quick TUI-only pass;
`-KeepIdentityRegistration` to leave the HKCU identity key installed after
the run; `-YttPath`/`-ProbePath`/`-EvidenceDir` to override paths.)

The script will, in order — **automated, no input needed**:

1. Refuse to start if `ytt.exe`/`mpv.exe` are already running.
2. Capture `mpv --version` and run the G3 capability probe
   (`mpv --no-config --media-controls=no --version` — flag before
   `--version`, that order is what makes the probe valid).
3. Register the media identity (`ytt register-media-identity`) and snapshot
   the HKCU key it wrote.
4. Assert no yututui session exists yet (lazy activation — `EAGER=false`).

Then it launches `ytt` in its own console window and **prompts you** to:

5. Play a catalog song **that has an album** (not a single, not radio) and
   queue several more (playing an album is the easy way). Albums matter
   because the G1 assertion checks the album field; a single would
   false-flag it.

From there the automated block asserts: session playing, AUMID equals
`io.github.ochi.yututui`, exactly one session / zero mpv sessions,
title+artist+artwork present, rate seeded, timeline sane
(`max_seek == end > 0`), then round-trips **pause → play → seek(30s) →
next** through the OS API and checks ytt obeyed each one.

Everything after that is the manual checklist — next section.

## 4. Manual checklist — what "good" looks like per surface

The script prompts each of these with a `[y/n]`; this section is what to
actually look at before answering.

### 4.1 The media surface

- **Win11**: `Win+A` (quick settings) — the media card sits above the
  sliders. Also appears on the volume-key OSD.
- **Win10**: press a volume key — banner appears top-left.

Expect: artwork thumbnail, correct title/artist, working ⏮ ⏯ ⏭. Expected
stock-UI limitations — do NOT file these as bugs:

- No seek bar in the Win10 banner or the Win11 quick-settings card
  (Microsoft never shipped one; third-party flyouts add it).
- No shuffle/repeat buttons in stock UI (the API round-trips exist and are
  exercised via the probe/Phone Link path instead).

### 4.2 Identity (the G2 check)

The card should say **YuTuTui!** with our icon. "Unknown app", a blank icon,
or a stale icon from another player = FAIL → answer `n` and continue; §5
tells you what to do after the run.

### 4.3 Media keys and Bluetooth

Focus a different app (notepad), then: keyboard play/pause, next. Then the
same from a Bluetooth headset if present. Both must control ytt while it is
in the background. (Before the first play of a session, keys must NOT
target ytt — that is the lazy-activation design, verified automatically.)

### 4.4 Lock screen

`Win+L` while playing. Win11: media card with controls; on 24H2 also a
progress bar + time labels that advance (5 s cadence — small jumps are
expected, not a bug). Win10: title/artist text, fewer affordances. Controls
must work from the lock screen.

### 4.5 Settings toggle cycle (window-destruction risk point)

The script walks you through Settings → Playback → *OS media controls* OFF
(entry must disappear) → ON + play (entry must return, fully functional).
This exercises full session+window teardown/recreation — the exact spot
where other players have seen SMTC silently die (termusic's field bug), so
take it seriously: after re-enabling, press a media key too.

### 4.6 Live radio

Play a radio station: card shows the station / on-air text, and **no
progress bar** (timeline deliberately cleared for live streams). Optional
prompt — answer `y` if you skipped it.

### 4.7 Rapid skips, queue end, coexistence (quick passes)

- Hammer next 4-5 times fast: the card must settle on the final track's
  metadata+art (no stuck stale art).
- Let the queue end (or clear it): no dead "ghost" card should linger; note
  what you see — this feeds an open design question (Stopped vs Closed).
- With Chrome/Edge playing a YouTube video at the same time: both sessions
  listed under volume controls; keys follow the most recent player.

### 4.8 Quit + hard kill

Normal quit (`q`): entry disappears immediately (automated assert + your
visual confirm). Then, outside the script if you want the extra credit:
start ytt again, play, kill it from Task Manager — the entry should drop
off within seconds; a permanently stuck ghost card = file it.

## 5. G2 decision step (only if identity FAILED in §4.2)

Order matters; capture evidence between each attempt
(`smtc-probe.exe > probe-identity.txt` + a screenshot):

1. Confirm the registry key the script wrote:
   `Get-ItemProperty "HKCU:\Software\Classes\AppUserModelId\io.github.ochi.yututui"`
   — DisplayName `YuTuTui!`, IconUri pointing at a real `.ico`.
2. Sign out and back in (shell caches identity resolutions), retest.
3. Plain Start-Menu shortcut experiment:

```powershell
$lnk = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\YuTuTui!.lnk"
$ws = New-Object -ComObject WScript.Shell
$sc = $ws.CreateShortcut($lnk)
$sc.TargetPath = "<repo>\target\x86_64-pc-windows-msvc\release\ytt.exe"
$sc.IconLocation = "<repo>\assets\icons\yututui.ico"
$sc.Save()
```

   Sign out/in, play, recheck the card.
4. Record which combination (registry only / +sign-out / +shortcut) fixed
   it — or that none did. **Do not hand-build an AUMID-property-stamped
   shortcut**; if the plain one doesn't do it, that property-stamping
   becomes a code follow-up (IPropertyStore) driven by this evidence, per
   plan doc §4. Delete the experimental `.lnk` afterwards.

## 6. Daemon pass + smoke diagnostic

The script's daemon block (skipped with `-SkipDaemon`) starts the daemon,
has you get playback going (`ytt -r resume` or `ytt -r play <query>`),
asserts the session appears from the **headless** process, then asserts it
disappears on `ytt daemon stop`.

Separately — after the QA run, in a fresh terminal, run the known-broken CI
smoke and simply observe:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\windows-daemon-smoke.ps1
```

It hangs in CI (~31 min) even with `YTM_NO_MEDIA_SESSION=1`; cause is OPEN
(plan doc §0.2). On a real desktop it may well pass — either outcome is
signal. Note where it stops if it stalls (last console line). This is a
diagnostic, **not** a gate for this release. Never run it mid-QA: it moves
your yututui profile directories around.

## 7. Evidence, reporting, sign-off

Everything lands in `target\windows-smtc-manual-qa-<timestamp>\`:
`results.json` (every automated + y/n outcome), `probe-*.txt` (each GSMTC
snapshot), `*.png` (your screenshots), `transcript.txt`.

- Any automated assertion failure throws immediately — keep the folder,
  note the step name, and map it back: identity → §5 / `src/media/identity.rs`;
  mpv duplicate → `src/player/mpv.rs` probe; album/rate/timeline →
  `src/media/smtc.rs`; round-trip failures → `src/app/media_reducer.rs`.
- Manual `n` answers don't stop the run; they are listed at the end and
  recorded in `results.json`.

Sign-off = on Win11: all automated asserts pass + every checklist item `y`
(or consciously waived with a note) + G2 decision recorded. Then release
per plan doc §7 — reminder: **version must bump to ≥ 1.6.0** (six pin
locations; install.ps1 gates the identity registration on 1.6.0) and the
zip must contain `yututui.ico` (CI verifies).

## 8. Fast re-test after a fix

No need to repeat the full runbook per fix: rebuild, then re-run the script
with `-SkipDaemon`, answer only the affected checklist item carefully, and
re-run the daemon pass only if the fix touched the daemon path. Fixes must
re-pass the local gates first (fmt, clippy incl. cross-windows, tests).
