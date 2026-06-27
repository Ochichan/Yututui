# ytm-tui

A fast, low-RAM YouTube Music player for your terminal. Inspired by youtube-music-cli.

## Requirements

[`mpv`](https://mpv.io) and [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) must be on your `PATH`
for playback:

- **macOS:** `brew install mpv yt-dlp`
- **Windows:** `scoop install mpv yt-dlp`

## Install

```sh
# macOS / Linux
./install.sh

# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

Or with a package manager:

```sh
brew install Ochichan/tap/ytm-tui                                                          # macOS
scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket; scoop install ytm-tui   # Windows
```

The installer uses a prebuilt binary when one ships for your platform, otherwise it builds from
source — install [Rust](https://rustup.rs) first.

## Run

```sh
ytm
```

Press `?` in-app for the key list.

## License

MIT. Do whatever you want.
