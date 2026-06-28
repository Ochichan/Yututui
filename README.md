# ytm-tui

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

*A YouTube Music player that lives inside your terminal — because somebody looked at a perfectly good app with buttons and thought, "what if this were all keyboard, used less memory, and made me feel slightly superior in coffee shops?"*

`ytt` (that's the command you'll type) plays music from YouTube Music right inside your terminal window. No browser tab quietly eating your RAM, no thumbnail-shaped ads, no mouse required — though there's mouse support if you miss it. Just you, some text on a black screen, and a frankly unreasonable number of keyboard shortcuts.

It's fast, it's light, and it was made by one person who probably should have been asleep. It works on their machine. Now, hopefully, on yours too.

---

## Table of contents

- [Is this for me?](#is-this-for-me)
- ["I just want music. Now."](#i-just-want-music-now-the-30-second-version)
- [Step 1 — Install the two helpers](#step-1--install-the-two-helpers)
- [Step 2 — Install ytm-tui](#step-2--install-ytm-tui)
- [Step 3 — Run it](#step-3--run-it)
- [How to actually use it](#how-to-actually-use-it)
- [The keys](#the-keys)
- [Making it yours (Settings)](#making-it-yours-settings)
- [Radio: music that never stops](#radio-music-that-never-stops)
- [The AI assistant (optional)](#the-ai-assistant-optional)
- [Where your downloads go](#where-your-downloads-go)
- [Signing in with cookies (optional)](#signing-in-with-cookies-optional)
- [When things go wrong](#when-things-go-wrong)
- [Special thanks](#special-thanks)
- [License](#license)

---

## Is this for me?

Maybe! You'll probably like `ytm-tui` if:

- You live in the terminal anyway and switching to a browser feels like a long walk.
- You want music with a tiny memory footprint and no ads cluttering the view.
- You think keyboard shortcuts are a love language.

You'll probably **not** like it if you want big album covers, a mouse-driven everything, and a polished phone app. That's fine. We can still be friends.

> **About the screenshot.** There isn't one. Picture a music player, but it's made of text. Or — better idea — just install it (it takes about two minutes) and look at the real thing instead of my cropping skills.

---

## "I just want music. Now." (the 30-second version)

If you're on a Mac and you already have [Homebrew](https://brew.sh):

```sh
brew install mpv yt-dlp                 # the two helpers ytm-tui needs
brew install Ochichan/tap/ytm-tui       # ytm-tui itself
ytt                                      # run it
```

Then press `/`, type a song name, hit `Enter`, and pick something with `↑`/`↓` + `Enter`. Done. Music.

Everyone else (Windows, Linux, or "what's a Homebrew?"), read on — it's still easy, I promise.

---

## Step 1 — Install the two helpers

`ytm-tui` doesn't reinvent the wheel. It quietly hands the actual "find and play the sound" job to two well-loved free programs:

- **[mpv](https://mpv.io)** — the thing that makes noise come out of your speakers.
- **[yt-dlp](https://github.com/yt-dlp/yt-dlp)** — the thing that fetches the audio from YouTube.

You only have to install these **once**. Pick your operating system:

| Your computer | Type this |
| --- | --- |
| **macOS** (with [Homebrew](https://brew.sh)) | `brew install mpv yt-dlp` |
| **Windows** (with [Scoop](https://scoop.sh)) | `scoop install mpv yt-dlp` |
| **Linux** (Debian/Ubuntu) | `sudo apt install mpv yt-dlp` |
| **Linux** (Arch) | `sudo pacman -S mpv yt-dlp` |

> **What's a "package manager"?** It's an app-store-for-the-terminal that installs programs with one line. Homebrew (Mac), Scoop (Windows), and apt/pacman (Linux) are the popular ones. If you don't have the one in the table, click its link and install it first — it's a one-time thing.

How do you know it worked? Type `mpv --version` and `yt-dlp --version`. If each prints a bunch of text instead of "command not found," you're good.

---

## Step 2 — Install ytm-tui

The **easy way** (recommended) — let a package manager do everything:

```sh
# macOS
brew install Ochichan/tap/ytm-tui

# Windows
scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket
scoop install ytm-tui
```

The **from-the-source way** — if you grabbed this repository as a folder:

```sh
# macOS / Linux
./install.sh

# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

The installer is polite: it uses a ready-made program when one exists for your computer, and otherwise builds it from scratch. Building from scratch needs [Rust](https://rustup.rs) (install that first), takes a few minutes, and is a great moment to go make a coffee. It also nudges the install location onto your `PATH` so you can run `ytt` from anywhere, and double-checks that mpv and yt-dlp are present.

---

## Step 3 — Run it

```sh
ytt
```

That's it. Three letters.

The very first thing to learn: press **`?`** at any time. That pops up the full, always-up-to-date list of every key and what it does — so even if you forget everything below, the app remembers it for you.

> **"`ytt: command not found`"?** Close your terminal, open a fresh one, and try again — your `PATH` needs a moment to catch up. Still stuck? See [When things go wrong](#when-things-go-wrong).

---

## How to actually use it

`ytm-tui` is a few different **screens** that you hop between. You're always on one of them. Here's the whole map:

- **🏠 Player** — the home base. Shows what's playing, the progress bar, volume, and a row of media buttons. Almost everything starts here.
- **🔍 Search** — press `/`, type what you want, press `Enter`. Move through the results with `↑`/`↓`, and `Enter` plays the highlighted one.
- **📚 Library** — press `l`. Your favorited songs and your downloaded files, all in one place. Browse `All`, `Downloads`, and your favorites.
- **📜 Queue** — press `c`. The list of what's playing next. `Enter` jumps straight to a song; `Delete` kicks one out of the line.
- **🎤 Lyrics** — press `L` (that's Shift+L). If the song has time-synced lyrics, they scroll along with the music. Karaoke for one.
- **⚙️ Settings** — press `,` (comma). Every knob and dial; see [Making it yours](#making-it-yours-settings).
- **🤖 AI assistant** — press `a`. Talk to it in plain English (optional — needs a free key; see below).

To **close** any panel and step back, press **`q`**. To **quit** the whole app, press **`Ctrl+Q`**. To teleport back to the Player from anywhere, press **`Ctrl+H`**.

A normal first session looks like this:

1. Press `/` and search for a song. → 2. `Enter` to play it. → 3. Press `f` to ♥ favorite the ones you love. → 4. Press `Ctrl+R` to flip on radio so the music keeps going forever. → 5. Press `,` to dive into Settings and make it pretty.

---

## The keys

Don't memorize these. Press `?` in the app and the cheat sheet is right there. But for the curious, here are the everyday ones.

### On the Player (home base)

| Key | What it does |
| --- | --- |
| `Space` | Play / pause |
| `←` / `→` | Rewind / fast-forward a few seconds |
| `↑` / `↓` | Volume up / down |
| `n` / `p` | Next / previous song |
| `f` | Rate the current song — tap to cycle 🤔 neutral → 👍 like → 👎 dislike (like = favorite; dislike means you'll hear less like it) |
| `/` | Open Search |
| `l` | Open your Library |
| `c` | Open the Queue (what's next) |
| `L` | Show / hide lyrics |
| `d` | Download the current song |
| `s` | Shuffle on / off |
| `r` | Repeat: off → all → one |
| `a` | Open the AI assistant |
| `,` | Open Settings |
| `?` | Show **every** key (the in-app cheat sheet) |
| `Ctrl+R` | Turn radio (autoplay) on / off |
| `Ctrl+H` | Jump back to the Player |
| `Ctrl+Q` | Quit the whole app |
| `q` | Go back / close the current panel |

### In any list (Search, Library, Queue)

| Key | What it does |
| --- | --- |
| `↑` / `↓` | Move up / down the list |
| `Enter` | Play (or jump to) the highlighted item |
| `f` | Favorite it |
| `d` | Download it |
| `Delete` | Remove it (from the queue or library) |
| `q` | Close the panel |

### For audio tinkerers

| Key | What it does |
| --- | --- |
| `e` | Cycle through equalizer presets |
| `N` | Toggle loudness normalization (evens out quiet/loud songs) |
| `>` / `<` | Speed up / slow down playback |

> **Korean keyboard tip:** you don't have to switch to English input — the shortcuts understand 두벌식 jamo, so `ㅂ` works just like `q`. Small thing, but a nice one.

Every single key above is **remappable** — if `q` for "back" offends you, change it (Settings → Keys).

---

## Making it yours (Settings)

Press `,` to open Settings. Move between the tabs at the top with `Tab` / `Shift+Tab`, move up and down with `↑`/`↓`, and change a value with `←`/`→` (or `Enter` for text fields). When you're done, press `q` — it **saves automatically** on the way out.

Here's what each tab does:

- **General** — where your cookies file lives, where downloads are saved, mouse support on/off, album art on/off, whether music auto-starts when you launch, plus two "reset" buttons (one just for keys, one for *everything*, for when you've made a mess).
- **Playback** — playback speed, how far the `←`/`→` seek keys jump, and *gapless* playback (removes the little silence between songs).
- **EQ** — a 10-band equalizer with ready-made presets (Flat, Bass Boost, and friends), plus a **Normalize** toggle so a whisper-quiet song and a wall-of-sound song play at roughly the same loudness.
- **AI** — pick the assistant's model, paste your key, switch on the autoplay **radio**, and set how adventurous radio is.
- **Theme** — pick a color scheme. There are several. One of them is bound to match your soul.
- **Colors** — not satisfied with the presets? Hand-pick individual colors with hex codes (`#ff8800` and the like).
- **Keys** — remap any shortcut to any key you want.

> **A small "gotcha":** a few settings are labeled **"(next launch)"** — things like mouse, album art, and gapless. Those only take effect after you quit and reopen `ytt`. This is normal; the app isn't ignoring you.

---

## Radio: music that never stops

Press **`Ctrl+R`** and `ytm-tui` turns into a little radio station built around whatever you're currently playing. When your queue starts running low, it quietly lines up more songs that fit the mood, so the music simply... never stops. It's the lazy person's dream, and we mean that as a compliment.

You can pick how adventurous it is in **Settings → AI → Radio mode**:

- **Focused** — sticks close to home. More of what you're already hearing.
- **Balanced** — a sensible middle. The safe default.
- **Discovery** — wanders off the beaten path to find things you haven't heard.

Exactly *how* it chooses the next song is a closely guarded trade secret — mostly because the honest answer is "a lot of trial, a lot of error, and some quiet pride." It just works. Enjoy it.

---

## The AI assistant (optional)

This one's a bonus, and it's completely optional. If you have a free **Google Gemini API key**, you can press `a` and just *talk* to your music player in plain English:

- "play some lo-fi"
- "queue three upbeat songs for cleaning"
- "what's playing right now?"
- "skip this and play something calmer"

It understands what you mean, goes and finds the music, and does it for you. No memorizing commands.

To switch it on: get a key from Google's AI Studio (it's free), then paste it into **Settings → AI → API key**. That's the whole setup.

No key? No problem — the assistant simply stays asleep, and absolutely everything else in the app works perfectly without it.

---

## Where your downloads go

Press `d` on a song and it gets saved to disk so you can play it even offline. By default they land here:

- **macOS:** `~/Music/ytm-tui`
- **Windows:** `%USERPROFILE%\Music\ytm-tui`
- **Linux:** your Music folder, in a `ytm-tui` subfolder

Files go straight into that one folder — no per-artist subfolders to dig through. Your downloaded tracks then show up automatically in the Library under **All** and **Downloads**, sitting happily next to your streamed music.

Want them somewhere else? Change **Download dir** in Settings, or set the `YTM_DOWNLOAD_DIR` environment variable.

---

## Signing in with cookies (optional)

Short version: **you probably don't need this.** Without signing in, `ytm-tui` searches and plays public tracks just fine, anonymously.

You'd only want to sign in if you need access to songs that require a logged-in YouTube Music account (members-only or region-locked tracks, that sort of thing). To do it, you give the app your browser's "cookies" — a little file that proves you're you.

By default it looks for a `cookies.txt` file here:

- **macOS:** `~/Music/ytm-tui/cookies.txt`
- **Windows:** `%USERPROFILE%\Music\ytm-tui\cookies.txt`

Create that folder, export your YouTube / YouTube Music cookies from your browser in **Netscape format** as `cookies.txt` (a browser extension can do this), drop the file in, and restart `ytt`.

```sh
# macOS — make the folder
mkdir -p ~/Music/ytm-tui
```

```powershell
# Windows (PowerShell) — make the folder
New-Item -ItemType Directory "$HOME\Music\ytm-tui" -Force
```

You can also point to a different file in **Settings → Cookies file**.

> ⚠️ **Treat this file like a password.** It can act like a logged-in browser session, so don't share it, don't commit it to a repo, don't post it anywhere.

---

## When things go wrong

Deep breath. It's almost always one of these:

| Symptom | The fix |
| --- | --- |
| **Nothing plays, or it errors the instant you hit play** | mpv or yt-dlp probably isn't installed (or isn't on your `PATH`). Re-do [Step 1](#step-1--install-the-two-helpers). |
| **`ytt: command not found`** | Open a brand-new terminal window. If it still happens, the install folder isn't on your `PATH` — the installer printed the exact line to add; paste it into your shell's config file. |
| **A specific song won't play** | Some tracks need you signed in. See [Signing in with cookies](#signing-in-with-cookies-optional). |
| **It worked yesterday, now playback fails everywhere** | YouTube changes things constantly and breaks yt-dlp. Update it: `brew upgrade yt-dlp` (Mac), `scoop update yt-dlp` (Windows), or your package manager (Linux). |
| **No album art showing** | It's off by default and only works in some terminals. Turn on **Album art** in Settings → General, then **restart** (it's a "next launch" setting). |
| **I changed a key and now everything's chaos** | Settings → General → **Reset keybindings**. All is forgiven. |

Still stuck? Open an issue on the [GitHub repo](https://github.com/Ochichan/ytm-tui) and describe what happened. Bonus points for telling us your OS.

---

## Special thanks

🙏 A huge, heartfelt shout-out to **[@ZZNN75](https://github.com/ZZNN75)** — who put in real hours on QA: poking at every corner, breaking things on purpose, and patiently reporting each weird little edge case so *you* never have to stumble into them. A lot of the rough spots you **won't** hit are smooth specifically because they hit them first and said something. Absolute legend. 🫡

If `ytm-tui` feels solid, a meaningful chunk of that credit belongs to them.

---

## License

MIT. Do whatever you want with it. Seriously — fork it, ship it, frame it, ignore it. It's yours.
