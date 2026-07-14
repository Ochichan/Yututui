# YuTuTui!

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/Ochichan/Yututui)](https://github.com/Ochichan/Yututui/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/Ochichan/Yututui/ci-pr.yml?branch=main&label=CI)](https://github.com/Ochichan/Yututui/actions/workflows/ci-pr.yml)
[![Downloads](https://img.shields.io/github/downloads/Ochichan/Yututui/total?color=f6c177)](https://github.com/Ochichan/Yututui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

YouTube Music in your terminal — fast, keyboard-driven, no browser tab eating your RAM, no ads. All behind a three-letter command: `ytt`. Rust + ratatui. MIT.

Public beta: stable enough for daily use, still moving fast.

### [▶ Live demo & the full feature tour → ochichan.github.io/Yututui](https://ochichan.github.io/Yututui/)

**📖 New to terminals?** The [friendly manual](MANUAL.md) walks through every mode — music, radio, the Local Deck, and the full Spotify move-in — step by step, no jargon. ([한국어](MANUAL.ko.md) · [日本語](MANUAL.ja.md))

> 🖼️ *Demo GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/hero.gif, delete the "coming soon" line above, then uncomment:
![Search, play, real album art and synced lyrics in one terminal](docs/media/hero.gif)
-->

---

## Install

Each command installs `ytt` **and** its helpers (mpv, yt-dlp, ffmpeg) in one go.

| OS | One command |
| --- | --- |
| **macOS** | `brew install Ochichan/tap/yututui` |
| **Windows** | `scoop bucket add extras; scoop bucket add yututui https://github.com/Ochichan/scoop-bucket; scoop install yututui` |
| **Linux** — any, with [Nix](https://nixos.org/download) | `nix run github:Ochichan/Yututui` |
| **Linux** — Arch | `yay -S yututui-bin` |
| **Linux** — any other | Download and run the installer below |
| **From source** | `./install.sh --build` (needs [Rust](https://rustup.rs)) |

> Arch AUR publishing is temporarily pending; use Nix or the installer until `yututui-bin` is available.

```sh
curl -fsSL https://raw.githubusercontent.com/Ochichan/Yututui/main/install.sh | bash
```

Windows direct installer:

```powershell
irm https://raw.githubusercontent.com/Ochichan/Yututui/main/install.ps1 | iex
```

On Windows, launch **YuTuTui!** from the Start Menu. The tray companion opens Windows Terminal
and starts `ytt` for you; its right-click menu also has **Open Player**. Double-clicking
`ytt.exe` directly is supported too, and the console stays open after exit so an error is not
lost. macOS offers the same **Open Player** action from its menu-bar companion. Linux keeps the
lightweight native path: run `ytt` from your terminal or make a desktop launcher for that command.

Then run `ytt`. If anything's off, `ytt doctor` tells you exactly what to fix — more in [Troubleshooting](#troubleshooting).

<details>
<summary><b>Tray companion (macOS / Windows)</b></summary>

macOS and Windows releases include `yututray`, the menu-bar / notification-area mini player.

| Channel | What gets installed | How to start the tray |
| --- | --- | --- |
| macOS Homebrew | `ytt`, `yututray`, runtime tools | `yututray --background` |
| Windows Scoop | `ytt.exe`, `yututray.exe`, runtime tools, Start Menu shortcut | `yututray --background` or **YuTuTray!** |
| Direct installers / source build scripts | `ytt`; macOS/Windows also get `yututray` | `yututray --background` |
| Linux | `ytt` with MPRIS media integration | no separate tray app |

Start-at-login is opt-in: `yututray --install-startup`.

`yututray` and `yututray --background` start tray-only, and `--mini` opens the native mini player.
Packaged releases include those tray/mini surfaces but intentionally exclude the full web-GUI app;
`--main-window` is available only in developer builds that embed its assets. Launching the bare
command again asks the existing instance to show its mini player instead of creating a second tray
icon. On Windows, left-click
toggles the mini player and right-click opens the menu; macOS keeps the native menu on the status
item and exposes the mini player from **Show Mini Player**.

The unpinned mini player behaves like a popover and hides after focus moves away. Pinning keeps it
visible, always on top, and restores its monitor-relative position. Tray-only and mini-only modes
stay out of the taskbar/Dock and app switcher.

</details>

### Runtime tools

YuTuTui! uses **mpv** for playback, **yt-dlp** for search/stream resolution, and **ffmpeg** for
download post-processing. Packaged installs include them. If a direct or source install is
missing one, the app shows a friendly setup card with a copyable OS command, setup guide, and
**Check again** button instead of exposing a process error. `ytt doctor` remains the detailed
diagnostic command.

## Quick start

```sh
ytt
```

On a new profile, a ten-second hint points to **Search**. Press the displayed search key
(normally `s`) or click **Search**; completing the hint once keeps later launches clean.

1. Press **`s`**, type a song, hit **`Enter`**.
2. Move with **`↑`/`↓`**, press **`Enter`** to play.
3. Press **`?`** anytime for the full, always-current key list.

That's it. Music.

**New to terminals?** Switch on **Beginner Mode** (Settings → General) and the next launch adds an interactive, step-by-step walkthrough — plus the [friendly manual](MANUAL.md) covers every mode at your own pace.

## Tour

Every feature below is shown live, in detail, on the **[feature tour](https://ochichan.github.io/Yututui/)**.

<!-- 📸 FOR THE PERSON ADDING MEDIA: drop files into docs/media/ with these exact names:
hero.gif · player.png · lyrics.gif · search.gif · sources.png · djgem.gif · assistant.gif ·
video.gif · radio.png · radio-id.gif · library.png · queue.png · downloads.png ·
localdeck.png · everywhere.png · tray.png · themes.gif · animations.gif · showpiece.gif · eq.png ·
audio-output.png · retro.png · transfer.gif · help.png · onboarding.gif · context-menu.png
The same files serve README.md, README.ko.md and README.ja.md. Every slot below has the
same one-line instruction; extra shots are welcome — just copy a slot block. -->

### The player — real album art & time-synced lyrics

Actual cover images drawn right in the terminal (Kitty/Sixel/iTerm2, auto-detected — pick Standard, High or Original quality in Settings); **`Shift+L`** scrolls the lyrics underneath. Click any visible lyric line to seek there, or use **`z`** / **`Z`** to show the lyrics 0.1 seconds earlier / later. When lyrics load, **`[ − 0.0s + ]`** appears for three seconds; after it folds to **`[±]`**, click the handle to reopen it for three seconds and use **`−/+`** for fine adjustment. The player controls dock to the bottom of every screen (collapse them with **`B`**; the classic top layout is one setting away), the art stays centered in whatever space is left, and shrinking the window below ~32×14 turns the whole app into a tiny miniplayer that springs back when the window grows.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/player.png, delete the "coming soon" line above, then uncomment:
![The player with album art and synced lyrics](docs/media/player.png)
-->
<!-- 📸 TO FILL: add docs/media/lyrics.gif, then uncomment:
![Time-synced lyrics scrolling under the player](docs/media/lyrics.gif)
-->
<!-- 📸 TO FILL: add docs/media/audio-output.png, then uncomment:
![Choosing the audio output device in Settings](docs/media/audio-output.png)
-->

### Six catalogs, one search box

**`Tab`** in Search flips between YouTube Music, SoundCloud, Audius, Jamendo, Internet Archive and Radio Browser — or all at once, every result tagged `[SRC]`.

> 🖼️ *Screenshots coming soon.*
<!-- 📸 TO FILL: add docs/media/search.gif, delete the "coming soon" line above, then uncomment:
![Typing a search and playing a result](docs/media/search.gif)
-->
<!-- 📸 TO FILL: add docs/media/sources.png, then uncomment:
![Searching across six catalogs from one box](docs/media/sources.png)
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

**`v`** opens it in a small mpv window; *Auto-continue videos* hands each video off to the next track's, and the mpv window answers `Space`, `.`, `,`, `q`, `f`, `m`.

> 🖼️ *GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/video.gif, delete the "coming soon" line above, then uncomment:
![The music video floating over the terminal](docs/media/video.gif)
-->

### Radio mode — and it knows the song

**`Alt+Shift+R`** turns the whole app into an internet-radio tuner; press **`i`** and Gemini names what's playing on the live stream, **`f`** favorites it.

> 🖼️ *Screenshots coming soon.*
<!-- 📸 TO FILL: add docs/media/radio.png, delete the "coming soon" line above, then uncomment:
![Radio mode as an internet-radio tuner](docs/media/radio.png)
-->
<!-- 📸 TO FILL: add docs/media/radio-id.gif, then uncomment:
![Pressing i to identify the song playing on live radio](docs/media/radio-id.gif)
-->

### Library, queue & downloads

Build playlists in the Library (or let DJ Gem build them), pop the queue with **`c`**, and **`d`** saves a tagged m4a with cover art — **`Shift+D`** grabs the whole list.

> 🖼️ *Screenshots coming soon.*
<!-- 📸 TO FILL: add docs/media/library.png, delete the "coming soon" line above, then uncomment:
![The Library with playlists, favorites and history](docs/media/library.png)
-->
<!-- 📸 TO FILL: add docs/media/queue.png, then uncomment:
![The queue popup over the player](docs/media/queue.png)
-->
<!-- 📸 TO FILL: add docs/media/downloads.png, then uncomment:
![Downloads: tagged m4a files with cover art, played offline](docs/media/downloads.png)
-->

### Local Deck — an offline player for everything on disk

**`Alt+Shift+L`** in the Library opens an immersive player for your downloads and local files — albums, artists, genres, smart lists, no internet needed. The [manual](MANUAL.md) has the full tour.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/localdeck.png, delete the "coming soon" line above, then uncomment:
![The Local Deck browsing local albums](docs/media/localdeck.png)
-->

### Control from anywhere

Media keys, macOS Control Center, Windows SMTC + tray mini player, Linux MPRIS, `ytt -r` from any shell — or a fully headless daemon.

> 🖼️ *Screenshots coming soon.*
<!-- 📸 TO FILL: add docs/media/everywhere.png, delete the "coming soon" line above, then uncomment:
![OS integrations: tray mini player, Control Center, SMTC, MPRIS](docs/media/everywhere.png)
-->
<!-- 📸 TO FILL: add docs/media/tray.png, then uncomment:
![The yututray mini player in the menu bar / tray](docs/media/tray.png)
-->

### Make it yours

14 themes with all 34 color roles hex-editable, 40 animations — from shooting stars and a spinning ASCII donut up to full-canvas showpieces (fireworks, Game of Life, pipes, plasma) — a 10-band EQ with presets, your pick of audio-output device, plus loudness normalization.

> 🖼️ *GIFs coming soon.*
<!-- 📸 TO FILL: add docs/media/themes.gif, delete the "coming soon" line above, then uncomment:
![Cycling through the built-in themes](docs/media/themes.gif)
-->
<!-- 📸 TO FILL: add docs/media/animations.gif, then uncomment:
![Animations, including the spinning ASCII donut](docs/media/animations.gif)
-->
<!-- 📸 TO FILL: add docs/media/showpiece.gif, then uncomment:
![A full-canvas showpiece animation — fireworks, Game of Life, pipes or plasma](docs/media/showpiece.gif)
-->
<!-- 📸 TO FILL: add docs/media/eq.png, then uncomment:
![The 10-band EQ with presets](docs/media/eq.png)
-->

### Retro mode

One toggle makes everything CP437-safe for a bare Linux console or a crusty SSH session — album art included, as honest ASCII art.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/retro.png, delete the "coming soon" line above, then uncomment:
![Retro mode with ASCII album art](docs/media/retro.png)
-->

### Spotify moves in with one command

`ytt transfer import <url>` — checkpointed, resumable, with a match report for anything ambiguous. Setup in the [reference](#reference) below, or let the [manual](MANUAL.md) hold your hand through the whole thing.

> 🖼️ *GIF coming soon.*
<!-- 📸 TO FILL: add docs/media/transfer.gif, delete the "coming soon" line above, then uncomment:
![A Spotify playlist importing in one command](docs/media/transfer.gif)
-->

### The app remembers the keys

**`?`** opens a live cheat sheet that reflects *your* bindings — every key rebindable, the whole UI mouse-aware, interface in English & 한국어.

> 🖼️ *Screenshot coming soon.*
<!-- 📸 TO FILL: add docs/media/help.png, delete the "coming soon" line above, then uncomment:
![The live keybinding cheat sheet](docs/media/help.png)
-->
<!-- 📸 TO FILL: add docs/media/onboarding.gif, then uncomment:
![Beginner Mode's interactive walkthrough on first launch](docs/media/onboarding.gif)
-->
<!-- 📸 TO FILL: add docs/media/context-menu.png, then uncomment:
![Right-click context menu on a track row](docs/media/context-menu.png)
-->

## Essential keys

Press **`?`** in-app for the complete live cheat sheet — it reflects *your* bindings, and every key is rebindable (Settings → Hotkeys). The core:

| Key | Does |
| --- | --- |
| `Space` | Play / pause |
| `,` / `.` | Previous / next (also inside the mpv video window) |
| `←` / `→` · `↑` / `↓` | Seek · volume |
| `s` | Search (`Tab` picks the catalog) |
| `l` / `c` | Library / queue |
| hold `↑`/`↓` · `Shift`+`↑`/`↓` | Fast-scroll a list (accelerates) · extend the selection |
| `f` / `d` | Favorite / download (select rows with `Shift`+`↑`/`↓` or drag, then `d`, to grab many) |
| `Shift+D` | Download the whole list / playlist |
| `Shift+L` | Synced lyrics; click a visible line to seek there |
| `z` / `Z` | Show lyrics 0.1s earlier / later (`[±]` reopens `−/+` for 3s) |
| `v` | Music-video overlay |
| `B` | Collapse / expand the docked control box |
| `Ctrl+R` | DJ Gem streaming |
| `g` | DJ Gem assistant |
| `o` | Settings |
| `Ctrl+Q` | Quit |

> **Korean keyboard?** Shortcuts understand 두벌식 jamo (`ㅂ` works like `q`) — no need to switch input. Prefer the mouse? Everything is clickable, and the wheel rides the volume. Drag across rows to select a range — in Search results just like the Library — and `Ctrl`+click (`⌘`+click on macOS) toggles single rows in and out of the selection. Right-click a row for a context menu, and remap any gesture under `mouse_bindings` in `config.json`. The footer **mouse** button opens the full mouse cheat sheet.

## Troubleshooting

First aid, always: **`ytt doctor`** checks mpv, yt-dlp and ffmpeg and tells you exactly what to fix. `ytt doctor --verbose` digs deeper; `ytt doctor terminal --json` reports what your terminal can do.

### Playback

| Symptom | Fix |
| --- | --- |
| Nothing plays, or it errors on play | mpv or yt-dlp missing — run `ytt doctor`. |
| Sound goes to the wrong device | Settings → Playback → **Audio output** picks from the detected local outputs; **Audio backend** exposes the mpv options. |
| Worked yesterday, not today | YouTube changed something — `ytt tools update`, then `ytt tools status --why`; if a managed update is bad, `ytt tools use system`. |
| Several tracks fail with 403/429 or "YouTube rejected the stream" | Run `ytt doctor --verbose`, check the [cookies reference](#reference), and make sure a supported JS runtime is available; `ytt tools status --why` shows the active yt-dlp. |
| A specific song won't play | It may need sign-in — see the cookies section in the [reference](#reference). |
| The app runs a different yt-dlp than your shell | That's by design (managed copy vs `PATH`) — see *yt-dlp selection* in the [reference](#reference). |

### Install & startup

| Symptom | Fix |
| --- | --- |
| `ytt: command not found` | Open a fresh terminal; still stuck, add the `PATH` line the installer printed. |
| Direct installer / source build is missing helpers | The one-line installers only install `ytt` itself — `ytt doctor` lists what to install and how. |

### Display & terminals

Terminal support varies by emulator — YuTuTui! probes capabilities and falls back where possible. Check your environment with `ytt doctor terminal --json` and compare with the [terminal compatibility matrix](docs/terminal-compatibility.md).

| Symptom | Fix |
| --- | --- |
| No album art | Off by default: Settings → General → **Album art**, then restart. |
| Album art or zoom behaves differently by terminal | Run `ytt doctor terminal --json` and compare with the [terminal matrix](docs/terminal-compatibility.md). |
| Album art looks blocky in VS Code / Apple Terminal | Those terminals have no image protocol — halfblocks are the intended fallback there. |
| Bare Linux console or an old SSH session looks broken | Switch on retro mode (Settings → Graphics): everything redraws CP437-safe, album art becomes ASCII art. |
| `v` (music video) does nothing over SSH / a bare TTY | The video overlay is an mpv GUI window — it needs a desktop session. |

### Spotify import

| Symptom | Fix |
| --- | --- |
| Spotify 403 / "not allowlisted" | Add your own account under *User Management* in your Spotify app dashboard, and check the Client ID for typos. |
| Browser shows INVALID_CLIENT / redirect mismatch | The redirect URI must match **exactly**: `http://127.0.0.1:9271/callback` — IP not `localhost`, correct port, no trailing slash. |
| "could not listen on 127.0.0.1:9271" | That port is busy. Set `spotify.redirect_port` in `config.json` and update the dashboard redirect URI to match. |
| Clicked Connect but no browser opened | On headless/SSH the auth URL is copied to your clipboard and saved to `spotify_auth_url.txt` — paste it into any browser to approve. |
| Spotify import "needs a YouTube Music cookie" | Importing into a YTM playlist/likes needs sign-in; importing into a local Library playlist works without one. See the cookies section in the [reference](#reference). |

### Accounts, scrobbling & OS integration

| Symptom | Fix |
| --- | --- |
| Scrobbles not appearing | Check Settings → Accounts; the daemon reads accounts at start — restart it after connecting. |
| No Control Center / SMTC / MPRIS entry | Settings → Playback → **OS media controls**; it publishes once something has played. |
| Flyout shows "Unknown app" / two entries | Run `ytt register-media-identity` once (two entries = mpv's own media session; auto-disabled on mpv ≥ 0.39). |
| No desktop update notification | Update notices still appear in About/status; desktop notifications are best-effort and depend on terminal, tmux, and OS notification support. |

### Everything else

| Symptom | Fix |
| --- | --- |
| DJ Gem won't respond | Add a free Gemini key in Settings → DJ Gem and switch **Enable DJ Gem** on. |
| Remapped a key into chaos | Settings → General → **Reset keybindings**. |

Still stuck? [Open an issue](https://github.com/Ochichan/Yututui/issues) and mention your OS.

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
ytt -r info                # owner mode, protocol and capabilities (never the token)
ytt -r queue-list          # numbered queue; the current row starts with >
ytt -r queue-play 2        # play queue row 2 (queue numbers start at 1)
ytt -r settings-show       # compact, non-secret settings summary
ytt -r watch --json        # live player/queue/system events as NDJSON (the default topics)
ytt -r watch all           # all published topics: player, queue, settings, system
```

Media keys on i3 / sway: `bindsym XF86AudioPlay exec ytt -r pp`.

Remote control stays on the same machine and is scoped to the current OS user through a private
Unix socket or Windows named pipe. It is not a LAN/HTTP remote: never share or expose its runtime
directory. Queue numbers shown by `queue-list` are 1-based.

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
ytt transfer import <url> --media music-video    # → a separate official-family MV playlist
ytt transfer import liked --media music-video    # the same MV mode for Spotify Liked Songs
ytt transfer import <url> --policy strict        # stricter review-heavy matching
ytt transfer export ytm:<id> --to spotify        # create/append on Spotify (not a live sync)
ytt transfer export ytm:<id> --to spotify:<22-character-playlist-id> --sync --dry-run
                                                  # preview an exact mirror into an existing playlist
ytt transfer backup --dir ~/music-backup --csv   # every YTM playlist → JSON (+CSV)
ytt transfer resume <job-id>                     # continue after a rate-limit/abort
```

Or stay in the TUI: Settings → **Accounts** → *Import from Spotify…* while the music keeps playing. Its fourth mode, **Music video playlist**, writes a separate playlist into Library → Playlists.

**One-time setup (~5 min).** Spotify apps in Development Mode only serve accounts you explicitly allowlist, so everyone brings their own personal app. Under [Spotify's 2026 Dev-Mode rules](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide), the app owner needs Premium, a new app gets one Client ID, and it can serve up to five allowlisted users. There is no client *secret* — PKCE doesn't use one.

1. Sign in at [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard) and click **Create app**.
2. Give it any **App name** and **App description** (e.g. `yututui`).
3. Under **Redirect URIs**, add exactly `http://127.0.0.1:9271/callback` and click **Add**. It must be the loopback IP literal `127.0.0.1`, **never `localhost`** (Spotify rejects `localhost`). Using a different port? Set `spotify.redirect_port` in `config.json` and match it here.
4. Under **Which API/SDKs are you planning to use?**, tick **Web API**.
5. Accept the terms and **Save**.
6. Open the app → **Settings** and copy the **Client ID** (you do *not* need the Client secret).
7. Open **User Management** (in the app's settings) and add your own account — your name plus the email on your Spotify account. New Dev-Mode apps serve up to five such allowlisted users.
8. In ytt: **Settings → Accounts → Spotify**, paste the Client ID, and choose **Connect** (or run `ytt auth spotify --client-id <ID>`). Your browser opens Spotify's approval page — approve it and you're done. On headless/SSH where no browser opens, the URL is copied to your clipboard and saved to `spotify_auth_url.txt`, so you can open it on any device.

Matching is metadata-based (NFKC-normalized, CJK-safe) and resolves Spotify imports cache-first, album-aware, and YTM-catalog-first before falling back to public YouTube videos. The CLI default is `--policy balanced`; use `--policy strict` for conservative review-heavy matching, `--policy aggressive` for fewer review rows, and `--allow-user-videos` only if generic public uploads are acceptable. Anything still ambiguous lands in the job report instead of being silently guessed — re-run with `--take-best` / `--min-score`, or preview big playlists with `--dry-run` and then `ytt transfer resume <job-id>`.

`--media music-video` works with a Spotify playlist or `liked` and creates a separate `<source> (Music Videos)` playlist unless you supply a destination name. It prefers YouTube Music's OMV / OfficialSourceMusic classifications and strongly corroborated official channels. That is a best-effort official-family check, not a 100% guarantee: the public APIs do not expose a definitive “official music video” flag. Hard-rejected user uploads cannot be forced through review, and unresolved candidates stay in the report.

The ordinary `--to spotify` export is intentionally non-destructive: it finds or creates a playlist (creation uses Spotify's current `POST /me/playlists` API) and appends missing matches. It does not remove Spotify-only tracks, reproduce duplicate positions, reorder the playlist, or keep watching for later edits.

For a destructive, one-shot exact mirror, use an explicit playlist ID with `--to spotify:<22-character-playlist-id> --sync`. Only a playlist owned by the connected account is accepted. Run `--dry-run` first: the preview shows additions, removals and reordering, and nothing is changed if even one source row is unresolved or the source was truncated. The real run preserves source order and duplicate occurrences and removes destination-only tracks. Without `--yes` it previews and asks before replacing anything; `ytt transfer resume <job-id>` builds a fresh preview and asks again (`resume <job-id> --yes` deliberately skips that confirmation).

</details>

<details>
<summary><b>Sign-in cookies & file locations</b></summary>

**Cookies (optional).** Public songs play anonymously — only members-only/region-locked tracks and account playlists need this. Export your YouTube Music cookies in **Netscape format** to `~/Music/yututui/cookies.txt` (Windows: `%USERPROFILE%\Music\yututui\cookies.txt`) and restart. **Treat that file like a password**, and export the *incognito way*: sign in inside a private window, export `cookies.txt` from that tab, then close the window — a session whose browser is gone never gets rotated or signed out. A good export has `SAPISID`/`SID` lines in it.

**Config & data.**

- Config: `~/Library/Application Support/yututui/config.json` (macOS) · `~/.config/yututui/config.json` (Linux) · `%APPDATA%\yututui\config.json` (Windows) — with `playlists.json`, `scrobble-queue.jsonl` and `transfers/` alongside.
- Downloads: `~/Music/yututui` — change via the **Download dir** setting or `YTM_DOWNLOAD_DIR`.
- `GEMINI_API_KEY` and `YTM_DOWNLOAD_DIR` override saved settings at launch.

**Portable personal-data export.** Choose **Settings (`o`) → General → Export personal data**, or run:

```sh
ytt data export                         # save to the OS Downloads folder
ytt data export --to ~/existing-folder # choose an existing directory
```

`--to` takes a directory, not a filename, and does not create it. A destination where another local account could replace the finished file is rejected. The result is a new, owner-private, never-overwritten, versioned JSON file containing sanitized portable settings; track and radio favorites; listening and radio history; playlists and safe track metadata/public catalog IDs; and recommendation signals, artist affinities and station preferences.

If the primary app or daemon is running, the CLI exports that owner's current in-memory state. With additional `--new-instance` players, the CLI still exports only the advertised primary; use each secondary's Settings screen for its live state. Offline export refuses to read the stores while any current-version ytt owner is active.

It excludes authentication cookies, API keys, OAuth tokens and account identifiers; every filesystem path and machine-specific audio setting; playable, origin, artwork and radio-stream URLs; downloaded/recorded media, manifests and sidecars; pending scrobbles, transfer jobs/reports and session queues; AI usage logs, generated caches, artwork caches and application logs; managed-tool binaries and paths, desktop geometry and recovery backups.

The JSON is **not encrypted** and still contains private listening history, so store or share it accordingly. This version is export-only: there is no import or restore command yet.

</details>

<details>
<summary><b>yt-dlp selection</b></summary>

**yt-dlp keeps itself fresh.** YouTube changes weekly, so `ytt` maintains its own current yt-dlp (SHA-256-verified from github.com) and uses whichever of {managed, system} is newer. It may therefore run a different yt-dlp than the one your shell prints with `yt-dlp --version`. To see the actual choice and candidates:

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

## Thanks & license

🙏 Huge thanks to **[@ZZNN75](https://github.com/ZZNN75)** for the real QA hours — the rough edges you *won't* hit are smooth because they hit them first. 🫡

MIT. Fork it, ship it, do whatever you want.
