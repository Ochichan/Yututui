# ytm-tui

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

*A YouTube Music player that lives inside your terminal — because somebody looked at a perfectly good app full of buttons and thought, "what if this were all keyboard, used less memory, and made me feel slightly superior in coffee shops?"*

`ytt` (that's the three-letter command you'll type) plays music from YouTube Music right inside your terminal window. No browser tab quietly eating your RAM, no thumbnail-shaped ads, no mouse required — though there's full mouse support if you miss it. Just you, some text on a dark screen, and a frankly unreasonable number of keyboard shortcuts.

It's fast, it's light, and it was built by one person who probably should have been asleep. It works on their machine. Now, hopefully, on yours too.

<!-- 📸 screenshot: hero shot — the Player screen with a song playing, album art, and the status line -->

---

## Table of contents

- [Is this for me?](#is-this-for-me)
- [Just give me music (the 60-second version)](#just-give-me-music-the-60-second-version)
- [Installing it properly](#installing-it-properly)
  - [Step 1 — the two helpers](#step-1--the-two-helpers-mpv--yt-dlp)
  - [Step 2 — ytm-tui itself](#step-2--ytm-tui-itself)
  - [Step 3 — run it](#step-3--run-it)
- [A tour of the screens](#a-tour-of-the-screens)
  - [The top bar (on every screen)](#the-top-bar-on-every-screen)
  - [The Player (home base)](#the-player-home-base)
  - [Search](#search)
  - [Library](#library)
  - [Queue](#queue)
  - [Lyrics](#lyrics)
  - [The AI assistant](#the-ai-assistant)
  - [Why these picks? (the AI's reasoning)](#why-these-picks-the-ais-reasoning)
  - [About & Help](#about--help)
- [Every key, explained](#every-key-explained)
- [Settings — every knob, one by one](#settings--every-knob-one-by-one)
  - [General](#general-tab)
  - [Playback (speed, seek, EQ)](#playback-tab)
  - [Hotkeys](#hotkeys-tab)
  - [Graphics (theme, colors, animations)](#graphics-tab)
  - [AI (assistant + radio)](#ai-tab)
- [Radio: music that never stops](#radio-music-that-never-stops)
- [Downloads: keeping music offline](#downloads-keeping-music-offline)
- [Remote control: drive it from anywhere](#remote-control-drive-it-from-anywhere)
- [Signing in with cookies (optional)](#signing-in-with-cookies-optional)
- [Where your settings are kept](#where-your-settings-are-kept)
- [When things go wrong](#when-things-go-wrong)
- [Special thanks](#special-thanks)
- [License](#license)

---

## Is this for me?

Probably yes, if any of these sound like you:

- You already live in the terminal, and opening a browser feels like a long walk.
- You want music with a tiny memory footprint and zero ads in sight.
- You think keyboard shortcuts are a love language.
- You like the idea of a music player you can *completely* recolor, rebind, and bend to your taste.

Probably **not** for you if you want giant album covers, an everything-is-a-mouse-click experience, and a glossy phone app. That's completely fair. We can still be friends.

> **About the screenshots.** They're being added. Picture, for now, a music player made entirely of text and the occasional emoji. Or — better idea — just install it (about two minutes) and look at the real thing.

---

## Just give me music (the 60-second version)

On a Mac with [Homebrew](https://brew.sh)? Paste these three lines and you're done:

```sh
brew install mpv yt-dlp                 # the two helpers ytm-tui leans on
brew install Ochichan/tap/ytm-tui       # ytm-tui itself
ytt                                      # run it
```

Then: press `/`, type a song name, hit `Enter`, move with `↑`/`↓`, and press `Enter` again to play. That's it. Music.

On Windows or Linux, or not sure what Homebrew is? The next section walks you through it slowly. It's still easy — promise.

---

## Installing it properly

`ytm-tui` is the friendly face. The actual "find the song and make sound come out" work is handed to two well-loved, free programs. You install those once, then install `ytm-tui`, then run it. Three steps.

### Step 1 — the two helpers (mpv + yt-dlp)

- **[mpv](https://mpv.io)** — the engine that actually makes sound come out of your speakers.
- **[yt-dlp](https://github.com/yt-dlp/yt-dlp)** — the part that fetches the audio from YouTube.

You install these **once**. Find your operating system in the table:

| Your computer | Type this |
| --- | --- |
| **macOS** (with [Homebrew](https://brew.sh)) | `brew install mpv yt-dlp` |
| **Windows** (with [Scoop](https://scoop.sh)) | `scoop install mpv yt-dlp` |
| **Linux** (Debian/Ubuntu) | `sudo apt install mpv yt-dlp` |
| **Linux** (Arch) | `sudo pacman -S mpv yt-dlp` |

> **What's a "package manager"?** Think of it as an app store for the terminal — it installs programs with a single line. Homebrew (Mac), Scoop (Windows), and apt/pacman (Linux) are the popular ones. If you don't have the one in your row yet, click its link above and install it first. Also a one-time thing.

**Did it work?** Type `mpv --version`, then `yt-dlp --version`. If each spits out a wall of text instead of "command not found," you're good to go.

### Step 2 — ytm-tui itself

**The easy way (recommended)** — let your package manager handle everything:

```sh
# macOS
brew install Ochichan/tap/ytm-tui

# Windows
scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket
scoop install ytm-tui
```

**The from-source way** — if you downloaded this repository as a folder:

```sh
# macOS / Linux
./install.sh

# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

The installer is polite: it grabs a ready-made build if one exists for your computer, and otherwise builds it from scratch. Building from scratch needs [Rust](https://rustup.rs) (install that first), takes a few minutes, and is a great excuse to go make coffee. It also nudges the install folder onto your `PATH` so `ytt` runs from anywhere, and double-checks that mpv and yt-dlp are present.

### Step 3 — run it

```sh
ytt
```

Three letters. That's the whole command.

**The single most useful thing to learn first:** press **`?`** at any time. It pops up the complete, always-current list of every key and what it does. So even if you forget everything in this README, the app itself remembers it for you.

> **Seeing `ytt: command not found`?** Close your terminal, open a fresh window, and try again — your `PATH` just needs a moment to catch up. Still stuck? Jump to [When things go wrong](#when-things-go-wrong).

---

## A tour of the screens

`ytm-tui` is built from a handful of **screens**. You're always looking at exactly one of them, and you hop between them with single keys. Here's the whole map, with what each one is for.

### The top bar (on every screen)

Across the very top of every screen runs a slim navigation bar that looks roughly like this:

```
✨ ytm-tui │  Player   Search   Library   Settings   AI
```

- The **`✨` spark** on the far left is the master switch for animations. Click it to turn all the motion on or off. (When animations are off, the spark quietly disappears — the spot is still clickable to bring them back.)
- **`ytm-tui`** is clickable too — it opens the **About** card.
- The five words after the divider — **Player, Search, Library, Settings, AI** — are tabs. The one you're on is highlighted; click any other to jump straight there. (The Queue and Lyrics live *inside* the Player, so they aren't tabs of their own.)

Every screen also ends with a dim **`?  keybindings`** footer — click it (or press `?`) for the full cheat sheet.

### The Player (home base)

<!-- 📸 screenshot: Player screen, fully annotated — title, seekbar, controls, status glyphs, album art -->

This is where you'll spend most of your time. From top to bottom:

**The title line.** Centered and bold, it shows what's playing: `♥ Song Title — Artist`. The little `♥` heart only appears if the song is one of your favorites. Before you've played anything it reads **`Nothing playing — press / to search`**. (When something noteworthy happens — a download finishes, an error pops up — a brief status message flashes here instead, green for good news, red for trouble.)

**The seek bar.** A progress bar spanning the width of the screen, with the time tucked inside it as `elapsed / total` — for example `2:34 / 4:15`. You can **click anywhere on the bar to jump** to that point in the song.

**The control buttons.** A centered row of media controls:

```
 ⇤     ▸     ⇥       vol  -  50%  +
```

- `⇤` previous track · `▸` / `‖` play-or-pause (it shows `▸` when paused, `‖` when playing) · `⇥` next track. All clickable.
- `vol  -  50%  +` is the volume cluster — click `-` or `+` to nudge it; the number in the middle is the current volume.

**The status line.** This is the row of little symbols that tells you, at a glance, everything about *how* the music is playing. Most of these are clickable. Here's the full decoder ring:

| You see | It means | Click it to… |
| --- | --- | --- |
| `▸ playing` / `‖ paused` | Current playback state | — |
| `1/3` | You're on track 1 of 3 in the queue | open the **Queue** |
| `🤔` / `👍` / `👎` | Your rating of this song (neutral / liked / disliked) | cycle the rating |
| `S:🔀` / `S:✗` | Shuffle on / off | toggle shuffle |
| `R:✗` / `R:🔁` / `R:🔂` | Repeat: off / whole queue / single song | cycle repeat |
| `1.5x` | Playback speed (only shown when it isn't normal `1.0x`) | — |
| `eq:Flat` | The active equalizer preset | open the **EQ** menu |
| `norm` | Loudness normalization is on | — |
| `radio:balanced` | Radio (autoplay) is on, and which mode | open the **Radio mode** menu |
| `⬇ 45%` / `⬇ ✓` / `⬇ ✗` | A download is in progress / finished / failed | — |

> Two of those open little pop-up menus right under the label: clicking **`eq:`** drops down the equalizer presets (Flat, Bass, Treble, Vocal, Rock, Jazz), and clicking **`radio:`** drops down the three radio moods (Focused, Balanced, Discovery). Pick one and it applies instantly.

**The big middle area.** This fills with whatever you've turned on: the **album art** (a real image if your terminal supports it), the **lyrics** panel (see below), and — if you like a bit of life on screen — gentle [animations](#graphics-tab) drifting in the empty space.

### Search

<!-- 📸 screenshot: Search screen — input box, Search button, results list with hearts and durations -->

Press **`/`** to search. You get:

- **A search box** at the top, labelled `Search` (or `Search · anonymous` when you're not signed in — that's normal, public songs play fine without an account), with a **⌕** magnifying-glass icon on its left. Type your query; press `Enter`, or click the **⌕** icon or the **Search** button beside it.
- **A results list** below. Each row reads `♥ Song Title — Artist  (3:48)` — the heart shows if it's already a favorite, and the time in parentheses is the song's length. The highlighted row is marked with `▶`.
- While a search is running you'll see **`Searching…`**.

Move with `↑`/`↓` and press `Enter` to play the highlighted song right away — your current queue is kept, so whatever's lined up still plays afterward. Press `\` (or **right-click** a row) to add a song to the queue instead, without interrupting what's playing (`Added to queue` flashes to confirm). You can also **double-click** any row to play it.

### Library

<!-- 📸 screenshot: Library screen — the four tabs with counts, a track list, the filter row -->

Press **`l`** for your personal collection. Across the top are four tabs, each with a live count:

```
 All (12)   Favorites (4)   History (8)   Downloads (2)
```

- **All** — everything you've touched, gathered into one list (a song that's both favorited and recently played appears just once).
- **Favorites** — songs you've hearted. To add one, press `f` on any track anywhere in the app.
- **History** — what you've played recently, newest first.
- **Downloads** — the audio files saved in your download folder, ready to play offline.

Switch tabs by clicking them, or with `Tab` / `Shift+Tab`. Inside a tab, move with `↑`/`↓`, then press `Enter` (or double-click) to play the selected song now — your current queue is kept. Press `\` (or **right-click** a row) to add it to the queue instead, or `P` (`Shift`+`P`) to play the whole tab as a fresh queue.

**Filtering.** Press **`/`** while in the Library to filter the current tab — start typing and the list narrows live, with a running `[5 matches]` count. Press `Enter` to keep the filter, or `Esc` to clear it.

**Removing things.** Every tab except *All* shows a red **`✗`** at the end of each row. Click it to remove that song — from Favorites, from History, or (for Downloads) **delete the actual file from your disk**. Because that last one is permanent, deleting a download first asks you to confirm with a clear `Delete (Enter) / Cancel (Esc)` prompt.

> Empty tabs explain themselves, e.g. *"No favorites yet — press f on a track to save it."* So you're never staring at a blank box wondering what to do.

### Queue

<!-- 📸 screenshot: Queue popup over the Player — numbered up-next list with the playing track marked -->

The queue is the line of songs waiting to play. It opens as a **pop-up** over the Player — press **`c`**, or click the `1/3` position counter in the status line.

- Each row is numbered: `▸  1 Song — Artist`. The `▸` marks what's playing right now.
- Press `Enter` (or **double-click**) on any row to jump straight to that song.
- Press `Delete`, or click the red **`✗`** on a row, to drop a song out of the line.

Close it with `q`, or by clicking anywhere outside the pop-up.

### Lyrics

<!-- 📸 screenshot: Player with the synced lyrics panel scrolling, current line highlighted -->

Press **`L`** (that's `Shift`+`L`) to show or hide lyrics. They appear in the Player's middle area, under the album art.

When the song has **time-synced** lyrics, they scroll along with the music and the current line stays highlighted in the center — karaoke for one. While they load you'll see `Fetching lyrics…`; if none exist for that track, `No synced lyrics found.` (Only properly time-synced lyrics are shown — plain untimed lyrics aren't displayed.)

### The AI assistant

<!-- 📸 screenshot: AI assistant — the chat transcript, a suggestions panel, the "Ask" input, the mascot -->

This is an optional bonus. Press **`a`** and you can talk to your music player in plain English. (It needs a free Google Gemini key — see the [AI settings](#ai-tab) — but everything else in the app works without it.)

The screen has three parts: a **conversation** area in the middle, a **suggestions** panel that appears when the AI proposes songs, and an **`Ask`** box at the bottom where you type. The current AI model's name sits in the top-right corner.

Just type what you want and press `Enter`:

- *"play some lo-fi beats"*
- *"queue three upbeat songs for cleaning"*
- *"what's playing right now?"*
- *"skip this and play something calmer"*

You'll see your message tagged `you`, a brief `…thinking`, then the AI's reply tagged `ai`. When it suggests songs, they show up in the suggestions panel as `♥ Title — Artist` (heart if already a favorite). Press `Tab` to hop into that list, move with `↑`/`↓`, and `Enter` to play one.

> No key set up yet? The screen greets you with setup instructions and a few example prompts — and a tiny animated mascot keeps you company until your first message. Nothing breaks; the assistant just naps until you add a key.

### Why these picks? (the AI's reasoning)

<!-- 📸 screenshot: the "Why these AI picks" overlay — numbered picks with role badges and reasons -->

Curious why the radio queued what it did? Press **`w`** for a small overlay titled **"Why these AI picks."** It lists the upcoming songs and, for each, a short plain-language note about the role it plays in the set and what made it a good fit — the human-readable "why," without the math. (If the AI hasn't lined anything up yet this session, the overlay simply has nothing to show.) Press `w` or `Esc` to close it.

### About & Help

- **About** — press **`F1`** (or click `ytm-tui` in the top bar) for a card with the app icon, version, license, author, and a clickable link to the project on GitHub.
- **Help** — press **`?`** any time for the full keybinding cheat sheet, neatly grouped by screen. It always reflects *your* current keys, so if you've remapped anything, the sheet shows your version, not the defaults.

To **close** any panel or step back a screen, press **`q`**. To **quit** the whole app, press **`Ctrl+Q`**. To jump back to the Player from anywhere, press **`Ctrl+H`**.

---

## Every key, explained

You don't need to memorize these — press `?` in the app for the live cheat sheet. But here's the full rundown for the curious. **Every single key below can be changed** to whatever you like (Settings → Hotkeys).

### On the Player

| Key | What it does |
| --- | --- |
| `Space` | Play / pause |
| `←` / `→` | Seek backward / forward a few seconds |
| `↑` / `↓` | Volume up / down |
| `n` / `p` | Next / previous song |
| `f` | Rate this song — cycles 🤔 neutral → 👍 like → 👎 dislike (a 👍 also adds it to Favorites; a 👎 means you'll hear less like it) |
| `s` | Shuffle on / off |
| `r` | Repeat: off → whole queue → one song |
| `e` | Cycle the equalizer preset |
| `N` | Loudness normalization on / off (evens out quiet and loud songs) |
| `>` / `<` | Speed up / slow down |
| `d` | Download this song |
| `y` | Copy the song's link to your clipboard |
| `v` | Pop open the **music video** in an mpv window |
| `V` | Change that video window's size / position |
| `/` | Open Search |
| `l` | Open your Library |
| `c` | Open the Queue |
| `L` | Show / hide lyrics |
| `a` | Open the AI assistant |
| `,` | Open Settings |

### In any list (Search, Library, Queue)

| Key | What it does |
| --- | --- |
| `↑` / `↓` | Move up / down |
| `PgUp` / `PgDn` | Jump a page at a time |
| `Home` / `End` | Jump to the top / bottom |
| `Enter` | Play the highlighted item now — your current queue is kept (in the Queue window, jump to it) |
| `\` | Add the highlighted item to the queue — Search / Library (or **right-click** a row) |
| `P` | Play the whole tab as a fresh queue (Library) |
| `f` | Favorite it (♥) |
| `d` | Download it |
| `Delete` | Remove it (from the queue or library) |
| `/` | Filter the list (Library), or jump back to the search box (Search) |
| `q` | Close the panel |

### Global (work almost everywhere)

| Key | What it does |
| --- | --- |
| `Ctrl+R` | Turn radio (autoplay) on / off |
| `w` | Show *why* the AI chose the upcoming songs |
| `A` | Turn all animations on / off |
| `?` | Show the full keybinding cheat sheet |
| `F1` | About ytm-tui |
| `Ctrl+H` | Jump back to the Player |
| `Ctrl+Q` | Quit the whole app |
| `q` | Go back / close the current panel |

> **Korean keyboard tip:** you don't have to switch to English input — the shortcuts understand 두벌식 jamo, so `ㅂ` works exactly like `q`, `ㄱ` like `r`, and so on. A small thing, but a nice one.

---

## Settings — every knob, one by one

Press **`,`** to open Settings. It's organized into **five tabs** along the top: **General · Playback · Hotkeys · Graphics · AI.**

**How to move around:**

- `Tab` / `Shift+Tab` — switch between tabs (or click them).
- `↑` / `↓` — move between settings in a tab.
- `←` / `→` — change the value of the highlighted setting (turn a toggle on/off, slide a slider, cycle through choices).
- `Enter` — flip a toggle, press a button, or start typing in a text field.
- `q` — close Settings. **It saves automatically** on the way out.

> **One thing to know:** a few settings are marked **"(next launch)"**. Those take effect only after you quit and reopen `ytt` — the app isn't ignoring you, it just can't change them mid-session.

<!-- 📸 screenshot: Settings screen — the five tabs and a list of fields -->

### General tab

The everyday housekeeping settings.

| Setting | What it does |
| --- | --- |
| **Language** | Switches the whole interface between **English** and **한국어 (Korean)**. Changes instantly. |
| **Cookies file** | Points to your `cookies.txt` if you want to sign in (see [cookies](#signing-in-with-cookies-optional)). Leave it blank to use the default location, or type a path. Most people never need this. |
| **Download dir** | The folder your downloads are saved to. Blank = the default (`~/Music/ytm-tui`). Type a path to send them elsewhere. |
| **Mouse** *(next launch)* | Turns mouse support on or off. On = you can click buttons, drag, and scroll. Off = pure keyboard. |
| **Album art** *(next launch)* | Shows the album cover as a real picture on the Player. Needs a terminal that can draw images (many can); if yours can't, leave it off. |
| **Autoplay on launch** | When on, `ytt` picks up where it left off and starts playing the moment it opens. |
| **Reset keybindings** | A button (press `Enter`). Restores **only** your keyboard shortcuts to their defaults — handy if you've remapped yourself into a corner. |
| **Reset all settings** | A button. Restores **everything** — keys, theme, AI key, all of it — to factory defaults. It asks you to confirm first, because there's no undo. |

### Playback tab

Two groups: **Now Playing** (how playback behaves) and **EQ** (how it sounds).

**Now Playing**

| Setting | What it does |
| --- | --- |
| **Playback speed** | A slider from `0.5x` (half speed) to `2.0x` (double speed). `1.0x` is normal. Great for slowing down a tricky guitar part or speeding through a long mix. |
| **Seek interval** | How many seconds the `←` / `→` keys jump each press — anywhere from 1 to 60 seconds. Set it to taste. |
| **Gapless** *(next launch)* | Removes the tiny silence between songs, so albums flow seamlessly into one another. |

**EQ** — a proper 10-band equalizer.

| Setting | What it does |
| --- | --- |
| **Preset** | Pick a ready-made sound: **Flat** (no change), **Bass**, **Treble**, **Vocal**, **Rock**, **Jazz**, or **Custom**. Cycling presets instantly reshapes the sound. |
| **The 10 bands** (`31 Hz` … `16 kHz`) | Ten sliders, one per frequency band, each adjustable from **−12 dB to +12 dB**. Nudge the low ones for more thump, the high ones for more sparkle. Touching any band switches the preset to **Custom** so your tweaks are remembered. |
| **Normalize (loudness)** | Evens out the volume between songs, so a whisper-quiet track and a wall-of-sound track play at roughly the same loudness. No more lunging for the volume key between songs. |

### Hotkeys tab

The full list of every keyboard shortcut, grouped by screen. Highlight any one, press `Enter`, then press the new key you want — done. If the key is already taken, the app warns you instead of silently breaking something. Press the reset key shown at the bottom to restore a single binding to its default. (Prefer the nuclear option? **General → Reset keybindings** resets them all.)

### Graphics tab

Everything about how `ytm-tui` looks. Three groups: **Theme**, **Colors**, and **Animations**.

**Theme** — the quick way to restyle the whole app.

| Setting | What it does |
| --- | --- |
| **Preset** | Choose a complete color scheme. Eleven to pick from: **Default, Midnight, Light, High Contrast, Terminal Green, Gruvbox, Nord, Dracula, Tokyo Night, Solarized Dark, Rosé Pine.** One of them is bound to match your soul (or at least your other terminal apps). |
| **Background: None** | Makes the app's background transparent, so your terminal's own background (wallpaper, blur, whatever) shows through. Lovely with a translucent terminal. |

**Colors** — for the perfectionists. Below the theme is a list of every individual interface element — the background, the various text shades, borders, the accent color, the selection highlight, the seek-bar fill, the player controls, and more. Highlight any one and type a **hex code** like `#ff8800` to recolor *just* that piece, or type `none` to make it transparent. This is how you build your own theme from scratch, one color at a time.

**Animations** — `ytm-tui` can be as lively or as still as you want. The top three switches are the controls; the rest are individual effects you can mix and match.

| Setting | What it does |
| --- | --- |
| **Enable animations** | The master switch. Off = a completely still, calm interface. (Same as the `✨` in the top bar, or the `A` key.) |
| **Frame rate** | How smooth the motion is, from 5 to 60 fps. Lower it to be kinder to your laptop battery; raise it for buttery smoothness. |
| **Pause when unfocused** | Stops all animation while the terminal window isn't the one you're using — saves power when ytm-tui is in the background. |

…and the effects themselves, each toggled on or off independently:

| Effect | What you'll see |
| --- | --- |
| **Title shimmer** | The now-playing title gently shimmers / scrolls |
| **Beating heart** | The `♥` favorite icon pulses like a heartbeat |
| **Seekbar glow** | A little comet of light sweeps along the progress bar |
| **Now-playing spinner** | A small spinner turns while music plays |
| **EQ bars** | Animated VU bars dance next to the `eq:` label |
| **Control pulse** | The play controls softly pulse |
| **Breathing border** | The window's border gently brightens and dims |
| **Matrix rain** | Falling green characters in the empty space |
| **Spinning donut** | The famous spinning ASCII donut |
| **Visualizer** | A music-style bar visualizer |
| **Starfield / notes** | Drifting stars and music notes |
| **Bouncing logo** | A logo that bounces around the screen, DVD-screensaver style |

### AI tab

The optional AI assistant and the radio that powers endless playback.

| Setting | What it does |
| --- | --- |
| **Enable AI** | The master switch for the assistant. Turn it off and the assistant goes quiet — but your saved key stays put, so you can switch it back on anytime without re-entering anything. |
| **Model** | Which Google Gemini model answers you: **Flash**, **Flash Lite** (the default — fastest and cheapest), or **Latest**. If you're not sure, leave it on Flash Lite. |
| **API key** | Paste your free Google Gemini key here (it's hidden once saved). This is the one thing the assistant *needs*. See [the AI assistant](#the-ai-assistant). |
| **Autoplay radio** | Turns the never-ending radio on (same as pressing `Ctrl+R`). When your queue runs low, it lines up more songs that fit the mood. |
| **Radio mode** | How adventurous the radio is: **Focused** (very close to what you're hearing), **Balanced** (the sensible default), or **Discovery** (wanders off to find new things). |

---

## Radio: music that never stops

Press **`Ctrl+R`** and `ytm-tui` becomes a little radio station built around whatever you're currently playing. As your queue starts to run dry, it quietly lines up more songs that fit the mood — so the music simply never stops. It's the lazy listener's dream, and we mean that as the highest compliment.

You choose how adventurous it is, in **Settings → AI → Radio mode** (or by clicking the `radio:` label on the Player):

- **Focused** — sticks close to home. More of what you're already enjoying.
- **Balanced** — a sensible middle ground. The safe default.
- **Discovery** — wanders off the beaten path to surface things you haven't heard.

*How* it chooses the next song is a quietly-guarded secret — mostly because the honest answer is "a great deal of trial and error, and some hard-won pride." It just works. If you're ever curious about a specific choice, press **`w`** for the plain-language reasoning. Otherwise, just enjoy it.

---

## Downloads: keeping music offline

Press **`d`** on any song and it's saved to your disk so you can play it even with no internet. By default, downloads land here:

- **macOS:** `~/Music/ytm-tui`
- **Windows:** `%USERPROFILE%\Music\ytm-tui`
- **Linux:** a `ytm-tui` folder inside your Music folder

Everything goes into that one folder — no fiddly per-artist subfolders to dig through. Downloaded songs then appear automatically in your **Library**, under both **All** and **Downloads**, sitting right next to your streamed music.

Want them somewhere else? Change **Download dir** in Settings, or set the `YTM_DOWNLOAD_DIR` environment variable.

---

## Remote control: drive it from anywhere

Once `ytt` is up and playing, you can boss it around from *another* terminal — or, much nicer, straight from your keyboard's media keys — without ever clicking back into its window. The magic prefix is **`ytt -r`** (`-r` for *remote*):

```sh
ytt -r pp          # play / pause
ytt -r next        # skip to the next song
ytt -r radio on    # switch the endless radio on
ytt -r status      # what's playing right now?
ytt -r quit        # stop the music and close ytt
```

Each one quietly connects to the `ytt` you already have open, does the single thing you asked, prints a one-line "here's what happened," and gets out of the way. The music screen doesn't so much as flicker.

**The full vocabulary** (every command has a short alias or two):

| Type this | …and ytt will | Short for |
| --- | --- | --- |
| `ytt -r next` | Skip to the next song | `n` |
| `ytt -r prev` | Back to the previous song | `p` |
| `ytt -r play-pause` | Toggle play / pause | `pp`, `toggle` |
| `ytt -r up` / `ytt -r down` | Nudge the volume up / down | `vol-up` / `vol-down` |
| `ytt -r back` / `ytt -r fwd` | Seek backward / forward | `rewind` / `forward` |
| `ytt -r radio [on\|off\|toggle]` | Turn the radio on, off, or flip it | — |
| `ytt -r status` | Print a one-line "now playing" summary | `st` |
| `ytt -r quit` | Stop playback and close the app | `exit` |

`ytt -r status` answers with one tidy line, something like:

```
[playing] Bohemian Rhapsody — Queen  •  vol 80%  •  3/12  •  radio on
```

Two flags worth knowing: add **`-q`** to silence the success line (handy for media-key bindings that shouldn't chatter), and **`--json`** if you'd rather have a machine-readable blob for a status bar. `ytt -r --help` lists the lot.

### Wiring it to your media keys

This is the fun part. Teach those tiny `⏯ ⏭ ⏮` keys once and you'll never alt-tab to the player again. On **i3** or **sway**, drop this into your config:

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
bindsym XF86AudioPrev exec ytt -r prev
```

Anything that can run a command on a keypress works the same way — Hyprland's `bindl`, GNOME's custom shortcuts, or on macOS a tool like [skhd](https://github.com/koekeishiya/skhd) or Karabiner. Just point the key at `ytt -r <command>`.

### Only one player at a time

A small, pleasant side effect: once `ytt` is running, typing `ytt` again **won't** launch a second player to fight over your speakers. It bows out politely instead:

```
ytt is already running.
  Control it:  ytt -r <command>   (e.g. `ytt -r pp`, `ytt -r next`)
  Stop it:     ytt -r quit
  New player:  ytt --new-instance
```

So a stray second launch just reminds you how to reach the one you've already got. And if you *really* do want a second, fully independent player, `ytt --new-instance` is the escape hatch.

### For the scripters

`ytt -r` follows the usual exit-code manners, so it behaves inside scripts and key bindings:

- **`0`** — done, all good.
- **`1`** — couldn't reach a running `ytt` (is one actually open?).
- **`2`** — the command didn't make sense or didn't apply (a typo'd verb, or `next` on an empty queue).

---

## Signing in with cookies (optional)

Short version: **you almost certainly don't need this.** Without signing in, `ytm-tui` searches and plays public songs perfectly well, anonymously.

You'd only sign in to reach songs that *require* a logged-in YouTube Music account — members-only uploads, certain region-locked tracks, that sort of thing. To do it, you hand the app your browser's "cookies": a small file that proves you're you.

By default it looks for `cookies.txt` here:

- **macOS:** `~/Music/ytm-tui/cookies.txt`
- **Windows:** `%USERPROFILE%\Music\ytm-tui\cookies.txt`

Create that folder, export your YouTube / YouTube Music cookies from your browser in **Netscape format** as `cookies.txt` (a browser extension can do this for you), drop the file in, and restart `ytt`.

```sh
# macOS / Linux — make the folder
mkdir -p ~/Music/ytm-tui
```

```powershell
# Windows (PowerShell) — make the folder
New-Item -ItemType Directory "$HOME\Music\ytm-tui" -Force
```

You can also point to a different file in **Settings → General → Cookies file**.

> ⚠️ **Treat this file like a password.** It can act like a logged-in browser session, so don't share it, don't commit it to a repository, and don't post it anywhere.

---

## Where your settings are kept

You never have to touch these — everything is editable inside the app — but for the curious, `ytm-tui` keeps its settings in a single `config.json`:

- **macOS:** `~/Library/Application Support/ytm-tui/config.json`
- **Linux:** `~/.config/ytm-tui/config.json`
- **Windows:** `%APPDATA%\ytm-tui\config.json`

Two environment variables can override settings at launch, handy for scripts or trying things out:

- `GEMINI_API_KEY` — your AI key (wins over whatever's saved in Settings).
- `YTM_DOWNLOAD_DIR` — where downloads go (wins over the Download dir setting).

---

## When things go wrong

Deep breath. It's almost always one of these:

| Symptom | The fix |
| --- | --- |
| **Nothing plays, or it errors the instant you press play** | mpv or yt-dlp probably isn't installed, or isn't on your `PATH`. Re-do [Step 1](#step-1--the-two-helpers-mpv--yt-dlp). |
| **`ytt: command not found`** | Open a brand-new terminal window. If it persists, the install folder isn't on your `PATH` — the installer printed the exact line to add; paste it into your shell's config file. |
| **A specific song won't play** | Some tracks need you signed in. See [Signing in with cookies](#signing-in-with-cookies-optional). |
| **It worked yesterday, now nothing plays** | YouTube changes things constantly, which breaks yt-dlp. Update it: `brew upgrade yt-dlp` (Mac), `scoop update yt-dlp` (Windows), or your package manager (Linux). |
| **No album art** | It's off by default and only works in some terminals. Turn on **Album art** in Settings → General, then **restart** (it's a "next launch" setting). |
| **The AI assistant won't respond** | It needs a free Gemini key in Settings → AI, and **Enable AI** switched on. |
| **I remapped a key and now everything's chaos** | Settings → General → **Reset keybindings**. All is forgiven. |
| **Animations are eating my battery** | Lower the **Frame rate**, turn on **Pause when unfocused**, or just press `A` to switch them all off. |

Still stuck? Open an issue on the [GitHub repo](https://github.com/Ochichan/ytm-tui) and describe what happened. Bonus points for mentioning your operating system.

---

## Special thanks

🙏 A huge, heartfelt shout-out to **[@ZZNN75](https://github.com/ZZNN75)** — who put in real hours on QA: poking at every corner, breaking things on purpose, and patiently reporting each weird little edge case so *you* never have to stumble into them. A lot of the rough spots you **won't** hit are smooth specifically because they hit them first and said something. Absolute legend. 🫡

If `ytm-tui` feels solid, a meaningful chunk of that credit belongs to them.

---

## License

MIT. Do whatever you want with it. Seriously — fork it, ship it, frame it, ignore it. It's yours.
