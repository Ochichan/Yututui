# Terminal Compatibility

Status: initial public-beta matrix, updated 2026-07-21. Entries marked
`Expected` still need a dated ytm-tui smoke run before they are marketed as
fully verified.

ytm-tui probes terminal capabilities and falls back where possible. Terminal
graphics, text zoom, mouse reporting, CJK/IME behavior, and video overlay
support are owned jointly by the terminal emulator, OS session, font, shell, and
ytm-tui configuration.

## Status Symbols

- `Yes`: supported and verified in ytm-tui.
- `Expected`: the terminal documents the required capability, but ytm-tui still
  needs a recorded smoke run.
- `Fallback`: ytm-tui has a degraded path such as halfblocks or retro ASCII.
- `No`: unsupported by the terminal or not meaningful in that environment.
- `Unknown`: not verified; README and release copy must not claim support.
- `Versioned`: support depends on the documented terminal version.

## Matrix

| Terminal | Album Art | Mouse | CJK / IME | Retro | Video Overlay | Text Zoom | ytm-tui Notes |
|---|---|---|---|---|---|---|---|
| Kitty | Expected via Kitty graphics | Expected | Expected; ytm-tui avoids all-keys enhancement to preserve IME | Yes | Expected on GUI OS with mpv | Versioned: OSC 66 on kitty >= 0.40 | Best target for full protocol path. |
| iTerm2 | Expected via iTerm2 graphics | Expected | Expected | Yes | Expected on macOS with mpv | Unknown until a ytm-tui probe run records OSC 66 or DECDHL behavior | Strong environment hint: `TERM_PROGRAM=iTerm.app`. |
| WezTerm | Expected via iTerm2, Kitty, or Sixel | Expected | Expected | Yes | Expected on GUI OS with mpv | Unknown until a ytm-tui probe run records OSC 66 or DECDHL behavior | WezTerm documents iTerm2, Kitty, and Sixel image protocols. |
| Windows Terminal | Versioned: Sixel in v1.22+ | Expected | Versioned: v1.22 added grapheme cluster work and improved IME paths | Yes | Expected in a desktop session with mpv | Expected through the `WT_SESSION` DECDHL path; needs smoke evidence | Use Microsoft release notes, not stale Sixel trackers. |
| cmd.exe inside Windows Terminal | Same as Windows Terminal | Same as Windows Terminal | Same as Windows Terminal | Yes | Same as Windows Terminal | Same as Windows Terminal | Shell is `cmd`; terminal capability is Windows Terminal. |
| Legacy conhost / bare cmd.exe | Unknown / Versioned | Expected partial | Version-dependent | Yes | Expected only in a desktop session with mpv | Unknown | Do not promise without a Windows build and terminal version. |
| Ghostty | Expected via Kitty graphics | Expected | Expected; grapheme clustering is documented | Yes | Expected on macOS/Linux with mpv | Unknown unless OSC 66 lands or DECDHL probe passes | Windows support must be verified separately. |
| foot | Expected via Sixel | Expected | Expected; package descriptions document IME through text-input-v3 | Yes | Expected on Linux GUI with mpv | Unknown | Wayland-only in normal use; verify on the target compositor. |
| Konsole / Yakuake | Versioned: < 26.04 defaults to halfblocks; >= 26.04 is best-effort via capability-gated Sixel | Expected | Expected | Yes | Expected on Linux GUI with mpv | Unknown | Yakuake inherits KonsolePart's terminal behavior. Sixel is selected only when DA1 advertises it and a real cell size is obtained; Kitty is not recommended. |
| mintty | Expected via Sixel | Expected | Expected | Yes | Expected on Windows desktop with mpv | Unknown | Probe longer, but Sixel is the first override to try. |
| mlterm | Expected via Sixel | Expected | Expected | Yes | Expected on GUI OS with mpv | Unknown | Probe longer, but Sixel is the first override to try. |
| VS Code integrated terminal | Fallback: halfblocks | Expected | Expected | Yes | Expected only through the host desktop session | Unknown | Keep conservative fallback unless a specific VS Code build is verified. |
| Apple Terminal | Fallback: halfblocks | Expected | Expected | Yes | Expected on macOS with mpv | Unknown / likely No | No native image protocol is expected. |
| Bare Linux TTY | Fallback: retro ASCII or halfblocks | No for crossterm mouse capture | Limited by console font/input method | Yes | No | No | Recommend retro mode. |
| Alacritty | Fallback: halfblocks; no native Sixel baseline | Expected | Expected | Yes | Expected on GUI OS with mpv | Unknown / likely No | Alacritty is a daily-driver beta model, but graphics support is deliberately conservative here. |

## ytm-tui Detection Path

- Interactive startup first verifies that stdin (or its controlling `/dev/tty`)
  and stdout identify the same TTY. A split or non-TTY pair fails before raw mode
  and before any capability-query bytes are sent. After that preflight, album art
  uses `ratatui-image` to query Kitty, Sixel, iTerm2, and halfblock fallbacks.
- Detected KonsolePart versions older than 26.04, or detected sessions without a
  valid `KONSOLE_VERSION`, stay on halfblocks by default. Starting with 26.04,
  ytm-tui permits a Sixel probe and selects Sixel only when DA1 advertises the
  capability and the terminal returns a real cell size. Yakuake exports the
  embedded KonsolePart version through the same variable, so this also works
  when its `TERM` is a generic `xterm-256color`. A missing response or either
  failed check still falls back to halfblocks. Kitty is not part of the normal
  Konsole/Yakuake path.
- Sixel on KonsolePart 26.04-26.07 is best-effort: the upstream fix that clears
  lingering non-Kitty image placements during TUI redraws landed for
  [Konsole 26.08](https://github.com/KDE/konsole/commit/a05e38fc6aa28ccb0e7875c82bd4d7a0b4e26cf5).
  Set `YTM_TUI_IMAGE_PROTOCOL=halfblocks` if an older build leaves fragments.
- Text zoom uses OSC 66 when the probe succeeds, `WT_SESSION` / DECDHL where
  applicable, and otherwise stays at 100%.
- Keyboard input negotiates one of four modes: native Windows console events,
  Kitty keyboard protocol, Win32 input mode, or a conservative legacy fallback.
  Kitty enhancement intentionally omits `REPORT_ALL_KEYS_AS_ESCAPE_CODES` so
  Hangul/CJK text input can compose normally in search and DJ Gem fields.
- Mouse support is a ytm-tui setting plus terminal support for mouse reporting.
- Video overlay is an mpv GUI window. It is not meaningful on a bare Linux TTY
  or a headless SSH session.

## Keyboard Input Modes

`Ctrl+Backspace` and `Ctrl+H` are different keys, but the oldest terminal wire
format encodes both as the same `^H` byte. ytm-tui uses an exact input protocol
where the direct terminal supports one and otherwise fails safe:

| Environment | Input path | Ctrl+Backspace / Ctrl+H |
|---|---|---|
| Native Windows console | Console key events | Exact |
| Direct Kitty, Ghostty, foot, or compatible Alacritty | Kitty keyboard query | Exact when the query succeeds |
| Direct WezTerm or iTerm2 | Kitty keyboard query | Exact when the terminal's extended-key reporting is enabled |
| Direct Konsole / Yakuake 26.04+ | Kitty query, then Win32 input fallback | Expected exact; needs a recorded smoke run |
| Windows Terminal through WSL | Kitty query, then Win32 input fallback | Expected exact; needs a recorded smoke run |
| tmux, GNU screen, Zellij, SSH, or an unknown terminal | Legacy safe fallback | Ambiguous `^H` never navigates by default |

In Legacy mode, while **Delete previous word** remains bound to its factory
`Ctrl+Backspace` chord, ytm-tui reserves ambiguous `^H`: it deletes a word in
the active text editor and is ignored elsewhere, so it cannot unexpectedly
open the Player. Remapping or unbinding **Delete previous word** releases that
reservation and lets `Ctrl+H` follow the effective keymap again. This policy is
derived from the existing key settings; it adds no config field.

Automatic Kitty/Win32 negotiation is disabled inside multiplexers and SSH.
`YTM_TUI_KEYBOARD_ENHANCEMENT=0|1` can disable or force the Kitty query, and
`YTM_TUI_WIN32_INPUT=0|1` can disable or force the Win32 fallback. Forced modes
are advanced troubleshooting overrides; an unset variable is the recommended
automatic behavior. Normal exit, errors, and panic restore an enabled mode.
An uncatchable process termination such as SIGKILL cannot emit a restore
sequence, so reset or reopen the terminal if its key reporting remains altered.

## Terminal Lifetime Detection

Playback lifetime protection and terminal-attachment detection are separate
layers:

- `ytt` routes mpv launches through a private heartbeat guardian. POSIX builds also
  give mpv an inherited `fd://` IPC lease (mpv 0.33 or newer), Linux adds a
  `PR_SET_PDEATHSIG`, and Windows uses parent-only and guardian-only
  kill-on-close Job Objects.
- On recognized Unix direct PTYs, one exclusive input worker checks periodic
  cursor-position replies through an independently reopened nonblocking TTY
  descriptor. Partial Unicode, focus, IME, or other terminal input is retained
  without letting a second blocking read pin the liveness worker. This includes
  the Distrobox/Podman `conmon` case where the PTY endpoint remains open after
  its interactive client disappears.
- Supported tmux, GNU screen, and Zellij 0.40.1+ sessions are checked through their
  client-query CLIs as well as the terminal reply. A parsed no-client result is
  definitive and shuts down immediately. A missing, inaccessible, timed-out,
  or malformed query is ambiguous and must recur independently in that owner
  layer before shutdown. Distinct multiplexer layers keep independent evidence.
- On Windows, normal console control events trigger guarded shutdown. A ConPTY
  broker that deliberately keeps the console and `ytt` process alive after its
  visible client disappears is not distinguishable from a live client inside
  `ytt`.
- Repeated same-type tmux, GNU screen, or Zellij nesting is likewise not
  distinguishable through those tools' public client listings: an inner client
  can still appear attached to an outer session whose real client is gone. A
  retained tmux control-mode broker also cannot be distinguished from a visible
  iTerm2/tmux integration, so any well-formed tmux client row counts as attached.

Use `ytt daemon` when playback is meant to survive terminal detach. If playback
must instead stop with an opaque ConPTY, retained tmux control broker, or repeated
same-type tmux/Screen/Zellij host session,
run `ytt` under a host-side lifetime supervisor or lease that owns that
boundary.

Transient terminal silence is not itself terminal loss. Ambiguous cursor replies and
unusable owner-layer queries require two independent observations in the same evidence
domain. Liveness output-gate contention defers that probe; the owner frame/control
operation is instead governed by its own seven-second deadline. A successful reply or real
input clears terminal-transport suspicion; an attached result clears only its
matching multiplexer layer. EOF, HUP, EIO, broken pipe, and parsed multiplexer
no-client states remain immediate failures.
A worker that enters a syscall/library phase and makes no progress is stopped by
an independent eight-second hard watchdog. A delayed watchdog tick requests a
fresh liveness observation but does not, by itself, extend either that hard
deadline or an active output deadline; watchdog-thread starvation therefore
cannot be mistaken for proof that the whole process was suspended.

Interactive Unix output is also written through an independently reopened
nonblocking TTY. Frame and IME output has a seven-second end-to-end bound;
notifications use one second. Graphics, keyboard, and zoom negotiation share one
three-second startup deadline. Normal restoration gets one second and a panic or
single-restore emergency gets 150 ms for raw-mode ioctls and control output. A
repeated signal uses one 1,650 ms deadline measured from handler entry: up to
1,500 ms orders the active graphics query behind its cancellation fence, and the
final 150 ms is reserved for terminal restoration. Restoration runs while that
fence is held, so a query paused before raw-mode activation cannot resume and
overwrite a completed restore. A delayed restore worker cannot restart either
budget or emit controls after its original deadline. An owner write deadline or
definitive write failure is returned immediately rather than entering the
two-observation liveness policy. The inherited shell stdin/stdout flags are never
changed.

Termination streams are registered before the second-launch chooser, graphics
probe, or TUI can enter raw mode, and the same registration remains live through
final terminal restoration. The first signal requests cooperative shutdown; a
repeat signal restores the terminal best-effort under the emergency policy and
forces exit even if later persistence or actor teardown is wedged.

Run:

```sh
ytt doctor terminal --json
```

This command is a no-playback diagnostic. It reports environment-derived
terminal facts, the selected input/output backend, sanitized owner-layer types,
the liveness policy, and whether stdin/stdout name the same TTY. It does not
start mpv, initialize playback, read cookies, enter raw mode, send a cursor
query, run a multiplexer command, or write user config. Pane/session identifiers
and TTY paths are never included.

`ytt doctor terminal --json` also reports native image hints, the probe timeout
ytm-tui will use for that environment, any `YTM_TUI_IMAGE_PROTOCOL` override,
and override suggestions. Supported override values are `halfblocks`, `sixel`,
`kitty`, and `iterm2`.

Recommended first override by terminal:

| Terminal hint | First override | Other candidates |
|---|---|---|
| Kitty / Ghostty | `YTM_TUI_IMAGE_PROTOCOL=kitty` | None |
| iTerm2 | `YTM_TUI_IMAGE_PROTOCOL=iterm2` | None |
| WezTerm | `YTM_TUI_IMAGE_PROTOCOL=iterm2` | `kitty`, `sixel` |
| Windows Terminal | `YTM_TUI_IMAGE_PROTOCOL=sixel` | None |
| foot / mintty / mlterm | `YTM_TUI_IMAGE_PROTOCOL=sixel` | None |
| Konsole / Yakuake | `YTM_TUI_IMAGE_PROTOCOL=sixel` | None |
| Unknown native hint | `YTM_TUI_IMAGE_PROTOCOL=kitty` | `iterm2`, `sixel` |

On KonsolePart versions older than 26.04, manually forcing Sixel is an
experimental escape hatch: it bypasses the conservative default and may leave
stale or broken image fragments. Do not use Kitty as the normal Konsole or
Yakuake override. On 26.04 and newer, force
`YTM_TUI_IMAGE_PROTOCOL=halfblocks` to opt out if Sixel still misbehaves.

## Smoke Runbook

Use the project verify workflow, not ad hoc `cargo run`, when recording runtime
evidence.

1. Record terminal name, version, OS, shell, font, `$TERM`, `$TERM_PROGRAM`, and
   the output of `ytt doctor terminal --json`.
2. Enable album art and verify native image rendering or fallback.
3. Test click, double-click, right-click, wheel scroll, and Ctrl+wheel when
   supported.
4. In Search, type `alpha beta`; verify `Ctrl+Backspace` leaves `alpha ` without
   changing screens and `Ctrl+H` opens Player on an exact direct-terminal path.
5. Type Korean/Hangul and a CJK-width sample into search and verify composition.
6. Toggle retro mode and confirm CP437-safe rendering.
7. Open and close video overlay where a GUI session exists.
8. Test text zoom in and out and verify mouse hit targets under zoom.

## Verification Log

No `Yes` entries are recorded yet. Keep entries in `Expected`, `Versioned`,
`Fallback`, or `Unknown` until a dated smoke run is added here.

| Date | Terminal | Version | OS | Evidence | Result |
|---|---|---|---|---|---|
| 2026-07-07 | Initial matrix | N/A | N/A | Documentation-only baseline | No runtime verification recorded. |

## Beta Support Policy

| Surface | Promise During Public Beta |
|---|---|
| Local playback, search, library, downloads, scrobbling | Intended for daily use; bugs are treated as product bugs. |
| v7 remote one-shot protocol | Frozen and backward-compatible. |
| v8 sessions / GUI protocol | Additive where possible; breaking changes need explicit release notes until declared stable. |
| Config files | Additive migrations; avoid destructive changes. |
| Terminal graphics / zoom | Best-effort by terminal capability; this matrix owns support expectations. |
| AI / DJ Gem | Optional; may change faster due model and provider behavior. |

## References

- `ratatui-image`: Kitty, Sixel, iTerm2, and halfblock protocol support:
  https://crates.io/crates/ratatui-image
- Kitty graphics protocol:
  https://sw.kovidgoyal.net/kitty/graphics-protocol/
- Kitty text sizing protocol:
  https://sw.kovidgoyal.net/kitty/text-sizing-protocol/
- Kitty keyboard protocol:
  https://sw.kovidgoyal.net/kitty/keyboard-protocol/
- Windows console virtual-terminal input sequences:
  https://learn.microsoft.com/en-us/windows/console/console-virtual-terminal-sequences
- WezTerm features:
  https://wezterm.org/features.html
- Windows Terminal Preview 1.22 release:
  https://devblogs.microsoft.com/commandline/windows-terminal-preview-1-22-release/
- Windows Terminal v1.22 notes:
  https://github.com/microsoft/terminal/discussions/17809
- Ghostty features:
  https://ghostty.org/docs/features
- Alacritty beta/daily-driver positioning:
  https://github.com/alacritty/alacritty
- Sixel tracker, useful but not authoritative for Windows Terminal:
  https://www.arewesixelyet.com/
