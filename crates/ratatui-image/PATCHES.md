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
- Stateful protocols carry a `RenderScale` with separate desired/encoded values. Native zoom
  enlarges the raster using scaled font metrics while Halfblocks, iTerm2, and every scale-one path
  retain their established behavior.
- Zoomed Kitty uses one explicit `a=T,c=…,r=…,C=1` direct placement below TUI backgrounds instead
  of Unicode placeholders. Scale one stays on the byte-identical virtual-placement path.
- DECDHL-aware Sixel encoding doubles raster/clear rows while keeping `CSI X` at the logical width
  of double-width lines, so Konsole/Yakuake art follows the text grid without clipping.
- Threaded resize requests use process-global generations, preserve the latest desired
  `RenderScale` while ownership is in flight, reject previous-track completions, and immediately
  requeue a superseding scale without drawing stale bytes.
- An empty local `[workspace]` boundary keeps standalone fork gates independent from yututui's
  parent workspace while this crate remains excluded from the application workspace members.

All local code changes should include a nearby `yututui patch` comment. CI runs
`scripts/check-ratatui-image-patch.sh` to catch accidental removal of the path patch, base-version
drift, or missing patch markers.

## Upgrade Checklist

1. Replace this directory with the desired upstream `ratatui-image` release.
2. Reapply every local patch group above.
3. Keep `yututui patch` comments next to each local behavior change.
4. Update the upstream base version in this file and in `scripts/check-ratatui-image-patch.sh`.
5. Verify that Kitty scale one still emits virtual-placement bytes, zoomed Kitty emits exactly one
   direct anchor with explicit cell dimensions, and DECDHL Sixel clears logical columns across the
   scaled physical rows.
6. Verify both threaded races: a scale change during encode must requeue the latest scale without
   rendering the returned stale encoding, and an old protocol owner's response must be rejected by
   its replacement even when both are queued through the same serial worker.
7. Run:

```sh
scripts/check-ratatui-image-patch.sh
cargo test --manifest-path crates/ratatui-image/Cargo.toml --no-default-features --features crossterm,tokio
cargo test
```
