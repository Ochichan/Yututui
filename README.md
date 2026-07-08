# ytm-tui

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/Ochichan/ytm-tui)](https://github.com/Ochichan/ytm-tui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

YouTube Music in your terminal — fast, keyboard-driven, no browser tab eating your RAM, no ads. All behind a three-letter command: `ytt`. Rust + ratatui. MIT.

Public beta: stable enough for daily use, still moving fast.

### [▶ Live demo & the full feature tour → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

> 🖼️ *Demo GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/hero.gif, delete the "coming soon" line above, then uncomment:
![Search, play, real album art and synced lyrics in one terminal](docs/media/hero.gif)
-->

---

## Install

Each command installs `ytt` **and** its helpers (mpv, yt-dlp, ffmpeg) in one go.

| OS | One command |
| --- | --- |
| **macOS** | `brew install Ochichan/tap/ytm-tui` |
| **Windows** | `scoop bucket add extras; scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket; scoop install ytm-tui` |
| **Linux** — any, with [Nix](https://nixos.org/download) | `nix run github:Ochichan/ytm-tui` |
| **Linux** — Arch | `yay -S ytm-tui-bin` |
| **Linux** — any other | Download and run the installer below |
| **From source** | `./install.sh --build` (needs [Rust](https://rustup.rs)) |

```sh
curl -fsSL https://raw.githubusercontent.com/Ochichan/ytm-tui/main/install.sh | bash
```

Windows direct installer:

```powershell
irm https://raw.githubusercontent.com/Ochichan/ytm-tui/main/install.ps1 | iex
```

**Tray companion:** macOS and Windows releases include `ytt-desktop`, the menu-bar / notification-area mini player.

| Channel | What gets installed | How to start the tray |
| --- | --- | --- |
| macOS Homebrew | `ytt`, `ytt-desktop`, runtime tools | `ytt-desktop --background` |
| Windows Scoop | `ytt.exe`, `ytt-desktop.exe`, runtime tools, Start Menu shortcut | `ytt-desktop --background` or **YtmTui Tray** |
| Direct installers / source build scripts | `ytt`; macOS/Windows also get `ytt-desktop` | `ytt-desktop --background` |
| Linux | `ytt` with MPRIS media integration | no separate tray app |

Start-at-login is opt-in: `ytt-desktop --install-startup`.

> After a direct installer or source build, run `ytt doctor` to see what's missing.
> **yt-dlp keeps itself fresh.** YouTube changes weekly, so `ytt` maintains its own current yt-dlp (SHA-256-verified from github.com) and uses whichever of {managed, system} is newer. Check with `ytt tools status --why`, update with `ytt tools update`, or pin a known-good binary with `ytt tools use system|managed|<path>`.

## Terminal support

- Terminal support varies by emulator; ytm-tui probes capabilities and falls back where possible.
- Album art uses Kitty/Sixel/iTerm2 protocols when the terminal supports them, otherwise halfblocks or retro ASCII.
- Text zoom, CJK/IME behavior, mouse reporting, and video overlay depend on terminal and OS support.
- Check your environment with `ytt doctor terminal --json`; see the full [terminal compatibility matrix](docs/terminal-compatibility.md).

## Quick start

```sh
ytt
```

1. Press **`s`**, type a song, hit **`Enter`**.
2. Move with **`↑`/`↓`**, press **`Enter`** to play.
3. Press **`?`** anytime for the full, always-current key list.

That's it. Music. (Something off? **`ytt doctor`** tells you exactly what to fix.)

## Tour

Screenshots and GIFs are landing here shortly — meanwhile the **[feature tour](https://ochichan.github.io/ytm-tui/)** has everything live, in detail.

<!-- 📸 FOR THE PERSON ADDING MEDIA: drop files into docs/media/ with these exact names:
hero.gif · player.png · djgem.gif · assistant.gif · video.gif · sources.png · radio.png ·
everywhere.png · themes.gif · animations.gif · retro.png · transfer.gif
The same files serve README.md, README.ko.md and README.ja.md. Every slot below has the
same one-line instruction; extra shots are welcome — just copy a slot block. -->

### The player — real album art & time-synced lyrics

Actual cover images drawn right in the terminal (Kitty/Sixel/iTerm2, auto-detected); **`Shift+L`** scrolls the lyrics underneath.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/player.png, delete the "coming soon" line above, then uncomment:
![The player with album art and synced lyrics](docs/media/player.png)
-->

### DJ Gem streaming

**`Ctrl+R`** builds an endless station around what you're hearing — **`w`** explains, in plain language, why it picked each song.

> 🖼️ *GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/djgem.gif, delete the "coming soon" line above, then uncomment:
![DJ Gem streaming with the "why this song" panel](docs/media/djgem.gif)
-->

### DJ Gem assistant *(optional)*

**`g`**, then ask in plain words: *"play some lo-fi", "make me a rainy-day playlist"*. Needs a free Gemini key; everything else works without it.

> 🖼️ *GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/assistant.gif, delete the "coming soon" line above, then uncomment:
![Asking the DJ Gem assistant for music in plain words](docs/media/assistant.gif)
-->

### The music video, floating over your terminal

**`v`** opens it in a small mpv window; *Auto-continue videos* hands each video off to the next track's.

> 🖼️ *GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/video.gif, delete the "coming soon" line above, then uncomment:
![The music video floating over the terminal](docs/media/video.gif)
-->

### Six catalogs, one search box — plus a radio mode

**`Tab`** in Search flips between YouTube Music, SoundCloud, Audius, Jamendo, Internet Archive and Radio Browser (or all at once). **`Alt+Shift+R`** turns the whole app into an internet-radio tuner.

> 🖼️ *Screenshots coming soon.*
<!-- 📸 TO FILL: add docs/media/sources.png, delete the "coming soon" line above, then uncomment both:
![Searching across six catalogs from one box](docs/media/sources.png)
-->
<!-- 📸 TO FILL: add docs/media/radio.png, then uncomment:
![Radio mode as an internet-radio tuner](docs/media/radio.png)
-->

### Control from anywhere

Media keys, macOS Control Center, Windows SMTC + tray mini player, Linux MPRIS, `ytt -r` from any shell — or a fully headless daemon.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/everywhere.png, delete the "coming soon" line above, then uncomment:
![OS integrations: tray mini player, Control Center, SMTC, MPRIS](docs/media/everywhere.png)
-->

### Make it yours

13 themes with all 34 color roles hex-editable, and 25 animations — up to and including a spinning ASCII donut.

> 🖼️ *GIFs coming soon.*
<!-- 📸 TO FILL: add docs/media/themes.gif, delete the "coming soon" line above, then uncomment both:
![Cycling through the built-in themes](docs/media/themes.gif)
-->
<!-- 📸 TO FILL: add docs/media/animations.gif, then uncomment:
![Animations, including the spinning ASCII donut](docs/media/animations.gif)
-->

### Retro mode

One toggle makes everything CP437-safe for a bare Linux console or a crusty SSH session — album art included, as honest ASCII art.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/retro.png, delete the "coming soon" line above, then uncomment:
![Retro mode with ASCII album art](docs/media/retro.png)
-->

### Spotify moves in with one command

`ytt transfer import <url>` — checkpointed, resumable, with a match report for anything ambiguous. Setup in the reference below.

> 🖼️ *GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/transfer.gif, delete the "coming soon" line above, then uncomment:
![A Spotify playlist importing in one command](docs/media/transfer.gif)
-->

### And the rest

- **Downloads** — `d` saves a tagged m4a with cover art for offline play.
- **Scrobbling** — Last.fm / ListenBrainz with a crash-safe offline queue.
- **10-band EQ** with presets, plus loudness normalization.
- Every key rebindable, the whole UI mouse-aware, interface in English & 한국어.

**The long version of everything → [feature tour](https://ochichan.github.io/ytm-tui/).**

## Essential keys

Press **`?`** in-app for the complete live cheat sheet — it reflects *your* bindings, and every key is rebindable (Settings → Hotkeys). The core:

| Key | Does |
| --- | --- |
| `Space` | Play / pause |
| `,` / `.` | Previous / next |
| `←` / `→` · `↑` / `↓` | Seek · volume |
| `s` | Search (`Tab` picks the catalog) |
| `l` / `c` | Library / queue |
| hold `↑`/`↓` · `Shift`+`↑`/`↓` | Fast-scroll a list (accelerates) · extend the selection |
| `f` / `d` | Favorite / download (select rows with `Shift`+`↑`/`↓` or drag, then `d`, to grab many) |
| `Shift+D` | Download the whole list / playlist |
| `Shift+L` | Synced lyrics |
| `v` | Music-video overlay |
| `Ctrl+R` | DJ Gem streaming |
| `g` | DJ Gem assistant |
| `o` | Settings |
| `Ctrl+Q` | Quit |

> **Korean keyboard?** Shortcuts understand 두벌식 jamo (`ㅂ` works like `q`) — no need to switch input. Prefer the mouse? Everything is clickable, and the wheel rides the volume.

## Reference

<details>
<summary><b>Remote control & daemon</b></summary>

Once `ytt` is playing, control it from any shell:

```sh
ytt -r pp                  # play / pause      (aliases: toggle, play, pause)
ytt -r next / prev         # skip around
ytt -r volume 40           # set volume; also: up / down
ytt -r seek-to 90          # jump to 1:30
ytt -r streaming on        # endless streaming: on / off / toggle
ytt -r play "lofi"         # daemon: search and play the first result
ytt -r status              # one-line "now playing" (--json for scripts)
```

Media keys on i3 / sway: `bindsym XF86AudioPlay exec ytt -r pp`.

For terminal-free playback, run the headless daemon:

```sh
ytt daemon start --resume   # restore the saved queue/session and play
ytt daemon stop             # stop the daemon and reap mpv
```

The daemon keeps streaming, scrobbling and OS media controls working. Launching `ytt` twice won't start a second player (`ytt --new-instance` if you really want two). Full lists: `ytt -r --help`, `ytt daemon --help`.

</details>

<details>
<summary><b>Scrobbling setup (Last.fm / ListenBrainz)</b></summary>

`ytt` scrobbles what you actually listen to — the standard half-track/4-minute rule, like→love sync, and an offline queue that hits disk *before* any network attempt, so crashes lose nothing. Works in the TUI and the daemon alike.

- **Last.fm** — Settings → **Accounts** → approve in the browser, or `ytt auth lastfm`. Self-built binaries can set `scrobble.lastfm.api_key` / `api_secret` in `config.json` ([create an API account](https://www.last.fm/api/account/create)).
- **ListenBrainz** — paste your [user token](https://listenbrainz.org/settings/) into Settings → Accounts, or `ytt auth listenbrainz <token>`. Self-hosted: set `scrobble.listenbrainz.api_url`.
- Undelivered listens wait in `scrobble-queue.jsonl` next to your config and flush automatically.

</details>

<details>
<summary><b>Spotify import / export</b></summary>

```sh
ytt auth spotify --client-id <YOUR-CLIENT-ID>   # one-time PKCE browser connect
ytt transfer import <spotify-url-or-id>          # → a new YTM playlist
ytt transfer import liked --to likes             # Spotify likes → YTM likes (order kept)
ytt transfer export ytm:<id> --to spotify        # the other direction
ytt transfer backup --dir ~/music-backup --csv   # every YTM playlist → JSON (+CSV)
ytt transfer resume <job-id>                     # continue after a rate-limit/abort
```

Or stay in the TUI: Settings → **Accounts** → *Import from Spotify…* while the music keeps playing.

**One-time setup (~5 min).** Spotify apps in Development Mode only serve accounts you explicitly allowlist, so everyone brings their own free app. There is no client *secret* — PKCE doesn't use one.

1. Sign in at [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard) and click **Create app**.
2. Give it any **App name** and **App description** (e.g. `ytm-tui`).
3. Under **Redirect URIs**, add exactly `http://127.0.0.1:9271/callback` and click **Add**. It must be the loopback IP literal `127.0.0.1`, **never `localhost`** (Spotify rejects `localhost`). Using a different port? Set `spotify.redirect_port` in `config.json` and match it here.
4. Under **Which API/SDKs are you planning to use?**, tick **Web API**.
5. Accept the terms and **Save**.
6. Open the app → **Settings** and copy the **Client ID** (you do *not* need the Client secret).
7. Open **User Management** (in the app's settings) and add your own account — your name plus the email on your Spotify account. Dev-Mode apps serve up to 25 such allowlisted users.
8. In ytt: **Settings → Accounts → Spotify**, paste the Client ID, and choose **Connect** (or run `ytt auth spotify --client-id <ID>`). Your browser opens Spotify's approval page — approve it and you're done. On headless/SSH where no browser opens, the URL is copied to your clipboard and saved to `spotify_auth_url.txt`, so you can open it on any device.

Matching is metadata-based (NFKC-normalized, CJK-safe). Anything ambiguous lands in the job report instead of being silently guessed — re-run with `--take-best` / `--min-score`, or preview big playlists with `--dry-run` and then `ytt transfer resume <job-id>`.

</details>

<details>
<summary><b>Sign-in cookies & file locations</b></summary>

**Cookies (optional).** Public songs play anonymously — only members-only/region-locked tracks and account playlists need this. Export your YouTube Music cookies in **Netscape format** to `~/Music/yututui/cookies.txt` (Windows: `%USERPROFILE%\Music\yututui\cookies.txt`) and restart. **Treat that file like a password**, and export the *incognito way*: sign in inside a private window, export `cookies.txt` from that tab, then close the window — a session whose browser is gone never gets rotated or signed out. A good export has `SAPISID`/`SID` lines in it.

**Config & data.**

- Config: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows) — with `playlists.json`, `scrobble-queue.jsonl` and `transfers/` alongside.
- Downloads: `~/Music/yututui` — change via the **Download dir** setting or `YTM_DOWNLOAD_DIR`.
- `GEMINI_API_KEY` and `YTM_DOWNLOAD_DIR` override saved settings at launch.

</details>

<details>
<summary><b>yt-dlp selection</b></summary>

`ytt` may run a different yt-dlp than the one your shell prints with `yt-dlp --version`: the app can use its managed copy, a configured override, or the system binary on `PATH`. To see the actual choice and candidates:

```sh
ytt tools status --why
```

Recovery commands:

```sh
ytt tools update              # refresh the managed copy now
ytt tools use system          # ignore managed yt-dlp and use PATH
ytt tools use managed         # pin the installed managed copy
ytt tools use /path/to/yt-dlp # pin a specific executable
ytt tools unpin               # return to normal managed/system selection
```

`YTM_YTDLP` is still the strongest override. If you change it in your OS settings, open a fresh terminal or unset it before expecting `ytt tools use ...` to take over.

The app's own yt-dlp calls ignore your yt-dlp config file by default, so options meant for shell downloads do not break parsed output. Set `YTM_YTDLP_USER_CONFIG=1` to re-enable your yt-dlp config for app-parsed calls. Playback through mpv's `ytdl_hook` still honors your yt-dlp config; only search, playlist fetches, metadata, prefetch resolution, and downloads ignore it by default.

</details>

<details>
<summary><b>Troubleshooting</b></summary>

| Symptom | Fix |
| --- | --- |
| Nothing plays, or it errors on play | mpv or yt-dlp missing — run `ytt doctor`. |
| `ytt: command not found` | Open a fresh terminal; still stuck, add the `PATH` line the installer printed. |
| Worked yesterday, not today | YouTube changed something — `ytt tools update`, then `ytt tools status --why`; if a managed update is bad, `ytt tools use system`. |
| A specific song won't play | It may need sign-in — see the cookies section above. |
| No album art | Off by default: Settings → General → **Album art**, then restart. |
| Album art or zoom behaves differently by terminal | Run `ytt doctor terminal --json` and compare with the [terminal matrix](docs/terminal-compatibility.md). |
| No Control Center / SMTC / MPRIS entry | Settings → Playback → **OS media controls**; it publishes once something has played. |
| Flyout shows "Unknown app" / two entries | Run `ytt register-media-identity` once (two entries = mpv's own media session; auto-disabled on mpv ≥ 0.39). |
| DJ Gem won't respond | Add a free Gemini key in Settings → DJ Gem and switch **Enable DJ Gem** on. |
| Spotify 403 / "not allowlisted" | Add your own account under *User Management* in your Spotify app dashboard, and check the Client ID for typos. |
| Browser shows INVALID_CLIENT / redirect mismatch | The redirect URI must match **exactly**: `http://127.0.0.1:9271/callback` — IP not `localhost`, correct port, no trailing slash. |
| "could not listen on 127.0.0.1:9271" | That port is busy. Set `spotify.redirect_port` in `config.json` and update the dashboard redirect URI to match. |
| Clicked Connect but no browser opened | On headless/SSH the auth URL is copied to your clipboard and saved to `spotify_auth_url.txt` — paste it into any browser to approve. |
| Spotify import "needs a YouTube Music cookie" | Importing into a YTM playlist/likes needs sign-in; importing into a local Library playlist works without one. See the cookies section. |
| Scrobbles not appearing | Check Settings → Accounts; the daemon reads accounts at start — restart it after connecting. |
| Remapped a key into chaos | Settings → General → **Reset keybindings**. |

Still stuck? [Open an issue](https://github.com/Ochichan/ytm-tui/issues) and mention your OS.

</details>

## Thanks & license

🙏 Huge thanks to **[@ZZNN75](https://github.com/ZZNN75)** for the real QA hours — the rough edges you *won't* hit are smooth because they hit them first. 🫡

MIT. Fork it, ship it, do whatever you want.
