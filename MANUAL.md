# The YuTuTui! manual

English · [한국어](MANUAL.ko.md) · [日本語](MANUAL.ja.md)

Press `?` anywhere in the app to open the key guide. It uses your current keybindings.
The [README](README.md) has a shorter command reference.

[DJ Gem and Momoring](#dj-gem) · [Atlas globe](#atlas-mode) · [All chapters](#contents)

<a id="contents"></a>

<details>
<summary>Contents</summary>

1. [First steps](#chapter-1)
2. [Everyday music](#chapter-2)
3. [Radio mode](#chapter-3)
4. [Local Deck](#chapter-4)
5. [Moving from Spotify](#chapter-5)
6. [Backing up personal data](#chapter-6)
7. [Syncing computers](#chapter-7)
8. [Playing from a music server](#chapter-8)
9. [Troubleshooting](#chapter-9)

</details>

---

<a id="chapter-1"></a>

## 1. First steps

### Install and open YuTuTui!

Follow the instructions for your system in the [README](README.md#install). On Windows, choose
**YuTuTui!** from the Start Menu. This opens Windows Terminal and starts the player. The tray
icon's **Open Player** action does the same.

On macOS or Linux, open a terminal. Windows users can also open Windows Terminal and run:

```sh
ytt
```

For ten seconds after the first launch, the app points to Search. Press the displayed Search key,
normally `s`, or click **Search**. If mpv, yt-dlp, or ffmpeg is missing, use the setup card's copy
or guide buttons. Install the missing program, then choose **Check again**. Run `ytt doctor` for
the full setup report.

<details>
<summary>Terminal closing and background playback</summary>

On POSIX systems, guarded playback requires mpv 0.33 or newer. The interactive `ytt` process exits
when it confirms that its terminal or multiplexer is gone. Retained Windows ConPTY or tmux-control
brokers can look like attached clients, as can repeated nesting of tmux, Screen, or Zellij. Use
`ytt daemon` or a host-side lifetime supervisor or lease in those cases. See
[terminal compatibility](docs/terminal-compatibility.md#terminal-lifetime-detection) for the
supported cases.

</details>

### Play a song

1. Press `s` to open Search.
2. Type a song or artist, then press `Enter`.
3. Use `↓` to select a result and press `Enter` to play it.

Text fields support these editing keys:

| Key | Action |
| --- | --- |
| `←` / `→` | Move one character |
| `Ctrl+←` / `Ctrl+→` | Move one word |
| `Backspace` | Delete one character |
| `Ctrl+Backspace` | Delete the previous word |

Some older or multiplexed terminals send the same code for `Ctrl+Backspace` and `Ctrl+H`. While
the Delete Word binding is at its default, YuTuTui! treats that code as word deletion inside text
fields and ignores it elsewhere. See
[keyboard input modes](docs/terminal-compatibility.md#keyboard-input-modes).

In Search, `Ctrl+P` cycles among songs, YouTube playlists, and artists. A playlist row can play,
queue with `\`, or import with `p` the entire playlist. An artist row opens the artist page, which
lists top songs, albums, and singles. Press `Enter` to play and `Esc` to return to the results.

### Beginner mode and language

Open **Settings → General** and enable **Beginner Mode** to start a nine-step walkthrough on the
next launch. Its first card offers **English**, **한국어**, and **日本語**. Moving between the choices
previews the card in that language. Press `Enter` to apply the choice to the walkthrough and the
rest of the app. You can later change it under **Settings → General → Language**.

In retro mode, the language card explains that the interface remains in English and offers
Continue or Skip.

### The five screens

| Default key | Screen | Purpose |
| --- | --- | --- |
| None | **Player** | Now playing, album art, lyrics, and progress |
| `s` | **Search** | Songs, albums, artists, and stations |
| `l` | **Library** | Favorites, history, downloads, and playlists |
| `o` | **Settings** | Playback, accounts, appearance, and other options |
| `g` | **DJ Gem** | Optional natural-language music requests |

These defaults apply outside text fields. Radio mode and Local Deck replace the normal Player.
Atlas is a globe view within Radio mode and has its own controls. `Esc` usually moves back one
level. Mouse scrolling can move a list, change volume, or zoom Atlas, depending on the pointer's
location.

### The player bar

The player bar remains at the bottom of every screen. It shows the title, progress, transport
controls, and status, so you can pause or seek while viewing Search or Library. On screens other
than Player, press `Shift+B` or click the `▼` or `▲` by the footer mouse hint to collapse or restore
the bar. The Player screen always shows it.

To keep the controls only at the top of Player, set **Settings → General → Player bar position**
to **Top**.

Below about 32 by 14 terminal cells, YuTuTui! switches to a small layout with the title, progress,
and transport controls. The full layout returns when the window grows.

---

<a id="chapter-2"></a>

## 2. Everyday music

### Playback keys

| Key | Action |
| --- | --- |
| `Space` | Play or pause |
| `,` / `.` | Previous or next song |
| `←` / `→` | Rewind or fast-forward |
| `↑` / `↓` | Change volume |
| `f` | Cycle the current song through like, dislike, and unrated |
| `x` / `r` | Toggle shuffle or cycle repeat |
| `c` | Show the queue |
| `Shift+L` | Show synchronized lyrics |
| `z` / `Shift+Z` | Show lyrics 0.1 seconds earlier or later |
| `v` | Open the music video in a floating window |
| `!` / `@` | Previous or next chapter in a long mix or podcast |
| `Shift+S` | Set the sleep timer in minutes, or type `off` |
| `w` | Explain the selected queue recommendation or current track |
| `Ctrl+Q` | Quit |

When synchronized lyrics load, `[ − 0.0s + ]` appears at the lower right for three seconds. It
then folds into `[±]`. Click that handle to reopen the control for three seconds. Use `−` or `+`
to change the timing by 0.1 seconds. Clicking a visible lyric line seeks to its timestamp.

Open **Settings → Playback → Audio output** to select an output that YuTuTui! detected. **Audio
backend** exposes the underlying mpv audio options.

Right-click a row to open its context menu. You can remap mouse gestures through `mouse_bindings`
in `config.json`.

### Library and downloads

Press `l` to open the Library. Its five tabs are **All**, **Favorites**, **History**, **Downloads**,
and **Playlists**. Press `n` to create a playlist.

Press `d` on a song to save it as a music file with its title and cover art in your Music folder.
It then appears under **Library → Downloads**. Press `Shift+D` to download a list or playlist.
Downloaded tracks play without an internet connection and also appear in Local Deck.

<a id="listening-records"></a>

### Listening records

Listening records are disabled on a new device. Press `Ctrl+B` to open them. The first opening
asks whether to enable recording locally. Update every paired client to a version that supports
the new records before confirming. Older clients stop syncing when they encounter the new data
format. To enable the feature on a remote owner, run:

```sh
ytt -r listening enable
```

Enabling the feature saves the local configuration. The first change to a listening record
upgrades the personal ledger to schema 3. Playback while the feature is disabled neither enables
recording nor upgrades the ledger.

The records view has three tabs: bookmarks and resume points, DJ presets, and the listening
passport. A bookmarks button also appears in the player footer when there is room. Use `Tab` to
change tabs, the arrow keys to select a row, and `Enter` to open it. The displayed shortcut labels
follow your remapped keys.

The common record actions are:

| Key | Action |
| --- | --- |
| `n` | Add a bookmark or save a preset |
| `e` | Edit the selected record |
| `Delete` | Confirm removal |
| `i` | Open complete, read-only details |
| `↑` / `↓` | Scroll the detail view |
| `Esc` | Close the detail view |

#### Manual bookmarks and automatic resume

Press `n` while any finite, seekable track is playing to save its current position under a label.
This works for tracks shorter than 20 minutes, and a track may have several labeled positions.
Opening a bookmark for another track loads the exact saved source and seeks after it is ready.
Live radio does not accept time bookmarks.

Automatic resume requires confirmed seekability and a duration of at least 20 minutes. YuTuTui!
starts saving useful positions at 30 seconds. It saves confirmed progress when you pause, leave a
track, or shut down normally. Periodic progress writes occur at most once every 60 seconds.
Reaching the final 60 seconds or finishing naturally clears the automatic point but leaves manual
bookmarks intact.

Deliberately reopening a track or restoring a session uses the automatic resume point.
Recommendation transitions and repeats start the track at zero. An explicit bookmark takes
precedence over automatic resume. Press `a` to toggle automatic resume. Press `r` in this view to
restart the currently playing track.

#### DJ presets

Prepare recommendation preferences from the normal music Player:

1. If repeat is on, press `r` until it is off. Then enable streaming with `Ctrl+R`.
2. Press `e` to open the station preferences.
3. Type `jazz` and press `Enter` to add a more-like term. Type `-rock` and press `Enter` to add an
   exclusion. Press `Enter` with empty input to add the current artist.
4. Select an entry and press `Delete` to remove it.
5. Press `Alt+Shift+P` to open saved DJ presets.

A preset stores at most 12 more-like and exclusion terms, plus excluded tracks and artists. It
does not contain the station query or explore state, DJ Gem chat, API or model settings, the
queue, or audio settings.

Press `n` to save the current preferences under a name. `Enter` replaces the active recommendation
preferences with the selected preset as one operation. Playback, the queue, and playback modes
remain unchanged, and YuTuTui! discards recommendations that were pending for the previous
preferences. Press `e` to edit a preset. Press `s` and confirm to replace its saved snapshot with
the current preferences. Changing the active preferences never changes the saved preset until you
explicitly save it.

#### Listening passport

In Atlas, press `Shift+P` to open the listening passport. A station visit qualifies after 30
seconds of observed, active playback. Paused and buffering time does not count. YuTuTui! marks a
country only when the station supplies a known country code. It does not infer missing locations.

Select a station and press `e` to edit its note. Press `Delete` to remove one station. Press
`Shift+C` and confirm to clear the passport.

#### Remote listening commands

The remote commands require the main app or daemon to be running. `list` returns at most 256 rows
and supplies the identifiers used by the other commands.

```sh
ytt -r listening enable
ytt -r listening list
ytt -r listening bookmark-add <label>
ytt -r listening bookmark-jump <bookmark-id>
ytt -r listening bookmark-delete <bookmark-id>
ytt -r listening restart
ytt -r listening preset-save <name>
ytt -r listening preset-load <preset-id>
ytt -r listening preset-delete <preset-id>
```

#### Sync and local-file limits

Encrypted sync and personal-data export or import include listening records. Syncing does not seek
the current playback position or apply a preset. If devices save different resume positions, both
choices remain explicit. Concurrent notes, presets, and bookmarks remain visible. For presets,
press `i` to inspect the saved contents, then edit the version you want to keep.
Offline edits remain in the ledger and merge during the next sync.

YuTuTui! does not upload local audio files. A synced record can remain on a device that cannot play
its source. Local track identity belongs to the device and does not use title matching. Moving a
file can therefore require a new bookmark.

<a id="dj-gem"></a>

### DJ Gem

DJ Gem is optional. Its chat requires a Gemini API key. Search, playback, radio, Atlas, and
fallback station recommendations work without that key.

1. Open **Settings → DJ Gem**, enter the API key, and enable **DJ Gem chat**. You can instead set
   `GEMINI_API_KEY` before starting `ytt`; the environment value takes precedence.
2. On Player, press `g`. Enter a request such as `play some quiet piano` or
   `make me a rainy-day playlist`, then press `Enter`. DJ Gem can create a Library playlist.
3. Choose a Gemini model under **Settings → DJ Gem**, or click the model name at the bottom of the
   chat screen.

In normal music mode, `Ctrl+R` toggles a continuous station around the current track. Fallback
recommendations can keep it running without a Gemini key. Streaming and repeat are mutually
exclusive. If repeat is active, YuTuTui! rejects the streaming change and shows a toast. Press `r`
to turn repeat off, then enable streaming.

Recommended tracks show a clickable `?` in the queue and beside Now Playing. With the queue open,
press `w` to explain the selected row. Otherwise, `w` explains the current track. The explanation
always identifies the recommendation source. If DJ Gem supplied model details, it also shows the
track's role, reasons, and optional confidence. Other recommendations show their source only.

The empty DJ Gem screen shows Momoring as a Braille character beside the setup text when the
terminal has enough room. She animates only while a queued track is playing and animations are
enabled. She remains still during paused playback or when animations are disabled. She disappears
after the conversation has messages and in small windows. Momoring needs neither an API key nor a
terminal image protocol. Press `A` on Player to toggle animations.

[Watch the 3-second Momoring close-up](docs/media/dj-gem-momoring.gif).

---

<a id="chapter-3"></a>

## 3. Radio mode

### Enter and leave Radio mode

On Player, press `Alt+Shift+R` and confirm **Switch to dedicated Radio mode?** The app saves the
music queue, changes to the Radio theme, and opens the radio player. Press `Alt+Shift+R` again to
restore normal music mode and its queue. Radio mode and Local Deck cannot be active together, so
leave one before entering the other.

### Find and save stations

Press `s` in Radio mode to search the Radio Browser directory. You can search by station name,
country, or genre. Select a result and press `Enter` to tune in.

The Radio Library, opened with `l`, has **Radio Likes** and **Radio History**. These are separate
from music favorites and music history. Press `f` on a station to add or remove its radio favorite.

Live radio cannot rewind. If playback falls behind the broadcast, the app reports the delay, such
as `Live: 25s behind`. Press `r` to return to the live edge.

Press `i` to open the station metadata card for the current broadcast. It uses the station's
metadata and does not send a Gemini request. If the station supplies no song metadata, the card
reports that instead of trying to identify the audio. In the card, `f` saves the identified song
to normal music favorites. `g` asks DJ Gem for more information or related songs and requires DJ
Gem chat setup.

Press `Alt+Shift+E` to open the recordings browser.

<a id="atlas-mode"></a>

### Atlas mode

Enter Radio mode, then press `a` on the Radio Player or click **Atlas globe** below the radio
artwork. Atlas renders the globe with Braille or ASCII characters and needs no image protocol. It
is unavailable in the small player layout.

[Watch the 27-second Atlas recording](docs/media/atlas.mp4) · [Animated preview](docs/media/atlas.gif)

Drag the globe to rotate it. If animations and coasting are enabled, a flick keeps it moving.
Scroll to zoom. Click a marker to tune to a station, or click a country to browse its stations.
Nearby stations may share a marker at the current zoom level. Zoom in or use the station list to
choose among them.

The side panel contains **World**, **Favorites**, and **Recent**. `Tab` moves focus between the
globe and panel. With panel focus, `↑` and `↓` select a row, while `←` and `→` change tabs. If the
panel is hidden, `Tab` reveals it when the terminal is wide enough.

| Key | Action in Atlas |
| --- | --- |
| Arrows or `h j k l` | Rotate with globe focus, or navigate with panel focus |
| `Shift` plus arrows | Rotate in larger steps with globe focus |
| `+` / `-` | Zoom in or out; `=` also zooms in |
| `0` | Reset zoom, leave country view, and center on the playing station when available |
| `n` / `p` | Select the next or previous visible signal |
| `Enter` | Open the selected signal or panel row |
| `c` | Browse the country under the globe cursor |
| `r` | Tune a random station, favoring stations not heard recently |
| `g` | Return to the station playing from Atlas |
| `f` | Toggle the selected station's radio favorite |
| `/` | Search by name, country, language, or tag; `Enter` fetches results |
| `Tab` / `Shift+Tab` | Move between globe and panel focus |
| `G` / `R` | Toggle the grid or autorotation; rotation also requires animations |
| `Shift+P` | Open the [listening passport](#listening-records) |
| `PageUp` / `PageDown` | Raise or lower the volume |
| `Space` / `m` / `,` / `.` | Pause, mute, previous, or next |
| `q` / `a` | Close Atlas when search text is not being edited |
| `Esc` | Leave search editing or clear search, then clear the signal, leave country view, or close |

Keys can mean different things in Atlas. Here, `g` returns to the playing station and `r` chooses
a random station. Close Atlas to use `g` for DJ Gem or `r` for the live edge. Return to Player to
toggle animations with `A`.

Radio mode adds Atlas options under **Settings → Playback**. They control the renderer, station
limit, side panel, coasting, grid, follow-playing behavior, and autorotation. The world catalog
loads up to 2,000 stations by default. You can set the limit from 500 to 5,000. Automatic
rendering uses Braille, except that retro mode uses ASCII. Select ASCII yourself if your font
does not contain Braille glyphs.

Radio Browser listings are cached for 24 hours. Stations without coordinates receive an
approximate position within their country. The cache does not make broadcasts available offline.
Live playback and fresh searches still need a network connection.

---

<a id="chapter-4"></a>

## 4. Local Deck

Local Deck browses and plays downloaded or other local audio files. Browsing, Find, and playback
stay local and never replace a missing result with an online stream. Features you separately
enabled, including lyrics, scrobbling, and update checks, may still use the network. Local Deck is
therefore not a whole-app offline switch.

### Enter and leave Local Deck

Open Library with `l`, then press `Alt+Shift+L` and confirm **Switch to Local Player mode?** Press
`Alt+Shift+L` again to leave.

Local Deck has a saved theme separate from the normal and Radio themes. A new installation, or an
upgraded configuration with no Local theme, starts with **Local Launch**. Save a different theme
while Local Deck is active to use it on later visits, including after a restart. Leaving restores
the normal theme.

### Browse the collection

Local Deck scans the download folder and recognizes an `Artist / Album / track` directory layout.
Number keys move among these sections:

| Section | Contents |
| --- | --- |
| **Home** | Local Deck overview |
| **Tracks**, **Albums**, **Artists**, **Genres** | Indexed views of the collection |
| **Folders** | Files in their directory structure |
| **Smart Lists** | Automatically maintained collections |
| **Scan Errors** | Files whose metadata the scanner could not read |
| **Import Sessions**, **Inbox** | Spotify imports that need review |

### Find local music

Choose **Find** in Local Deck navigation or press `Ctrl+F`. Find searches **All**, **Tracks**,
**Albums**, **Artists**, **Genres**, **Folders**, and the locally playable portion of **Playlists**.
An empty local result stays empty. Outside Find, `/` filters only the section you are viewing.

Plain words require every word to match. Put an exact phrase in quotes. These prefixes restrict
the query:

| Prefix | Field |
| --- | --- |
| `t:` | Title |
| `ar:` | Track artist |
| `al:` | Album |
| `aa:` | Album artist |
| `g:` | Genre |
| `path:` | File path |
| `fmt:` | File format |
| `year:` | Year or year range |
| `is:` | Indexed property |
| `missing:` | Missing property |
| `sort:` | Result order |

For example, `ar:bjork year:1995..2001 sort:recent` searches Björk tracks from that year range and
puts the newest matches first. A query beginning with `>` offers Local Deck commands for tasks
such as rescanning, rebuilding, queueing, or moving to a section. It never runs a shell command.

Open **Refine**, or press `/` while the results have focus, to set the scope and default sort
without changing the query. A `/` typed in the Find input remains a path character. **Apply** saves
the choice. **Cancel** or `Esc` discards the draft. A `sort:` term temporarily overrides the
Refine default. Removing the term restores that default.

### Play and queue results

- On a track, `Enter` or double-click plays it. On an album, artist, genre, folder, or playlist,
  the same action opens its local tracks.
- Press `a` or `\` to add the selected track or collection to the queue. Press `P` to play it now.
- Press `A` to add the entire result mix. Press `s` to shuffle and play the entire result mix.
  YuTuTui! follows the selected sort, removes duplicates, and shows an exact confirmation before
  the 999-item queue limit would omit any tracks.
- Empty and no-result screens offer local actions such as **Rescan**, **Add Music Folder**, and
  **View Scan Errors**.

A manual candidate search from **Import Sessions** is the one explicit exit to online Search.
YuTuTui! asks before leaving Local Deck. It sends that single query in normal mode only after you
confirm and the mode switch succeeds. Cancelling, or a stale or failed switch, sends nothing.

Downloaded tracks from `d` and `Shift+D` appear in Local Deck automatically. Add other scan roots
under **Settings → Local Deck roots**. Spotify imports can also download into the collection.

---

<a id="chapter-5"></a>

## 5. Moving from Spotify

YuTuTui! can import Spotify playlists and Liked Songs. Matching uses available title, artist, and
album data. Results can still be uncertain, so the app keeps uncertain rows for review.

The in-app flow writes to YuTuTui!'s Library playlists and does not require a YouTube account. The
command-line flow can also write playlists or likes to your YouTube Music account when you provide
the YouTube sign-in cookies described in the README.

### Register a Spotify app

Spotify requires a registered app and an allowlist for Development Mode. Under
[Spotify's 2026 Development Mode rules](https://developer.spotify.com/documentation/web-api/tutorials/february-2026-migration-guide),
the app owner needs Premium. New developers are limited to one Client ID, which means one
development app, and may allowlist up to five users. Spotify grandfathered existing allocations
above those limits. YuTuTui! uses PKCE, so it does not need a client secret.

1. Sign in at [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard).
2. Select **Create app**.
3. Enter any **App name** and **App description**.
4. Add this exact value under **Redirect URIs**:

   ```text
   http://127.0.0.1:9271/callback
   ```

   Use `127.0.0.1`, not `localhost`, and do not add a trailing slash. Spotify's
   [redirect URI rules](https://developer.spotify.com/documentation/web-api/concepts/redirect_uri)
   allow an explicit loopback IP address.
5. Select **Web API** under **Which API/SDKs are you planning to use?**
6. Accept the terms and select **Save**.
7. Open the app's **Settings** and copy the **Client ID**. Do not copy the Client secret.
8. Open **User Management** and add the Spotify account that will connect. An authorized account
   that is not allowlisted can still receive HTTP 403 responses. Spotify documents this in its
   [quota-mode rules](https://developer.spotify.com/documentation/web-api/concepts/quota-modes).

### Connect YuTuTui! to Spotify

Open **Settings (`o`) → Accounts → Spotify**, paste the Client ID, and choose **Connect**. Your
browser opens Spotify's approval page. After approval, Spotify redirects to the listener on
`127.0.0.1`. YuTuTui! waits for that callback for five minutes.

You can start the same flow from a terminal:

```sh
ytt auth spotify --client-id <YOUR-ID>
```

If the browser does not open, YuTuTui! copies the authorization URL and saves it in
`spotify_auth_url.txt`. Open that URL in a browser on the same computer so the loopback callback
can reach YuTuTui!. Using another device requires separate network forwarding.

### Import in the app

1. Open **Settings → Accounts → Import from Spotify…**.
2. Select a Spotify playlist.
3. Choose an import mode.

| Mode | Behavior |
| --- | --- |
| **Fast playlist** | Accept confident matches and safe near-matches; keep uncertain rows for review |
| **Strict playlist** | Accept only the strongest matches; keep the rest for review |
| **Review first** | Match rows but write nothing until you approve them |
| **Music video playlist** | Build a separate Library playlist from official-family video candidates |

The status line reports progress while the app remains running. When the import finishes, the
result appears under **Library → Playlists**.

Music-video mode names the result `<original name> (Music Videos)`. It favors YouTube Music OMV
and OfficialSourceMusic classifications and corroborated official channels. Public APIs do not
provide one definitive official-music-video flag. The importer rejects clearly ineligible user
uploads and sends uncertain candidates to review.

### Review uncertain matches

Open **Local Deck → Import Sessions** or **Inbox**, then open the session. Each row shows the
Spotify item and its candidate matches. Accept individual matches, or press `Shift+A` to accept
all matched candidates. A row can also retry its download or open candidate links for inspection.

### Command-line transfers

The command line exposes all transfer destinations and recovery controls:

```sh
ytt transfer import <spotify-url-or-id>      # playlist → your YTM account (needs cookies)
ytt transfer import liked --to likes         # Spotify likes → YTM likes, order kept
ytt transfer import <url> --to local:Name    # → the app's own Library playlist (no YTM account)
ytt transfer import <url> --media music-video
ytt transfer import liked --media music-video
                                             # playlist or Liked Songs → a separate official-family MV playlist
ytt transfer export ytm:<id> --to spotify    # create/append; not continuous sync
ytt transfer export ytm:<id> --to spotify:<22-character-playlist-id> --sync --dry-run
                                             # preview a destructive exact mirror
ytt transfer resume <job-id>                 # pick up after an interruption
ytt transfer backup --dir ~/music-backup     # back up every playlist to files
ytt transfer session <id>                    # inspect an import session
```

Imports store checkpoints. If a rate limit, suspension, power loss, or app shutdown interrupts a
job, run `ytt transfer resume <job-id>` after restarting. A checkpoint does not keep the process
running after shutdown.

An export to `--to spotify` uses Spotify's current `POST /me/playlists` endpoint when it must
create the destination, then appends missing tracks. It does not remove extra tracks, reproduce
duplicate positions, reorder existing entries later, or continue syncing in the background.

The ID-targeted `--sync` form performs an exact, destructive mirror to an existing playlist owned
by the connected Spotify account. Run it with `--dry-run` first to inspect additions, removals,
and ordering. If a source row is unresolved or the source was truncated, YuTuTui! stops before
changing Spotify. Otherwise, the real run mirrors order, duplicates, and removals.

Without `--yes`, YuTuTui! previews the replacement and asks for confirmation. Running
`ytt transfer resume <job-id>` refreshes that preview and asks again. Running
`ytt transfer resume <job-id> --yes` skips the confirmation.

For HTTP 403, `INVALID_CLIENT`, and port conflicts, see the
[README troubleshooting section](README.md#troubleshooting).

---

<a id="chapter-6"></a>

## 6. Backing up personal data

Open **Settings (`o`) → General → Export personal data** to write a versioned JSON export to the
system Downloads folder. The app reports the completed filename.

The terminal commands are:

```sh
ytt data export                         # save to the OS Downloads folder
ytt data export --to ~/existing-folder # choose an existing directory
```

The directory passed to `--to` must already exist. Pass a directory, not a filename. YuTuTui!
does not create the directory or fall back to the current directory when it cannot find Downloads.

### What the export contains

The default schema 2 or 3 export contains the portable personal ledger. This includes track and
radio favorites, listening and radio history, Library playlists, safe track metadata, public
catalog identifiers, recommendation signals, artist affinities, station preferences, and any
listening records supported by that ledger.

The default export does not contain:

- the app configuration or Settings values, including pending edits;
- live volume, shuffle state, or queues;
- authentication cookies, API keys, OAuth tokens, or account identifiers;
- actual filesystem paths, playable URLs, origin URLs, artwork URLs, radio stream URLs, or media
  files;
- downloads, recordings, download manifests, or media sidecars;
- caches, logs, managed-tool binaries or paths, desktop geometry, or recovery backups;
- pending scrobbles, transfer jobs, reports, or other pending work.

The export is not encrypted and contains private listening data. Store and share it as a private
file.

### Exporting from a running owner

When the main `ytt` app or daemon is running, the CLI asks it for the current personal data instead
of reading a possibly stale file. Export stops if that app is outdated, its connection information
is invalid, it is running but unreachable, or the CLI cannot identify the main instance. The CLI
reads saved data only after confirming that the registered process has ended and acquiring a lock
that prevents another instance from using the data at the same time.

If `--new-instance` players are open, the CLI exports only the registered main instance. Export
each additional instance through its own Settings screen. For an offline CLI export, close every
current-version instance.

### Import a file

```sh
ytt data import ~/Downloads/the-file.json              # preview only (the default)
ytt data import ~/Downloads/the-file.json --apply      # actually merge it
```

Without `--apply`, the command only previews the merge. An export from another installation adds
data without deleting the destination's existing records. For an export from the same
installation, the recorded order of changes prevents an older export from replacing newer
listening data.

### Schema compatibility

The default export uses the ledger's current format. It is schema 2 before listening records have
upgraded the ledger and schema 3 afterward.

```sh
ytt data export --schema 2  # request v2; refuses a ledger already upgraded to v3
ytt data export --schema 3  # request v3 explicitly
ytt data export --schema 1  # legacy format; omits listening records
```

Schema 1 alone contains its legacy settings representation. It is not a full backup of this
version after you start using listening records.

YuTuTui! creates a new owner-only export file and never overwrites an existing path. It rejects a
destination where another local account could create, replace, or remove the completed file. If
the filesystem cannot enforce and verify the required private permissions or access-control list,
the export fails.

---

<a id="chapter-7"></a>

## 7. Syncing computers

YuTuTui! can sync favorites, history, playlists, taste signals, and listening records through a
WebDAV folder on services such as Nextcloud, ownCloud, or a NAS. Sync is disabled until you set it
up.

The client encrypts vault contents before upload. The WebDAV service cannot read those contents,
but it can observe connection metadata and file metadata such as names, sizes, and timestamps.

### Set up the first device

Open **Settings (`o`) → Sync**, or run:

```sh
ytt sync setup
```

Enter the WebDAV address, username, password, and a name for the device. Use an `https://` address.
A plain `http://` address is accepted only for a server on the same computer. The password prompt
does not display the password, and the command does not accept a password as an argument.

Setup writes a recovery kit to the location you choose. Keep it outside this computer. It contains
material needed for a future recovery and cannot be regenerated. No command currently restores a
vault from the kit alone, so retain at least one connected device.

### Pair another device

On a connected device, run:

```sh
ytt sync pair create
```

The command prints a one-use code that expires after ten minutes. On the new device, run:

```sh
ytt sync pair join ABCDE-FGHIJ-KLMNO-PQRST-UV
```

Enter the same WebDAV details on the new device. It then waits for approval. The connected device
shows the joining device's name and a short key fingerprint before you approve it. Approve only
the device you are currently adding. The joining device does not display the fingerprint for
comparison, so the short-lived one-use code protects this step.

Resume or cancel an unfinished pairing with:

```sh
ytt sync pair join --resume   # pick up where you left off, no code needed
ytt sync pair cancel          # throw away an unfinished attempt
```

### Check and run sync

```sh
ytt sync status          # one line: what state sync is in
ytt sync now             # merge this device with the folder right now
```

| Status | Meaning |
| --- | --- |
| **Off** | Sync is not configured on this device |
| **Up to date** | The local device and vault match |
| **Syncing** | A merge is running |
| **Offline — will retry** | The service is unreachable and the client will retry |
| **Needs attention** | The following line describes a decision or problem |

### Manage devices

```sh
ytt sync devices                  # list active and removed devices
ytt sync revoke <DEVICE_ID>       # remove a device you no longer have
ytt sync recovery export --to DIR # save another copy of the recovery kit
ytt sync audit                    # what sync has done, with no private details
```

Revoking a device changes access so it cannot decrypt data uploaded afterward. Revocation cannot
erase or make unreadable anything that the device downloaded before it was removed.

---

<a id="chapter-8"></a>

## 8. Playing from a music server

YuTuTui! can connect to one OpenSubsonic or Navidrome server. Open
**Settings (`o`) → Music server**, or use these commands:

```sh
ytt server setup            # test and save a connection
ytt server status           # show the connection, with secrets left out
ytt server remove           # forget the server; your local data stays
```

`ytt server setup` asks for the address and either a password or API key. It hides the entered
secret and does not accept the secret as a command argument.

YuTuTui! sends ratings and listening reports to the server. A failed report remains pending until
you retry it or mark it as sent:

```sh
ytt server scrobbles list                  # reports waiting on a decision
ytt server scrobbles retry <OPAQUE_ID>     # try that one again
ytt server scrobbles mark-sent <OPAQUE_ID> # accept it as done, stop retrying
```

If a connection drops during playlist creation, the server may have created the playlist even
though YuTuTui! did not receive confirmation. Inspect or abandon the local guard with:

```sh
ytt server playlists pending
ytt server playlists abandon <LOCAL_PLAYLIST_ID>   # forget the guard; deletes nothing
```

Navidrome detailed history is experimental and disabled by default:

```sh
ytt server history enable --experimental
ytt server history disable      # also removes the extra saved password
```

Detailed history needs its own password. Disabling it does not change the ordinary server
connection.

---

<a id="chapter-9"></a>

## 9. Troubleshooting

Quit the app and run:

```sh
ytt doctor
```

The report checks required helper programs and gives setup instructions for anything missing. For
playback, artwork, scrobbling, Spotify, and other known problems, use the symptom tables in the
[README troubleshooting section](README.md#troubleshooting).

For a YouTube stream error such as HTTP 403 or 429, run:

```sh
ytt doctor --verbose
```

Then follow the [playback troubleshooting steps](README.md#playback).

If the problem remains, [open an issue](https://github.com/Ochichan/Yututui/issues). Include your
operating system, the action that caused the problem, the complete error, and relevant diagnostic
results. Remove credentials and personal paths before posting.
