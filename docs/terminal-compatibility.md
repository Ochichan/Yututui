# Terminal Compatibility

Status: initial public-beta matrix, updated 2026-07-07. Entries marked
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
| WezTerm | Expected via iTerm2, Kitty, or Sixel | Expected | Expected | Yes | Expected on GUI OS with mpv | Unknown until a ytm-tui probe run records OSC 66 or DECDHL behavior | WezTerm documents iTerm2, Kitty, and Sixel image protocols. |
| Windows Terminal | Versioned: Sixel in v1.22+ | Expected | Versioned: v1.22 added grapheme cluster work and improved IME paths | Yes | Expected in a desktop session with mpv | Expected through the `WT_SESSION` DECDHL path; needs smoke evidence | Use Microsoft release notes, not stale Sixel trackers. |
| cmd.exe inside Windows Terminal | Same as Windows Terminal | Same as Windows Terminal | Same as Windows Terminal | Yes | Same as Windows Terminal | Same as Windows Terminal | Shell is `cmd`; terminal capability is Windows Terminal. |
| Legacy conhost / bare cmd.exe | Unknown / Versioned | Expected partial | Version-dependent | Yes | Expected only in a desktop session with mpv | Unknown | Do not promise without a Windows build and terminal version. |
| Ghostty | Expected via Kitty graphics | Expected | Expected; grapheme clustering is documented | Yes | Expected on macOS/Linux with mpv | Unknown unless OSC 66 lands or DECDHL probe passes | Windows support must be verified separately. |
| foot | Expected via Sixel | Expected | Expected; package descriptions document IME through text-input-v3 | Yes | Expected on Linux GUI with mpv | Unknown | Wayland-only in normal use; verify on the target compositor. |
| Bare Linux TTY | Fallback: retro ASCII or halfblocks | No for crossterm mouse capture | Limited by console font/input method | Yes | No | No | Recommend retro mode. |
| Alacritty | Fallback: halfblocks; no native Sixel baseline | Expected | Expected | Yes | Expected on GUI OS with mpv | Unknown / likely No | Alacritty is a daily-driver beta model, but graphics support is deliberately conservative here. |

## ytm-tui Detection Path

- Album art uses `ratatui-image`, which can query Kitty, Sixel, iTerm2, and
  halfblock fallback protocols. If stdout is not a TTY, ytm-tui skips image
  probing and uses halfblocks.
- Text zoom uses OSC 66 when the probe succeeds, `WT_SESSION` / DECDHL where
  applicable, and otherwise stays at 100%.
- Keyboard enhancement intentionally omits `REPORT_ALL_KEYS_AS_ESCAPE_CODES` so
  Hangul/CJK text input can compose normally in search and DJ Gem fields.
- Mouse support is a ytm-tui setting plus terminal support for mouse reporting.
- Video overlay is an mpv GUI window. It is not meaningful on a bare Linux TTY
  or a headless SSH session.

Run:

```sh
ytt doctor terminal --json
```

This command is a no-playback diagnostic. It reports environment-derived
terminal facts and does not start mpv, initialize playback, read cookies, or
write user config.

## Smoke Runbook

Use the project verify workflow, not ad hoc `cargo run`, when recording runtime
evidence.

1. Record terminal name, version, OS, shell, font, `$TERM`, `$TERM_PROGRAM`, and
   the output of `ytt doctor terminal --json`.
2. Enable album art and verify native image rendering or fallback.
3. Test click, double-click, right-click, wheel scroll, and Ctrl+wheel when
   supported.
4. Type Korean/Hangul and a CJK-width sample into search.
5. Toggle retro mode and confirm CP437-safe rendering.
6. Open and close video overlay where a GUI session exists.
7. Test text zoom in and out and verify mouse hit targets under zoom.

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
