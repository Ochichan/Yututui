# yututui crossterm Patch Notes

This directory vendors `crossterm` 0.29.0 through the root `[patch.crates-io]` entry. The fork is
intentionally narrow. Most changes live in the Unix event source and parser; one additive cursor
probe API is exposed so yututui can distinguish recent user input from terminal loss.

## Upstream Base

- Crate: `crossterm`
- Version: `0.29.0`
- crates.io archive SHA-256: `d8b9f2e4c67f833b660cdb0a3523065869fb35570177239812ed4c905aeff87b`
- Upstream Git revision recorded by crates.io: `36d95b26a26e64b0f8c12edfe11f410a6d56a812`
- Root override: `Cargo.toml` -> `[patch.crates-io] crossterm = { path = "crates/crossterm" }`

## Local Patch

- Parse Windows Terminal's `CSI Vk;Sc;Uc;Kd;Cs;Rc_` win32-input-mode records on Unix.
- Use the virtual-key identity for control characters so `Ctrl+Backspace` and `Ctrl+H` remain
  distinct, while printable Unicode continues to come from the active keyboard layout.
- Preserve key press/release/repeat, modifier, lock, keypad, navigation, function, and media-key
  information in crossterm's existing `KeyEvent` representation.
- Accept the protocol's omitted/default parameters and wait for the final byte when the first
  parameter is omitted.
- Apply six behavior-neutral lint cleanups in upstream terminal/event/Unix/example code so 0.29.0
  remains warning-free under the repository's newer `-D warnings` toolchain.
- Keep the vendored crate's upstream standalone `Cargo.lock` visible to Git.
- Keep an empty local `[workspace]` boundary so standalone fmt/clippy/test commands do not inherit
  yututui's parent workspace while the fork remains excluded from its members.
- Serialize the upstream ANSI formatting/round-trip tests around their process-global color flag,
  and mutate `NO_COLOR` through the shared test environment guard so inherited state is restored.
- Propagate keyboard-enhancement polling errors instead of retrying forever, keeping terminal
  capability probing bounded and allowing the application to select its conservative fallback.
- Open an independent `O_NONBLOCK | O_CLOEXEC | O_NOCTTY` event-input descriptor for the same TTY
  instead of changing or duplicating inherited stdin. Both Mio and `use-dev-tty` use the shared
  descriptor, read loop, and parser.
- Give each `try_read` call one shared 64 KiB / 50 ms drain budget across every
  `read -> WouldBlock -> poll -> read` cycle. This is required on PTYs such as macOS, where one
  writer operation is exposed as many short readiness bursts. When the shared budget is exhausted,
  retain an undrained edge when necessary and yield so a long incomplete paste, spurious readiness,
  or repeated `EINTR` cannot look like a wedged input worker. Propagate EOF/HUP/EIO and other
  permanent read failures, retain simultaneous TTY/SIGWINCH/waker readiness, and preserve
  event-source initialization errors. Deterministic fragmented-readiness tests cover both Unix
  backends, and PTY regressions have parent wall-clock deadlines so a future blocking-read
  regression cannot hang the test suite.
- Recover from abandoned UTF-8/CSI prefixes without consuming the first byte of the next event.
  A lone legacy ESC gets a 100 ms ambiguity window so a syscall split immediately after the
  prefix does not corrupt CSI/focus/CPR input. A second ESC preserves the first as a key and starts
  a new ambiguity window, so a quick Esc followed by a focus/CSI sequence loses neither event.
  On reader resume, both Unix backends give a continuation already queued in the TTY one
  nonblocking drain opportunity before expiring its prefix, so a scheduler stall cannot split a
  complete control sequence. Generic pending input expires after one idle second. Bracketed paste
  expires as a paste event after three idle seconds and is capped at 16 MiB.
- Add `cursor::probe_position_with(&mut impl Write, Duration) -> CursorPositionProbe`. It uses the
  caller's writer, purges stale replies, defers without writing when incomplete or complete recent
  input already proves client activity, and uses one absolute response deadline. Preserved input
  stays queued for the normal event reader. Because the terminal protocol has no query identifier,
  the first cursor reply parsed after the successful write wins; a reply that arrives on the wire
  after the pre-query purge is inherently indistinguishable. The original `cursor::position()`
  remains available.
- Add `terminal::supports_keyboard_enhancement_with(&mut impl Write)` and its timeout-taking
  counterpart so startup capability probes can use the application's bounded terminal writer and
  share one absolute deadline across request output and response polling. The legacy no-argument
  wrapper remains and preserves its two-second policy.
- Port the bounded cursor poll/read error behavior from upstream PR
  <https://github.com/crossterm-rs/crossterm/pull/1067> and the EOF/EIO intent tracked in issue
  <https://github.com/crossterm-rs/crossterm/issues/793>. The independent nonblocking input and
  parser resynchronization remain local additions not covered by that upstream PR.
- Remove the Unix `tput` size fallback. Resize handling now uses only bounded ioctl calls, avoiding
  an unbounded subprocess from the SIGWINCH/event path (upstream issue
  <https://github.com/crossterm-rs/crossterm/issues/422>).

The protocol carries `UnicodeChar` as one UTF-16 code unit. The narrow parser rejects isolated
surrogates rather than emitting invalid text; Konsole sends IME and multi-character commits through
its normal Unicode path. Supporting paired surrogate records would require state in both upstream
Unix event-source implementations and is intentionally outside this patch.

All local source changes include a nearby `yututui patch` comment. CI runs
`scripts/check-crossterm-patch.sh` to catch path-patch drift, base-version drift, or removal of the
parser and its core regression test. The same check hashes every vendored file (excluding ignored
`target/` build artifacts), including upstream base files and the documented local patches, so
unreviewed source drift also fails.

After an intentional, reviewed change to this vendored tree, print the new digest with
`scripts/check-crossterm-patch.sh --print-tree-digest` and update `expected_tree_digest` in that
script. Never rebless the digest merely to make an unexplained check failure pass.

## Upgrade Checklist

1. Replace this directory with the desired upstream crossterm release.
2. Verify the new release does not already support win32-input-mode on Unix, independently opened
   nonblocking TTY input, bounded EOF/error handling, parser resynchronization, and typed cursor
   deferral.
3. Reapply only the remaining local parser/event-source/cursor patches and their PTY tests.
4. Update the version, archive checksum, upstream revision, and invariant script.
5. Run:

```sh
scripts/check-crossterm-patch.sh
cargo fmt --manifest-path crates/crossterm/Cargo.toml --all --check
cargo clippy --manifest-path crates/crossterm/Cargo.toml --all-targets -- -D warnings
cargo clippy --manifest-path crates/crossterm/Cargo.toml --all-targets --features libc -- -D warnings
cargo clippy --manifest-path crates/crossterm/Cargo.toml --all-targets --no-default-features --features events,bracketed-paste,use-dev-tty -- -D warnings
cargo clippy --manifest-path crates/crossterm/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --manifest-path crates/crossterm/Cargo.toml
cargo test --manifest-path crates/crossterm/Cargo.toml --features libc
cargo test --manifest-path crates/crossterm/Cargo.toml --no-default-features --features events,bracketed-paste,use-dev-tty
cargo test --manifest-path crates/crossterm/Cargo.toml --all-features
```
