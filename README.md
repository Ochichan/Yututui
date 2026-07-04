# ytm-tui

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/Ochichan/ytm-tui)](https://github.com/Ochichan/ytm-tui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

### [▶ Live demo & feature tour → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

YouTube Music in your terminal. Fast, keyboard-driven, no browser tab eating your RAM, no ads.
DJ Gem streaming, real album art with synced lyrics, Last.fm / ListenBrainz scrobbling, one-command
Spotify migration, and remote control from anywhere — all behind a three-letter command: `ytt`.

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
> On Windows, Scoop also installs `ytt-tray.exe` and a **YtmTui Tray** shortcut — the notification-area mini player ([details below](#media-keys--os-integration)). A terminal-hosted `ytt` session still belongs to Windows Terminal's taskbar button.
> On macOS, Homebrew and the release archive ship `ytt-tray` too — the same companion, living in your menu bar (releases after v1.5.8).
> Tray startup is opt-in on both: `ytt-tray --install-startup` to enable, `ytt-tray --uninstall-startup` to remove.
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
- **Six catalogs, one search box** — YouTube Music is home base, but **`Tab`** in Search flips to SoundCloud, Audius, Jamendo, Internet Archive, or Radio Browser (or all at once, every result tagged `[SRC]`). There's even a dedicated Radio mode (**`Alt+Shift+R`**) that turns the whole app into an internet-radio tuner with its own favorites and history.
- **Real album art + synced lyrics + the video** — actual cover images drawn right in the Player (Kitty/Sixel/iTerm2 graphics, auto-detected). Time-synced lyrics scroll underneath (**`Shift+L`**). And when hearing isn't enough, **`v`** floats the music video over your terminal in a small mpv window — flip on *Auto-continue videos* (Settings → Playback) and each video hands off to the next track's, right in the same window.
- **Search · Library · Queue · Playlists** — **`s`** to search, **`l`** for your library (favorites, history, downloads, playlists), **`c`** for the queue. Build playlists in-app (**`n`**), or let DJ Gem build them for you.
- **Downloads** — press **`d`** to save a song as an m4a with embedded cover art and tags, playable offline from the Downloads tab.
- **Scrobbling** — Last.fm and ListenBrainz, with a crash-safe offline queue and like→love sync. [Details below](#scrobbling-lastfm--listenbrainz).
- **Spotify import / export** — your liked songs and playlists migrate with one checkpointed, resumable command. [Details below](#spotify-import--export).
- **Control from everywhere** — media keys, macOS Control Center, Windows SMTC + tray mini player, Linux MPRIS, `ytt -r` from any shell, or a fully headless daemon. [Details below](#media-keys--os-integration).
- **Yours to tweak** — 13 themes, all 34 color roles editable in hex, every key rebindable, a 10-band EQ with presets, animations from a calm still screen to a spinning ASCII donut — and a retro mode that runs in a bare Linux console, ASCII album art included.
- **DJ Gem assistant** *(optional)* — press **`g`** and ask in plain words: *"play some lo-fi", "queue three upbeat songs", "make me a rainy-day playlist"*. Needs a free Google Gemini key; everything else works without it.

The app interface is in **English and 한국어** (Settings → General → Language). This README is also available in [한국어](README.ko.md) and [日本語](README.ja.md).

---

## Essential keys

Press **`?`** in the app for the complete, live cheat sheet — it reflects *your* custom bindings, and every key below is rebindable (Settings → Hotkeys). The basics:

| Key | Does |
| --- | --- |
| `Space` | Play / pause |
| `←` / `→` | Seek back / forward |
| `↑` / `↓` | Volume up / down |
| `m` | Mute / unmute |
| `,` / `.` | Previous / next |
| `s` | Search (`Tab` picks the source) |
| `l` | Library · `a` play whole tab · `\` enqueue · `/` filter |
| `c` | Queue |
| `f` | Favorite / rate |
| `d` | Download |
| `P` | Add to playlist (from lists: `p`) |
| `Shift+L` | Synced lyrics |
| `v` | Music video overlay (`V` moves it) |
| `y` | Copy the track's link |
| `Ctrl+R` | DJ Gem streaming on/off |
| `w` | Why DJ Gem picked these |
| `g` | DJ Gem assistant |
| `Shift+S` / `r` | Shuffle / repeat cycle |
| `e` | EQ preset (Flat · Bass · Treble · Vocal · Rock · Jazz) |
| `[` / `]` | Playback speed (0.5×–2×) |
| `Ctrl+-` / `Ctrl+=` | Text zoom out / in (also Ctrl+wheel, or Settings → Large text; kitty, Windows Terminal, …) |
| `o` | Settings |
| `Ctrl+Q` | Quit |

> **Korean keyboard?** Shortcuts understand 두벌식 jamo, so `ㅂ` works like `q` and `ㄱ` like `r` — no need to switch input. Prefer the mouse? Everything on screen is clickable, and the wheel rides the volume.

---

## Playlists

The Library's **Playlists** tab holds your own local playlists — created in-app (**`n`**), filled from anywhere (**`P`** on the current track, **`p`** on any list row), reordered and pruned at will, played or enqueued whole (**`a`** / **`\`**). They live in a plain `playlists.json` next to your config, so they're yours to back up.

Spotify imports can land here too (`--to local`), the DJ Gem assistant can create and fill them on request, and `ytt transfer export local:<name> --to spotify` sends one back the other way.

---

## Remote control & daemon

Once `ytt` is playing, control it from another terminal — or your media keys — with `ytt -r`:

```sh
ytt -r pp                  # play / pause      (aliases: toggle, play, pause)
ytt -r next / prev         # skip around
ytt -r volume 40           # set volume; also: up / down
ytt -r back / fwd          # seek by your configured step
ytt -r seek-to 90          # jump to 1:30
ytt -r streaming on        # endless streaming: on / off / toggle
ytt -r play "lofi"         # daemon: search and play the first result
ytt -r enqueue "city pop"  # daemon: search and add the first result
ytt -r status              # one-line "now playing" (--json for scripts, -q for silence)
ytt -r quit                # stop and close
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

Daemon resume restores the saved queue order, cursor, shuffle/repeat, and normal/radio mode queues. Autoplay streaming keeps topping up the queue headlessly through the same recommendation path as the TUI, scrobbling keeps scrobbling, and the OS media controls keep working. `ytt -r play …` and `ytt -r enqueue …` are daemon search commands; a standalone TUI rejects them.

Launching `ytt` twice won't start a second player fighting over your speakers — it just reminds you how to reach the one you've got (`ytt --new-instance` if you really want two). Run `ytt -r --help` and `ytt daemon --help` for the full command list.

---

## Media keys & OS integration

`ytt` shows up wherever your OS shows music — on by default, toggle under Settings → Playback → *OS media controls*. Works from the TUI and the daemon alike.

- **macOS** — the real Now Playing card in Control Center: artwork, a working scrubber, next/previous, and a Like button. Play/pause from your AirPods' stem does what it should. And the **YtmTui Tray** companion rides the menu bar (`ytt-tray`, included with brew and the release archive) — the same mini player and menu as Windows, one click from the clock.
- **Windows** — the SMTC media overlay with artwork and seek, plus the optional **YtmTui Tray** companion (installed by Scoop): left-click for a mini player with Now / Queue / Stream / Tune tabs, right-click for the full menu — start/stop the daemon, resume the last session, open the TUI. `ytt-tray --install-startup` makes it start at login; it's opt-in. Installers also register the app identity so the flyout says **YtmTui** — if it ever shows "Unknown app", run `ytt register-media-identity` once.
- **Linux** — a first-class MPRIS player (`org.mpris.MediaPlayer2.ytmtui`): playerctl, GNOME/KDE media widgets, and waybar all just work.

---

## Scrobbling (Last.fm / ListenBrainz)

`ytt` scrobbles what you actually listen to — Now Playing updates, the standard
half-track/4-minute rule, love/unlove sync for in-app likes, and a durable offline queue
(listens recorded on a plane are delivered when you're back online — they hit disk *before*
any network attempt, so crashes lose nothing). Works in the TUI and
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
ytt transfer list spotify                        # find the playlist id (list ytm works too)
ytt transfer import <spotify-url-or-id>          # → a new YTM playlist
ytt transfer import liked --to likes             # Spotify likes → YTM likes (order kept)
ytt transfer import <id> --to local:"Gym"        # → an in-app Library playlist instead
ytt transfer import backup.csv --to-playlist "Restored"   # Exportify CSV / ytm-tui JSON
ytt transfer export ytm:<id> --to spotify        # the other direction
ytt transfer backup --dir ~/music-backup --csv   # every YTM playlist → JSON (+CSV)
ytt transfer resume <job-id>                     # continue after a rate-limit/abort
```

Or stay in the TUI: Settings → **Accounts** → *Import from Spotify…* picks a playlist and
imports it into your Library's Playlists tab while you keep listening; progress rides the
status line.

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

## Make it yours

- **Themes** — 13 built-ins (Default, Midnight, Light, High Contrast, Terminal Green, Gruvbox, Nord, Dracula, Tokyo Night, Solarized Dark, Rosé Pine, Dario, Retro), and every one of the 34 color roles takes a `#RRGGBB` (or `none` for transparent) in Settings → Graphics.
- **EQ** — a real 10-band graphic EQ (31 Hz–16 kHz) under Settings → Playback; **`e`** cycles the presets, **`N`** toggles loudness normalization.
- **Animations** — the `✨` in the nav (or **`A`**) toggles them; Settings → Graphics picks which of the 25: title shimmer, beating heart, seekbar glow, EQ bars, a track-intro reveal, lyrics glow, typewriter toasts, volume/seek/like feedback flashes, breathing selections, list cascades, blinking carets, tab pops, popup fade-ins, activity dots, About-card sparkles, matrix rain, starfield, a DVD-style bouncing logo, and yes, the spinning ASCII donut.
- **Retro mode** — one toggle (Settings → Graphics) makes everything CP437-safe for a bare Linux console or a crusty SSH session: Retro theme, ASCII-only glyphs, and album art re-rendered as honest-to-goodness ASCII art.
- **Keys & mouse** — every binding is remappable with conflict detection (Settings → Hotkeys), and the whole UI is mouse-aware if you'd rather click.

---

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| Nothing plays, or it errors on play | mpv or yt-dlp is missing or not on your `PATH`. Run `ytt doctor`. |
| `ytt: command not found` | Open a fresh terminal. Still stuck? The installer printed the `PATH` line to add. |
| Worked yesterday, not today | YouTube changed something — update yt-dlp (`brew upgrade yt-dlp`, `scoop update yt-dlp`, or your package manager). |
| A specific song won't play | It may need sign-in — see Cookies below. |
| No album art | Off by default, and terminal-dependent. Turn on **Album art** (Settings → General) and restart. |
| `v` won't open the video | It launches a separate mpv window — check `ytt doctor`. Local-only tracks have no video to show. |
| No Control Center / SMTC / MPRIS entry | Check Settings → Playback → **OS media controls**; it publishes once something has played. |
| Media flyout shows "Unknown app" / two entries | Run `ytt register-media-identity` once (installers do it for you). Two entries means mpv's own media session is on — `ytt` turns it off automatically on mpv ≥ 0.39; re-enable it deliberately with `YTM_MPV_EXTRA=--media-controls=yes`. |
| DJ Gem won't respond | Add a free Gemini key in Settings → DJ Gem and switch **Enable DJ Gem** on. |
| Spotify returns 403 on connect/import | Your app is in Development Mode: add your own Spotify account under *User Management* in the developer dashboard, and re-check the Client ID. |
| Scrobbles not appearing | Check Settings → Accounts is connected and enabled; offline listens flush automatically (they wait in `scrobble-queue.jsonl`). The daemon reads accounts at start — restart it after connecting. |
| Remapped a key into chaos | Settings → General → **Reset keybindings**. |

Still stuck? [Open an issue](https://github.com/Ochichan/ytm-tui/issues) and mention your OS.

---

## Sign-in & file locations

**Cookies (optional).** You almost certainly don't need this — public songs search and play fine anonymously. To reach members-only or region-locked tracks (and for playlist transfer / account playlists), export your YouTube Music cookies in **Netscape format** to `cookies.txt` (macOS: `~/Music/ytm-tui/cookies.txt`, Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`) and restart `ytt`. **Treat that file like a password.** You can also point to it in Settings → General.

Export them the *incognito way* or they die within minutes: open a **private/incognito window**, sign in to music.youtube.com there, export `cookies.txt` from that tab (allow your exporter extension in incognito first), then **close the incognito window**. A session whose browser is gone never gets rotated or signed out — exports from your everyday browser stop working as soon as it rotates the session, and heavy tool use can even sign that browser out. A good export has `SAPISID`/`SID` lines in it; visitor-only exports (no login) won't work and `ytt` will say so.

**Config & data.**
- Config: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows).
- Alongside it: `playlists.json` (your playlists), `scrobble-queue.jsonl` (undelivered listens), and `transfers/` (resumable job checkpoints + reports).
- Downloads default to `~/Music/ytm-tui`; change it with the **Download dir** setting or `YTM_DOWNLOAD_DIR`.
- `GEMINI_API_KEY` and `YTM_DOWNLOAD_DIR` environment variables override the saved settings at launch.

---

## Special thanks

🙏 Huge thanks to **[@ZZNN75](https://github.com/ZZNN75)** for the real QA hours — poking at every corner and breaking things on purpose so you won't have to. A lot of the rough edges you *won't* hit are smooth because they hit them first. 🫡

## License

MIT. Fork it, ship it, do whatever you want.
