# Packaging — one-click install channels

These files are the **source of truth** for the external package repos. The `build`
workflow (`.github/workflows/build.yml`) renders them on every `v*` tag and pushes the
results out, so the tap / bucket / AUR package stay in sync with each release automatically.
**Edit the templates here — never the copies in the external repos** (CI overwrites them).

| File | Renders to | One command it powers |
| --- | --- | --- |
| `homebrew/ytm-tui.rb.tmpl` | `Ochichan/homebrew-tap` → `Formula/ytm-tui.rb` | `brew install Ochichan/tap/ytm-tui` |
| `scoop/ytm-tui.json.tmpl` | `Ochichan/scoop-bucket` → `bucket/ytm-tui.json` | `scoop install ytm-tui` |
| `aur/PKGBUILD.tmpl` | AUR → `ytm-tui-bin` | `yay -S ytm-tui-bin` |

Each renders from the release's `checksums.txt` (the SHA-256 of every archive), so the
published formula/manifest always points at the exact bytes of that release.

## What the CI does on a `v*` tag

After the `build` matrix and the `release` job (which publishes the archives, installer
scripts, and a combined `checksums.txt`), three jobs run:

- **`publish-homebrew`** — fills the version + the four macOS/Linux SHAs into the formula
  template and pushes it to the tap. The formula is a **prebuilt binary** install
  (`bin.install "ytt"`), so `brew install` no longer compiles Rust, and it now
  `depends_on` mpv, yt-dlp, **and ffmpeg**.
- **`publish-scoop`** — fills the version + the Windows SHA into the manifest and pushes it
  to the bucket. `suggest` became **`depends`** (`extras/mpv`, `main/yt-dlp`, `main/ffmpeg`),
  and `autoupdate.hash` reads `checksums.txt` so the ScoopInstaller bot can bump it too.
- **`publish-aur`** — substitutes the version into the PKGBUILD and hands it to the
  deploy-aur action, which runs `updpkgsums` (fills the real checksums), regenerates
  `.SRCINFO`, and pushes to `ytm-tui-bin`.

Each job **no-ops cleanly if its secret is missing**, so tagging works before you finish the
one-time setup below — the channels simply don't publish until their secret is present.

## One-time setup (you, the maintainer)

### 1. Secrets (repo → Settings → Secrets and variables → Actions)

- **`TAP_TOKEN`** — a *fine-grained* Personal Access Token with **Contents: Read and write**
  selected for **both** `Ochichan/homebrew-tap` and `Ochichan/scoop-bucket` (one token can
  cover multiple repos under your account). Used by `publish-homebrew` and `publish-scoop`.
- **`AUR_SSH_PRIVATE_KEY`** — the private half of an SSH key whose public half is registered
  on your AUR account (https://aur.archlinux.org → My Account → SSH Public Key). Used by
  `publish-aur`.

### 2. Repos

- `homebrew-tap` and `scoop-bucket` already exist — the first tagged run overwrites their
  formula/manifest with the prebuilt versions. No manual edit needed.
- AUR `ytm-tui-bin` does **not** exist yet. The AUR creates the package repo automatically on
  the **first authenticated push**, so once `AUR_SSH_PRIVATE_KEY` is set, the first `v*` tag
  creates and populates it. (You must have an AUR account; the SSH key ties the push to it.)

### 3. Windows users + the `extras` bucket

mpv lives in Scoop's **extras** bucket (yt-dlp and ffmpeg are in **main**, added by default).
So the manifest's `depends` on `extras/mpv` requires that bucket to be present. The README's
Windows command adds it first:

```powershell
scoop bucket add extras
scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket
scoop install ytm-tui
```

## Testing before the real tag

Per the release plan, exercise the whole pipeline on a throwaway pre-release first:

```sh
git tag vX.Y.Z-rc1 && git push origin vX.Y.Z-rc1
```

That runs the 5-leg build matrix, publishes a pre-release with `checksums.txt`, and fires the
three publish jobs — letting you confirm the tap/bucket/AUR pushes (and the unconfirmed
`macos-15-intel` / `ubuntu-22.04-arm` runner labels) before tagging the real `vX.Y.Z`. Delete the
pre-release + tag afterwards.
