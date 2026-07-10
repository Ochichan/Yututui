# The YuTuTui! Manual

**English** · [한국어](MANUAL.ko.md) · [日本語](MANUAL.ja.md)

This is the friendly, take-your-time guide to YuTuTui!. It's written for people who don't live in a terminal — no jargon, every step spelled out. (If you *do* live in a terminal, the [README](README.md) has the fast version.)

One thing before anything else: **you can always press `?` inside the app.** It opens a cheat sheet of every key, and it always matches *your* settings. If you remember one thing from this manual, remember `?`.

---

## 1. First steps

### Install and open it

Follow the one-line install for your computer in the [README](README.md#install). Then open your terminal app — that's the window where you type commands:

- **macOS** — the app called *Terminal* (or iTerm2 if you have it)
- **Windows** — *Windows Terminal* from the Start menu
- **Linux** — you know which one you like

Type this and press Enter:

```sh
ytt
```

That's the whole launch. The player appears in the window.

### Play your first song

1. Press **`s`** — a search box opens.
2. Type a song or artist name, press **`Enter`**.
3. Move down the results with **`↓`**, press **`Enter`** on the one you want.

Music. If instead you got an error, type `ytt doctor` in the terminal — it checks your setup and tells you, in plain words, what to fix.

### The five screens

YuTuTui! is five screens, each one key away:

| Key | Screen | What it's for |
| --- | --- | --- |
| — | **Player** | The now-playing screen: album art, lyrics, progress bar |
| `s` | **Search** | Find songs, albums, artists, stations |
| `l` | **Library** | Your favorites, history, downloads and playlists |
| `o` | **Settings** | Everything adjustable, including accounts |
| `g` | **DJ Gem** | Ask for music in plain words *(optional, see below)* |

`Esc` generally backs you out of wherever you are. The mouse works everywhere too — click anything, scroll the wheel to change volume.

---

## 2. Everyday music (the normal mode)

### The keys you'll actually use

| Key | Does |
| --- | --- |
| `Space` | Play / pause |
| `,` / `.` | Previous / next song |
| `←` / `→` | Rewind / fast-forward |
| `↑` / `↓` | Volume |
| `f` | ♥ Favorite the current song |
| `c` | Show the queue (what plays next) |
| `Shift+L` | Lyrics, synced to the music |
| `v` | Music video in a floating window |
| `Ctrl+Q` | Quit |

### Your Library

Press **`l`**. The Library has five tabs: **All**, **Favorites**, **History**, **Downloads**, and **Playlists**. Everything you favorite, play, download or collect ends up in one of them. Press **`n`** to start a new playlist of your own.

### Downloads — keep songs offline

On any song, press **`d`**: it's saved as a proper music file (cover art and title included) into your Music folder, and appears under Library → Downloads. **`Shift+D`** downloads a whole list or playlist at once. Downloaded songs play without internet — and they feed the Local Deck (chapter 4).

### DJ Gem *(optional)*

DJ Gem is the app's built-in music brain. It's optional and needs a free Gemini API key from Google — everything else in the app works without it.

- **Endless station:** press **`Ctrl+R`** and it keeps the queue filled with songs that fit what you're hearing. Press **`w`** and it explains, in plain language, why it picked each one.
- **Ask in words:** press **`g`** and type things like *"play some quiet piano"* or *"make me a rainy-day playlist"* — it can build the playlist right into your Library.

To switch it on: get a free Gemini API key from Google, then paste it in **Settings → DJ Gem** and enable it.

---

## 3. Radio mode — the app becomes a radio tuner

Sometimes you don't want to pick songs. Radio mode turns the whole app into an internet-radio tuner with thousands of real, live stations.

### In and out

On the Player screen, press **`Alt+Shift+R`**. The app asks *"Switch to dedicated Radio mode?"* — confirm, and everything changes: the colors switch to a radio-only theme (that's on purpose — it's how you know where you are), and your music queue is safely tucked away, exactly as it was, until you come back.

Press **`Alt+Shift+R`** again to return to normal music mode. (One rule: Radio and the Local Deck can't be open at the same time — leave one before entering the other.)

### Finding a station

Press **`s`** and search, just like for songs — except now you're searching **Radio Browser**, a huge public directory of internet stations. Search for a genre ("jazz"), a country, or a station name, and press `Enter` to tune in.

Your Library (press `l`) also changes in radio mode: it shows just two tabs, **Radio Likes** and **Radio History** — your favorite stations and recently tuned ones. They're kept completely separate from your music favorites. Press **`f`** on a station to like it.

### While you listen

Live radio is *live*, so there's no rewinding. If your connection hiccups and you drift behind the broadcast, the app tells you — *"Live: 25s behind"* — and pressing **`r`** snaps you back to the live edge.

The best part: **press `i`** when a song catches your ear. A little card pops up telling you *what's playing right now*, using the station's own broadcast info. Inside that card:

- **`f`** saves the identified song to your *music* favorites (so you'll find it back in normal mode),
- **`g`** asks DJ Gem to tell you more or find similar songs.

There's also a recordings browser on **`Alt+Shift+E`**.

---

## 4. Local Deck — your own music, beautifully

The Local Deck is a dedicated player for music that lives *on your computer*: your downloads and your own audio files. No internet needed at all.

### In and out

Open the Library (**`l`**), then press **`Alt+Shift+L`**. The app asks *"Switch to Local Player mode?"* — confirm, and you're in an immersive shell built just for browsing local music. Press **`Alt+Shift+L`** again to leave.

### What you'll see

The Local Deck scans your download folder (it understands the *Artist / Album / track* layout) and organizes everything into sections. Press the **number keys** to jump between them:

**Home · Tracks · Albums · Artists · Genres · Folders · Smart Lists · Scan Errors · Import Sessions · Inbox**

- **Tracks / Albums / Artists / Genres** — your collection, sliced every way.
- **Folders** — browse exactly as the files sit on disk.
- **Smart Lists** — automatic collections.
- **Scan Errors** — files the scanner couldn't read, so nothing fails silently.
- **Import Sessions / Inbox** — where Spotify imports arrive for review (next chapter!).

### Getting music in

- Every song you download with `d` / `Shift+D` shows up here automatically.
- You can add more folders to scan in Settings (Local Deck roots).
- Spotify imports can download straight into it — read on.

---

## 5. Moving in from Spotify — the full, gentle walkthrough

You can bring your Spotify playlists and liked songs into YuTuTui!. Nothing is guessed silently: every song is matched by its actual title, artist and album, and anything uncertain is set aside for *you* to decide.

**Where can the music go?** Two options:

1. **Into the app's own Library playlists** — works immediately, no YouTube account needed. This is what the in-app import does.
2. **Into your real YouTube Music account** (playlists or likes) — the command-line way, needs your YouTube sign-in cookies (see the README reference).

### 5a. One-time setup (~5 minutes, free)

Here's the honest reason this setup exists: Spotify only lets apps read your library if the app is registered with them, and their registration for personal apps ("Development Mode") only serves people the app owner explicitly lists. So instead of everyone sharing one app, *you create your own tiny free one* — it takes five minutes, costs nothing, and only you will ever use it. There is no secret password involved; you'll only copy one ID.

1. Go to **[developer.spotify.com/dashboard](https://developer.spotify.com/dashboard)** in your browser and log in with your normal Spotify account.
2. Click **Create app**.
3. **App name** and **App description** can be anything — `yututui` is fine.
4. In **Redirect URIs**, type exactly:

   ```
   http://127.0.0.1:9271/callback
   ```

   and click **Add**. This must be letter-for-letter exact — the numbers `127.0.0.1`, not the word `localhost` (Spotify refuses that), and no extra slash at the end. This address just means "come back to the app on this computer" — nothing leaves your machine.
5. Where it asks **Which API/SDKs are you planning to use?**, tick **Web API**.
6. Accept the terms, click **Save**.
7. Open your new app → **Settings**, and copy the **Client ID** (a long string of letters and numbers). Ignore the "Client secret" — you don't need it.
8. Still in the app's settings, open **User Management** and add *yourself*: your name and the email of your Spotify account. This is the allowlist — without this step Spotify will answer "403" later.

Done. You never have to do this again.

### 5b. Connect YuTuTui! to Spotify

In the app: **Settings (`o`) → Accounts → Spotify** → paste the Client ID → choose **Connect**. Your browser opens a Spotify page asking to approve — click approve, and the app says connected.

(Prefer typing? `ytt auth spotify --client-id <YOUR-ID>` does the same. On a machine with no browser, the approval link is copied to your clipboard and saved to `spotify_auth_url.txt` — open it on any device.)

### 5c. Import, inside the app

1. Go to **Settings → Accounts → "Import from Spotify…"**.
2. A picker opens with your Spotify playlists. Choose one.
3. Pick an **import mode** (there's a dropdown right there):

   | Mode | What it means |
   | --- | --- |
   | **Fast playlist** | Take confident matches *and* safe near-matches. Most songs land immediately. |
   | **Strict playlist** | Only take matches the app is sure about; everything else waits for your review. |
   | **Review first** | Match everything but write *nothing* yet — you approve it all later. |

4. That's it — the import runs **in the background while your music keeps playing**. The status line shows the progress.

When it finishes, you'll see: *"Import finished … saved in Library → Playlists"*. The playlist is there, playable right away.

### 5d. Reviewing the leftovers

Songs the app wasn't sure about are never guessed — they wait in the **Local Deck → Import Sessions** (and its **Inbox**). Go there (chapter 4), open the session, and go through the rows: each shows what Spotify had and what the best candidates are. Accept the ones that look right — or press **`A`** to accept all matched candidates at once. Rows can also retry their downloads or open candidate links so you can check with your own ears.

### 5e. The command-line version *(optional)*

If you're comfortable typing commands, the same machinery is available with more options:

```sh
ytt transfer import <spotify-url-or-id>      # playlist → your YTM account (needs cookies)
ytt transfer import liked --to likes         # Spotify likes → YTM likes, order kept
ytt transfer import <url> --to local:Name    # → the app's own Library playlist (no YTM account)
ytt transfer resume <job-id>                 # pick up after an interruption
ytt transfer backup --dir ~/music-backup     # back up every playlist to files
ytt transfer session <id>                    # inspect an import session
```

Imports are checkpointed: a rate limit, a closed lid or a power cut just means `resume` later — it continues where it stopped.

### If something goes wrong

The most common hiccups (403 "not allowlisted", INVALID_CLIENT, a busy port) all have one-line fixes in the **[README's troubleshooting section](README.md#troubleshooting)**.

---

## 6. When something goes wrong (anywhere)

First, always: quit the app and run

```sh
ytt doctor
```

It checks all the helper programs and tells you exactly what's missing and how to get it. For everything else — songs that won't play, missing album art, scrobbles, Spotify errors — the **[README troubleshooting tables](README.md#troubleshooting)** cover the known cases, sorted by symptom.

Still stuck? [Open an issue](https://github.com/Ochichan/Yututui/issues) and just describe what you saw — mention your operating system.

---

*Happy listening! — and remember: `?`*
