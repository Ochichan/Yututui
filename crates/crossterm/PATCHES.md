# yututui crossterm Patch Notes

This directory vendors `crossterm` 0.29.0 through the root `[patch.crates-io]` entry. The fork is
intentionally narrow: public crossterm APIs are unchanged, and the local behavior lives in the Unix
event parser.

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
- Propagate keyboard-enhancement polling errors instead of retrying forever, keeping terminal
  capability probing bounded and allowing the application to select its conservative fallback.

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
2. Verify the new release does not already support win32-input-mode on Unix.
3. Reapply the local parser patch and its tests only if still needed.
4. Update the version, archive checksum, upstream revision, and invariant script.
5. Run:

```sh
scripts/check-crossterm-patch.sh
cargo fmt --manifest-path crates/crossterm/Cargo.toml --all --check
cargo clippy --manifest-path crates/crossterm/Cargo.toml --all-features --all-targets -- -D warnings
cargo test --manifest-path crates/crossterm/Cargo.toml --all-features
```
