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

## Downloads

Downloaded tracks are saved under the user Music folder by default:

- **macOS:** `~/Music/ytm-tui`
- **Windows:** `%USERPROFILE%\Music\ytm-tui`

You can override this in Settings (`Download dir`) or with `YTM_DOWNLOAD_DIR`.

## Cookies

Cookies are optional. Without a cookie file, `ytm` uses anonymous search and playback,
which works for public tracks. Add cookies only if you need signed-in YouTube Music
access, gated tracks, or downloads that require your browser session.

By default, `ytm` looks for a Netscape-format `cookies.txt` here:

- **macOS:** `~/Music/ytm-tui/cookies.txt`
- **Windows:** `%USERPROFILE%\Music\ytm-tui\cookies.txt`

Create the folder, export YouTube/YouTube Music cookies from your browser as
`cookies.txt`, then restart `ytm`.

```sh
# macOS
mkdir -p ~/Music/ytm-tui
```

```powershell
# Windows (PowerShell)
New-Item -ItemType Directory "$HOME\Music\ytm-tui" -Force
```

You can also override the path in Settings (`Cookies file`). Keep this file private:
it can act like a logged-in browser session.

## License

MIT. Do whatever you want.
