//! Local, on-disk playlists — the backend behind the DJ Gem assistant's playlist tools
//! (`get_user_playlists`, `create_playlist`, `add_to_playlist`, `play_playlist`).
//!
//! ytm-tui has no account-sourced playlists, so these are *ours*: created and edited
//! locally, persisted to `<data dir>/playlists.json`. Persistence mirrors [`crate::config`]
//! and [`crate::library`]: pretty JSON written atomically (temp file + rename), with both
//! the playlist count and per-playlist track count bounded at write time (priority #1:
//! flat memory).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::Song;
use crate::util::safe_fs;

/// Caps (bounded memory).
const PLAYLISTS_MAX: usize = 999;
const SONGS_PER_PLAYLIST_MAX: usize = 999;

/// A named, ordered collection of tracks with a stable slug `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Playlist {
    /// Stable, URL-ish slug derived from the name at creation (e.g. `"chill-vibes"`).
    pub id: String,
    pub name: String,
    pub songs: Vec<Song>,
}

/// The outcome of [`Playlists::add`], so callers can report a precise message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddResult {
    Added,
    NotFound,
    Duplicate,
    Full,
}

/// All local playlists, persisted to `<data dir>/playlists.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Playlists {
    pub playlists: Vec<Playlist>,
}

impl Playlists {
    /// Load from disk, falling back to empty if absent or unreadable.
    pub fn load() -> Self {
        let Some(path) = playlists_path() else {
            return Playlists::default();
        };
        // Schema-drift tolerant: one changed field no longer discards saved playlists.
        safe_fs::load_json_or_default::<Playlists>(&path)
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = playlists_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    pub fn list(&self) -> &[Playlist] {
        &self.playlists
    }

    /// Resolve a playlist by `id` first, then by case-insensitive name — the DJ Gem may
    /// reference either.
    pub fn find(&self, key: &str) -> Option<&Playlist> {
        let key = key.trim();
        self.playlists.iter().find(|p| p.id == key).or_else(|| {
            self.playlists
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(key))
        })
    }

    /// Create a playlist with a unique slug id. Returns the new id, or `None` if the name
    /// is blank or the playlist cap is reached.
    pub fn create(&mut self, name: &str) -> Option<String> {
        let name = name.trim();
        if name.is_empty() || self.playlists.len() >= PLAYLISTS_MAX {
            return None;
        }
        let id = self.unique_id(name);
        self.playlists.push(Playlist {
            id: id.clone(),
            name: name.to_owned(),
            songs: Vec::new(),
        });
        Some(id)
    }

    /// Add `song` to the playlist matched by `key` (id or name). De-dupes by `video_id`
    /// and respects the per-playlist cap.
    pub fn add(&mut self, key: &str, song: Song) -> AddResult {
        let key = key.trim();
        // Resolve to an index first (immutable borrow), then mutate.
        let idx = self.playlists.iter().position(|p| p.id == key).or_else(|| {
            self.playlists
                .iter()
                .position(|p| p.name.eq_ignore_ascii_case(key))
        });
        match idx {
            Some(i) => Self::push_song(&mut self.playlists[i], song),
            None => AddResult::NotFound,
        }
    }

    /// Remove the playlist matched by `key` (id or name), returning it so callers can
    /// name it in a status message.
    pub fn delete(&mut self, key: &str) -> Option<Playlist> {
        let key = key.trim();
        let idx = self
            .playlists
            .iter()
            .position(|p| p.id == key)
            .or_else(|| {
                self.playlists
                    .iter()
                    .position(|p| p.name.eq_ignore_ascii_case(key))
            })?;
        Some(self.playlists.remove(idx))
    }

    /// Remove one track (by `video_id`) from the playlist matched by `key`. Returns
    /// whether anything was removed.
    pub fn remove_song(&mut self, key: &str, video_id: &str) -> bool {
        let key = key.trim();
        let idx = self.playlists.iter().position(|p| p.id == key).or_else(|| {
            self.playlists
                .iter()
                .position(|p| p.name.eq_ignore_ascii_case(key))
        });
        let Some(i) = idx else { return false };
        let before = self.playlists[i].songs.len();
        self.playlists[i].songs.retain(|s| s.video_id != video_id);
        self.playlists[i].songs.len() != before
    }

    fn push_song(p: &mut Playlist, song: Song) -> AddResult {
        if p.songs.iter().any(|s| s.video_id == song.video_id) {
            return AddResult::Duplicate;
        }
        if p.songs.len() >= SONGS_PER_PLAYLIST_MAX {
            return AddResult::Full;
        }
        p.songs.push(song);
        AddResult::Added
    }

    /// A unique slug id derived from `name`, disambiguated with a numeric suffix.
    fn unique_id(&self, name: &str) -> String {
        let base = slug(name);
        let base = if base.is_empty() {
            "playlist".to_owned()
        } else {
            base
        };
        if !self.playlists.iter().any(|p| p.id == base) {
            return base;
        }
        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|c| !self.playlists.iter().any(|p| &p.id == c))
            .unwrap_or(base)
    }
}

/// Lowercase ASCII-alnum slug; runs of other chars collapse to a single `-`, trimmed.
fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.extend(ch.to_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

fn playlists_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.data_dir().join("playlists.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("t-{id}"), "a", "1:00")
    }

    #[test]
    fn create_slugs_the_name() {
        let mut p = Playlists::default();
        let id = p.create("Chill Vibes!").unwrap();
        assert_eq!(id, "chill-vibes");
        assert_eq!(p.list().len(), 1);
    }

    #[test]
    fn create_disambiguates_colliding_slugs() {
        let mut p = Playlists::default();
        assert_eq!(p.create("Road Trip").unwrap(), "road-trip");
        assert_eq!(p.create("road  trip").unwrap(), "road-trip-2");
        assert_eq!(p.create("ROAD TRIP").unwrap(), "road-trip-3");
    }

    #[test]
    fn create_rejects_blank_names() {
        let mut p = Playlists::default();
        assert!(p.create("   ").is_none());
        assert!(p.list().is_empty());
    }

    #[test]
    fn find_matches_by_id_or_name() {
        let mut p = Playlists::default();
        let id = p.create("My Mix").unwrap();
        assert_eq!(p.find(&id).unwrap().name, "My Mix");
        assert_eq!(p.find("my mix").unwrap().id, id); // case-insensitive name
        assert!(p.find("nope").is_none());
    }

    #[test]
    fn add_dedupes_and_reports_outcome() {
        let mut p = Playlists::default();
        p.create("Mix").unwrap();
        assert_eq!(p.add("Mix", song("a")), AddResult::Added);
        assert_eq!(p.add("mix", song("a")), AddResult::Duplicate); // same id, by name
        assert_eq!(p.add("missing", song("b")), AddResult::NotFound);
        assert_eq!(p.find("mix").unwrap().songs.len(), 1);
    }

    #[test]
    fn add_respects_per_playlist_cap() {
        let mut p = Playlists::default();
        let id = p.create("Big").unwrap();
        for i in 0..SONGS_PER_PLAYLIST_MAX {
            assert_eq!(p.add(&id, song(&i.to_string())), AddResult::Added);
        }
        assert_eq!(p.add(&id, song("overflow")), AddResult::Full);
    }

    #[test]
    fn playlist_count_is_capped() {
        let mut p = Playlists::default();
        for i in 0..PLAYLISTS_MAX {
            assert!(p.create(&format!("p{i}")).is_some());
        }
        assert!(p.create("one too many").is_none());
    }

    #[test]
    fn delete_matches_by_id_or_name_and_reports_miss() {
        let mut p = Playlists::default();
        let id = p.create("Old Mix").unwrap();
        assert!(p.delete("nope").is_none());
        assert_eq!(p.delete("old mix").unwrap().id, id); // case-insensitive name
        assert!(p.list().is_empty());
        assert!(p.delete(&id).is_none()); // already gone
    }

    #[test]
    fn remove_song_by_video_id() {
        let mut p = Playlists::default();
        let id = p.create("Mix").unwrap();
        p.add(&id, song("a"));
        p.add(&id, song("b"));
        assert!(p.remove_song(&id, "a"));
        assert!(!p.remove_song(&id, "a")); // already removed
        assert!(!p.remove_song("missing", "b")); // unknown playlist
        assert_eq!(p.find(&id).unwrap().songs.len(), 1);
        assert_eq!(p.find(&id).unwrap().songs[0].video_id, "b");
    }

    #[test]
    fn json_round_trips() {
        let mut p = Playlists::default();
        let id = p.create("Faves").unwrap();
        p.add(&id, song("x"));
        let s = serde_json::to_string(&p).unwrap();
        let back: Playlists = serde_json::from_str(&s).unwrap();
        assert_eq!(back.find(&id).unwrap().songs[0].video_id, "x");
    }

    #[test]
    fn missing_fields_default() {
        let back: Playlists = serde_json::from_str("{}").unwrap();
        assert!(back.list().is_empty());
    }
}
