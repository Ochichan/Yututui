# yututui ratatui-image Patch Notes

This directory vendors `ratatui-image` 11.0.6 through the root `[patch.crates-io]` entry.
The fork is intentionally narrow and should stay easy to rebase onto a future upstream release.

## Upstream Base

- Crate: `ratatui-image`
- Version: `11.0.6`
- Root override: `Cargo.toml` -> `[patch.crates-io] ratatui-image = { path = "crates/ratatui-image" }`

## Local Patches

- Kitty placeholder rendering emits explicit per-cell coordinates so album art survives popup
  overlap and partial redraws.
- Stateful Kitty can be constructed with a low z-index so graphics-protocol album art remains
  behind normal TUI cells.
- Kitty rows damaged by overlay redraws can be marked for retransmission.
- Sixel and iTerm2 encodes stamp the anchor cell with a monotonic redraw tag so freshly rebuilt
  protocols are not skipped by ratatui's diffing.
- KonsolePart versions before 26.04 keep Kitty and Sixel capability queries disabled. Konsole and
  Yakuake on 26.04 and newer may select Sixel only after DA1 advertises it and the cell-size query
  returns usable dimensions. This is best-effort on 26.04-26.07 because the upstream placement
  cleanup landed for 26.08; users can force `YTM_TUI_IMAGE_PROTOCOL=halfblocks` if fragments linger.
- An empty local `[workspace]` boundary keeps standalone fork gates independent from yututui's
  parent workspace while this crate remains excluded from the application workspace members.
- Unix capability queries reopen the stdout TTY with independent nonblocking flags, validate its
  device identity, and share one absolute deadline across raw-mode setup, partial request writes,
  response waits, and a 64 KiB-capped late-input drain. Raw-mode changes use immediate termios
  application instead of an unbounded output drain, and a raw-mode restoration failure is surfaced
  as a distinct fatal error rather than being downgraded to an unsupported-capability fallback. A
  writer-taking overload lets applications route probe bytes through their own serialized terminal
  output. An additive absolute-`Instant` overload accepts a shared one-way cancellation fence;
  cancellation plus an exclusive cleanup closure orders inherited-mode restoration after every
  admitted raw-mode activation, query output, input poll/read, and late-response drain; a bounded
  acquisition failure never runs that restore unfenced. Bounded polling and nonblocking-output
  retries observe cancellation promptly. Tmux detection is now read-only: it never launches or
  waits on a configuration-mutating `tmux set` subprocess.

All local code changes should include a nearby `yututui patch` comment. CI runs
`scripts/check-ratatui-image-patch.sh` to catch accidental removal of the path patch, base-version
drift, or missing patch markers. The check also hashes every vendored file except ignored `target/`
artifacts, so unreviewed base or patch drift fails even when a marker still exists.

After an intentional, reviewed change to this vendored tree, print the new digest with
`scripts/check-ratatui-image-patch.sh --print-tree-digest` and update `expected_tree_digest` in that
script. Never rebless the digest merely to make an unexplained check failure pass.

## Upgrade Checklist

1. Replace this directory with the desired upstream `ratatui-image` release.
2. Reapply the seven local patch groups above.
3. Keep `yututui patch` comments next to each local behavior change.
4. Update the upstream base version in this file and in `scripts/check-ratatui-image-patch.sh`.
5. Run:

```sh
scripts/check-ratatui-image-patch.sh
cargo test --manifest-path crates/ratatui-image/Cargo.toml --no-default-features --features crossterm,tokio
cargo test
```
