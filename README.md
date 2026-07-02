# ytm-tui

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

### [▶ Live demo & feature tour → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

YouTube Music in your terminal. Fast, keyboard-driven, no browser tab eating your RAM, no ads. DJ Gem streaming, real album art, and remote control — all from a three-letter command: `ytt`.

Rust + ratatui. MIT.

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

> The `curl | bash` and from-source paths install **only** `ytt`. Install the helpers yourself (`brew install mpv yt-dlp ffmpeg`, `sudo apt install mpv yt-dlp ffmpeg`, `sudo pacman -S mpv yt-dlp ffmpeg`) — or just run `ytt doctor` afterward to see what's missing.
> On Windows, Scoop also installs `ytt-tray.exe` and a **YtmTui Tray** shortcut. It lives in the notification area; a terminal-hosted `ytt` session still belongs to Windows Terminal's taskbar button.
> Windows tray startup is opt-in: run `ytt-tray --install-startup` to enable it and `ytt-tray --uninstall-startup` to remove it.
> Background playback: `ytt daemon start --resume` starts the headless music daemon from your saved queue; control it with `ytt -r status`, `ytt -r pp`, `ytt -r next`, `ytt -r play "lofi"`, and `ytt daemon stop`.

---

## Quick start

```sh
ytt
```

1. Press **`s`**, type a song, hit **`Enter`**.
2. Move with **`↑`/`↓`**, press **`Enter`** to play.
3. Press **`?`** anytime for the full, always-current key list.

That's it. Music.

> **Something off?** Run **`ytt doctor`** — it checks mpv, yt-dlp and ffmpeg and tells you exactly what to fix. Seeing `ytt: command not found`? Open a fresh terminal window so your `PATH` catches up.

---

## What it does

- **DJ Gem streaming** — press **`Ctrl+R`** and it builds an endless station around what you're hearing. Three moods: Focused, Balanced, Discovery. Press **`w`** to see, in plain language, why it picked each song.
- **Real album art** — actual cover images drawn right in the Player, on terminals that support them. Time-synced lyrics scroll underneath (**`Shift+L`**).
- **Remote control + daemon mode** — drive a running TUI or the headless daemon from another terminal: `ytt -r pp`, `ytt -r next`, `ytt -r status`, `ytt -r play "city pop"`.
- **Search · Library · Queue** — **`s`** to search, **`l`** for your library (favorites, history, downloads), **`c`** for the queue.
- **Yours to tweak** — 11 themes, every color editable in hex, every key rebindable, a 10-band EQ, and animations from a calm still screen to a spinning ASCII donut.
- **DJ Gem assistant** *(optional)* — press **`g`** and ask in plain words: *"play some lo-fi", "queue three upbeat songs"*. Needs a free Google Gemini key; everything else works without it.
- **Downloads** — press **`d`** to save a song and play it offline.

The app interface is in **English and 한국어** (Settings → General → Language). This README is also available in [한국어](README.ko.md) and [日本語](README.ja.md).

---

## Essential keys

Press **`?`** in the app for the complete, live cheat sheet — it reflects *your* custom bindings, and every key below is rebindable (Settings → Hotkeys). The basics:

| Key | Does |
| --- | --- |
| `Space` | Play / pause |
| `←` / `→` | Seek back / forward |
| `↑` / `↓` | Volume up / down |
| `n` / `p` | Next / previous |
| `s` | Search |
| `Shift+S` | Shuffle |
| `l` | Library |
| `a` | Play whole library tab |
| `c` | Queue |
| `f` | Favorite / rate |
| `d` | Download |
| `Ctrl+R` | Toggle DJ Gem streaming |
| `Shift+L` | Lyrics |
| `g` | DJ Gem assistant |
| `w` | Why DJ Gem picked these |
| `,` | Settings |
| `?` | Full keybinding list |
| `Ctrl+Q` | Quit |

> **Korean keyboard?** Shortcuts understand 두벌식 jamo, so `ㅂ` works like `q` and `ㄱ` like `r` — no need to switch input.

---

## Remote control

Once `ytt` is playing, control it from another terminal — or your media keys — with `ytt -r`:

```sh
ytt -r pp          # play / pause
ytt -r next        # next song
ytt -r streaming on    # turn streaming on
ytt -r play "lofi"      # daemon: search and play the first result
ytt -r enqueue "city pop"  # daemon: search and add the first result
ytt -r status      # one-line "now playing"
ytt -r quit        # stop and close
```

Wire it to your media keys (i3 / sway):

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
```

For terminal-free playback, start the daemon:

```sh
ytt daemon start --resume   # restore the saved queue/session and play it
ytt daemon status --json    # owner/status snapshot for scripts
ytt daemon stop             # stop the daemon and reap mpv
```

Daemon resume restores the saved queue order, cursor, shuffle/repeat, and normal/radio mode queues. Autoplay streaming keeps working headlessly and tops up the queue through the same recommendation path as the TUI. `ytt -r play …` and `ytt -r enqueue …` are daemon search commands; a standalone TUI owner rejects them with `daemon_required`.

Launching `ytt` twice won't start a second player fighting over your speakers — it just reminds you how to reach the one you've got. (`ytt --new-instance` if you really want two.) Run `ytt -r --help` and `ytt daemon --help` for the full command list.

---

## Scrobbling (Last.fm / ListenBrainz)

`ytt` scrobbles what you actually listen to — Now Playing updates, the standard
half-track/4-minute rule, love/unlove sync for in-app likes, and a durable offline queue
(listens recorded on a plane are delivered when you're back online). Works in the TUI and
the headless daemon alike, independent of the OS media-controls toggle.

- **Last.fm** — Settings → **Accounts** → *Last.fm account* → approve in the browser. Or
  headless: `ytt auth lastfm`. Self-built binaries without embedded API credentials can
  set their own via `scrobble.lastfm.api_key` / `api_secret` in `config.json`
  ([create an API account](https://www.last.fm/api/account/create)).
- **ListenBrainz** — paste your [user token](https://listenbrainz.org/settings/) into
  Settings → Accounts, or run `ytt auth listenbrainz <token>`. Self-hosted instances:
  set `scrobble.listenbrainz.api_url`.
- Toggles for each service, like→love sync, and local-file scrobbling live on the same
  tab. Queued-but-undelivered listens sit in `scrobble-queue.jsonl` next to your config.

## Spotify import / export

Move playlists between Spotify, YouTube Music, and plain files — with checkpointed,
resumable jobs and a match report for anything ambiguous:

```sh
ytt auth spotify --client-id <YOUR-CLIENT-ID>   # one-time PKCE browser connect
ytt transfer list spotify                        # find the playlist id
ytt transfer import <spotify-url-or-id>          # → a new YTM playlist
ytt transfer import liked --to likes             # Spotify likes → YTM likes (order kept)
ytt transfer import backup.csv --to-playlist "Restored"   # Exportify CSV / ytm-tui JSON
ytt transfer export ytm:<id> --to spotify        # the other direction
ytt transfer backup --dir ~/music-backup --csv   # every YTM playlist → JSON (+CSV)
ytt transfer resume <job-id>                     # continue after a rate-limit/abort
```

Or stay in the TUI: Settings → **Accounts** → *Import from Spotify…* picks a playlist and
imports it while you keep listening; progress rides the status line.

**Spotify setup (one time).** Spotify Development-Mode apps only serve accounts on their
own allowlist, so you bring your own (free) app: create one at
[developer.spotify.com/dashboard](https://developer.spotify.com/dashboard), add the
redirect URI `http://127.0.0.1:9271/callback` **exactly** (the loopback IP, not
`localhost`; change the port via `spotify.redirect_port` if 9271 is taken), add your own
Spotify account under *User Management*, and paste the app's Client ID into
Settings → Accounts (or `ytt auth spotify --client-id …`). There is no client secret —
the PKCE flow doesn't use one.

Matching is metadata-based (NFKC-normalized, CJK-safe titles + artist + duration + album
tie-breaks). Anything under the accept threshold lands in the job report as *ambiguous*
or *not found* instead of being silently guessed; re-run with `--take-best`,
`--min-score`, or fix by hand. Big playlists: run with `--dry-run`, review, then
`ytt transfer resume <job-id>` to write.

---

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| Nothing plays, or it errors on play | mpv or yt-dlp is missing or not on your `PATH`. Run `ytt doctor`. |
| `ytt: command not found` | Open a fresh terminal. Still stuck? The installer printed the `PATH` line to add. |
| Worked yesterday, not today | YouTube changed something — update yt-dlp (`brew upgrade yt-dlp`, `scoop update yt-dlp`, or your package manager). |
| A specific song won't play | It may need sign-in — see Cookies below. |
| No album art | Off by default, and terminal-dependent. Turn on **Album art** (Settings → General) and restart. |
| DJ Gem won't respond | Add a free Gemini key in Settings → DJ Gem and switch **Enable DJ Gem** on. |
| Spotify returns 403 on connect/import | Your app is in Development Mode: add your own Spotify account under *User Management* in the developer dashboard, and re-check the Client ID. |
| Scrobbles not appearing | Check Settings → Accounts is connected and enabled; offline listens flush automatically (they wait in `scrobble-queue.jsonl`). The daemon reads accounts at start — restart it after connecting. |
| Remapped a key into chaos | Settings → General → **Reset keybindings**. |

Still stuck? [Open an issue](https://github.com/Ochichan/ytm-tui/issues) and mention your OS.

---

## Sign-in & file locations

**Cookies (optional).** You almost certainly don't need this — public songs search and play fine anonymously. To reach members-only or region-locked tracks (and for playlist transfer / account playlists), export your YouTube Music cookies in **Netscape format** to `cookies.txt` (macOS: `~/Music/ytm-tui/cookies.txt`, Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`) and restart `ytt`. **Treat that file like a password.** You can also point to it in Settings → General.

Export them the *incognito way* or they die within minutes: open a **private/incognito window**, sign in to music.youtube.com there, export `cookies.txt` from that tab (allow your exporter extension in incognito first), then **close the incognito window**. A session whose browser is gone never gets rotated or signed out — exports from your everyday browser stop working as soon as it rotates the session, and heavy tool use can even sign that browser out. A good export has `SAPISID`/`SID` lines in it; visitor-only exports (no login) won't work and `ytt` will say so.

**Config & downloads.**
- Config: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows).
- Downloads default to `~/Music/ytm-tui`; change it with the **Download dir** setting or `YTM_DOWNLOAD_DIR`.
- `GEMINI_API_KEY` and `YTM_DOWNLOAD_DIR` environment variables override the saved settings at launch.

---

## Special thanks

🙏 Huge thanks to **[@ZZNN75](https://github.com/ZZNN75)** for the real QA hours — poking at every corner and breaking things on purpose so you won't have to. A lot of the rough edges you *won't* hit are smooth because they hit them first. 🫡

## License

MIT. Fork it, ship it, do whatever you want.
