# ytm-tui

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

### [▶ Live demo & feature tour → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

YouTube Music in your terminal. Fast, keyboard-driven, no browser tab eating your RAM, no ads. DJ Gem radio, real album art, and remote control — all from a three-letter command: `ytt`.

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

- **DJ Gem radio** — press **`Ctrl+R`** and it builds an endless station around what you're hearing. Three moods: Focused, Balanced, Discovery. Press **`w`** to see, in plain language, why it picked each song.
- **Real album art** — actual cover images drawn right in the Player, on terminals that support them. Time-synced lyrics scroll underneath (**`Shift+L`**).
- **Remote control** — drive it from another terminal or your media keys: `ytt -r pp`, `ytt -r next`, `ytt -r status`.
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
| `Ctrl+R` | Toggle DJ Gem radio |
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
ytt -r radio on    # turn the radio on
ytt -r status      # one-line "now playing"
ytt -r quit        # stop and close
```

Wire it to your media keys (i3 / sway):

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
```

Launching `ytt` twice won't start a second player fighting over your speakers — it just reminds you how to reach the one you've got. (`ytt --new-instance` if you really want two.) Run `ytt -r --help` for the full command list.

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
| Remapped a key into chaos | Settings → General → **Reset keybindings**. |

Still stuck? [Open an issue](https://github.com/Ochichan/ytm-tui/issues) and mention your OS.

---

## Sign-in & file locations

**Cookies (optional).** You almost certainly don't need this — public songs search and play fine anonymously. To reach members-only or region-locked tracks, export your YouTube Music cookies in **Netscape format** to `cookies.txt` (macOS: `~/Music/ytm-tui/cookies.txt`, Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`) and restart `ytt`. **Treat that file like a password.** You can also point to it in Settings → General.

**Config & downloads.**
- Config: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows).
- Downloads default to `~/Music/ytm-tui`; change it with the **Download dir** setting or `YTM_DOWNLOAD_DIR`.
- `GEMINI_API_KEY` and `YTM_DOWNLOAD_DIR` environment variables override the saved settings at launch.

---

## Special thanks

🙏 Huge thanks to **[@ZZNN75](https://github.com/ZZNN75)** for the real QA hours — poking at every corner and breaking things on purpose so you won't have to. A lot of the rough edges you *won't* hit are smooth because they hit them first. 🫡

## License

MIT. Fork it, ship it, do whatever you want.
