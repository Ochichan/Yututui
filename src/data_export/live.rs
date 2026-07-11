//! Bounded owner-lane capture for live personal-data exports.
//!
//! The runtime worker owns source clones so projection and IO stay off the owner loop. Validate
//! their dynamic heap first: persisted collection counts alone do not bound nested string vectors.

use std::io::{self, Write};

use serde::Serialize;

use crate::api::{PlayableRef, Song};
use crate::config::Config;
use crate::library::Library;
use crate::playlists::Playlists;
use crate::signals::Signals;
use crate::station::StationStore;

pub(crate) const PLAYLIST_TRACK_LIMIT: usize = 10_000;
pub(crate) const CLONE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const NESTED_TEXT_ITEMS_LIMIT: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeError {
    TooManyPlaylistTracks(usize),
    TooManyNestedTextItems(usize),
    CloneBudgetExceeded,
    InvalidSource,
    Overflow,
}

impl SizeError {
    pub(crate) fn detail(self) -> String {
        match self {
            Self::TooManyPlaylistTracks(count) => {
                format!("it has {count} playlist track entries (limit {PLAYLIST_TRACK_LIMIT})")
            }
            Self::TooManyNestedTextItems(count) => format!(
                "track/station metadata has {count} nested text entries (limit {NESTED_TEXT_ITEMS_LIMIT})"
            ),
            Self::CloneBudgetExceeded => format!(
                "copying its live metadata would exceed {} MiB",
                CLONE_BUDGET_BYTES / (1024 * 1024)
            ),
            Self::InvalidSource => {
                "some live settings or preference values cannot be serialized safely".to_owned()
            }
            Self::Overflow => "its live metadata is too large to count safely".to_owned(),
        }
    }
}

pub(crate) fn checked_playlist_track_count(
    lengths: impl IntoIterator<Item = usize>,
) -> Result<usize, SizeError> {
    let total = lengths
        .into_iter()
        .try_fold(0usize, |total, len| total.checked_add(len))
        .ok_or(SizeError::Overflow)?;
    if total > PLAYLIST_TRACK_LIMIT {
        Err(SizeError::TooManyPlaylistTracks(total))
    } else {
        Ok(total)
    }
}

/// Validate every store cloned by a live owner. Daemon callers pass no playlists because their
/// worker loads that immutable store directly; TUI callers include the in-memory playlist view.
pub(crate) fn validate_source_clone(
    config: &Config,
    library: &Library,
    playlists: Option<&Playlists>,
    signals: &Signals,
    station: &StationStore,
) -> Result<usize, SizeError> {
    if let Some(playlists) = playlists {
        checked_playlist_track_count(playlists.list().iter().map(|playlist| playlist.songs.len()))?;
    }

    let mut budget = CloneBudget::default();
    for fixed in [
        std::mem::size_of_val(config),
        std::mem::size_of_val(library),
        std::mem::size_of_val(signals),
        std::mem::size_of_val(station),
    ] {
        budget.add(fixed)?;
    }
    count_serialized(&mut budget, config)?;
    count_serialized(&mut budget, signals)?;
    for song in library
        .favorites
        .iter()
        .chain(library.history.iter())
        .chain(library.radio_favorites.iter())
        .chain(library.radios.iter())
    {
        budget.add_song(song)?;
    }
    if let Some(playlists) = playlists {
        for playlist in playlists.list() {
            budget.add(std::mem::size_of_val(playlist))?;
            budget.add_text(&playlist.id)?;
            budget.add_text(&playlist.name)?;
            for song in &playlist.songs {
                budget.add_song(song)?;
            }
        }
    }
    if let Some(profile) = station.active.as_ref() {
        budget.add_text(&profile.query)?;
        budget.add_nested_texts(&profile.avoid_artist_keys)?;
    }
    Ok(budget.bytes)
}

#[derive(Default)]
struct CloneBudget {
    bytes: usize,
    nested_text_items: usize,
    exceeded: bool,
}

impl CloneBudget {
    fn add(&mut self, bytes: usize) -> Result<(), SizeError> {
        self.bytes = self.bytes.checked_add(bytes).ok_or(SizeError::Overflow)?;
        if self.bytes > CLONE_BUDGET_BYTES {
            self.exceeded = true;
            return Err(SizeError::CloneBudgetExceeded);
        }
        Ok(())
    }

    fn add_text(&mut self, value: &str) -> Result<(), SizeError> {
        self.add(value.len())
    }

    fn add_optional_text(&mut self, value: Option<&str>) -> Result<(), SizeError> {
        if let Some(value) = value {
            self.add_text(value)?;
        }
        Ok(())
    }

    fn add_nested_texts(&mut self, values: &[String]) -> Result<(), SizeError> {
        self.nested_text_items = self
            .nested_text_items
            .checked_add(values.len())
            .ok_or(SizeError::Overflow)?;
        if self.nested_text_items > NESTED_TEXT_ITEMS_LIMIT {
            return Err(SizeError::TooManyNestedTextItems(self.nested_text_items));
        }
        self.add(
            values
                .len()
                .checked_mul(std::mem::size_of::<String>())
                .ok_or(SizeError::Overflow)?,
        )?;
        for value in values {
            self.add_text(value)?;
        }
        Ok(())
    }

    fn add_song(&mut self, song: &Song) -> Result<(), SizeError> {
        self.add(std::mem::size_of::<Song>())?;
        for value in [&song.video_id, &song.title, &song.artist, &song.duration] {
            self.add_text(value)?;
        }
        for value in [
            song.album.as_deref(),
            song.album_artist.as_deref(),
            song.album_release_date.as_deref(),
            song.album_release_date_precision.as_deref(),
            song.album_type.as_deref(),
            song.album_art_url.as_deref(),
            song.isrc.as_deref(),
            song.origin_key.as_deref(),
            song.origin_url.as_deref(),
            song.import_session_id.as_deref(),
            song.yt_video_id.as_deref(),
        ] {
            self.add_optional_text(value)?;
        }
        self.add_nested_texts(&song.artists)?;
        self.add_nested_texts(&song.album_artists)?;
        if let Some(path) = song.local_path.as_deref() {
            self.add(path.as_os_str().as_encoded_bytes().len())?;
        }
        if let Some(playable) = song.playable.as_ref() {
            self.add_playable(playable)?;
        }
        Ok(())
    }

    fn add_playable(&mut self, playable: &PlayableRef) -> Result<(), SizeError> {
        match playable {
            PlayableRef::YoutubeVideo { id } => self.add_text(id),
            PlayableRef::DirectUrl { url, .. }
            | PlayableRef::YtdlpUrl { url, .. }
            | PlayableRef::RadioStream { url } => self.add_text(url),
            PlayableRef::AudiusTrackId { id, app_name } => {
                self.add_text(id)?;
                self.add_text(app_name)
            }
            PlayableRef::JamendoTrackId { id, url } => {
                self.add_text(id)?;
                self.add_text(url)
            }
            PlayableRef::ArchiveFile {
                identifier,
                file,
                url,
            } => {
                self.add_text(identifier)?;
                self.add_text(file)?;
                self.add_text(url)
            }
        }
    }
}

struct BudgetWriter<'a>(&'a mut CloneBudget);

impl Write for BudgetWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.add(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::FileTooLarge,
                "live export clone budget exceeded",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn count_serialized<T: Serialize>(budget: &mut CloneBudget, value: &T) -> Result<(), SizeError> {
    let mut writer = BudgetWriter(budget);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(()),
        Err(_) if writer.0.exceeded => Err(SizeError::CloneBudgetExceeded),
        Err(_) => Err(SizeError::InvalidSource),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_shape_rejects_nested_library_amplification() {
        let mut library = Library::default();
        let mut song = Song::remote("track", "Track", "Artist", "1:00");
        song.album_artists = vec![String::new(); NESTED_TEXT_ITEMS_LIMIT + 1];
        library.favorites.push(song);

        assert_eq!(
            validate_source_clone(
                &Config::default(),
                &library,
                None,
                &Signals::default(),
                &StationStore::default(),
            ),
            Err(SizeError::TooManyNestedTextItems(
                NESTED_TEXT_ITEMS_LIMIT + 1
            ))
        );
    }
}
