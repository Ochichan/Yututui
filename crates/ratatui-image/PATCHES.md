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
- Konsole versions before 26.08 keep Kitty and Sixel capability queries disabled; 26.08 and newer
  may select Sixel only after DA1 advertises it and the cell-size query returns usable dimensions.

All local code changes should include a nearby `yututui patch` comment. CI runs
`scripts/check-ratatui-image-patch.sh` to catch accidental removal of the path patch, base-version
drift, or missing patch markers.

## Upgrade Checklist

1. Replace this directory with the desired upstream `ratatui-image` release.
2. Reapply the five local patch groups above.
3. Keep `yututui patch` comments next to each local behavior change.
4. Update the upstream base version in this file and in `scripts/check-ratatui-image-patch.sh`.
5. Run:

```sh
scripts/check-ratatui-image-patch.sh
cargo test --manifest-path crates/ratatui-image/Cargo.toml --no-default-features --features crossterm,tokio
cargo test
```
