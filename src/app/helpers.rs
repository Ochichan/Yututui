//! Small cross-reducer helper functions.

use super::*;

/// Build the lyrics-fetch effect for `song`.
pub(in crate::app) fn fetch_lyrics_cmd(song: &Song) -> Cmd {
    Cmd::FetchLyrics {
        video_id: song.video_id.clone(),
        artist: song.artist.clone(),
        title: song.title.clone(),
    }
}

pub(in crate::app) fn song_label(song: &Song) -> String {
    if song.artist.trim().is_empty() {
        song.title.clone()
    } else {
        format!("{} — {}", song.title, song.artist)
    }
}

pub(in crate::app) fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

pub(crate) use crate::util::browser::open_in_browser;

// Both owners spawn the same overlay window; the implementation moved to the shared
// module so the daemon's `PlayVideo` host launches an identical mpv.
pub(in crate::app) use crate::video_overlay::spawn_video_overlay;
