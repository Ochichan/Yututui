//! Local, on-disk playlists — the backend behind the DJ Gem assistant's playlist tools
//! (`get_user_playlists`, `create_playlist`, `add_to_playlist`, `play_playlist`).
//!
//! yututui has no account-sourced playlists, so these are *ours*: created and edited
//! locally, persisted to `<data dir>/playlists.json`. Persistence mirrors [`crate::config`]
//! and [`crate::library`]: pretty JSON written atomically (temp file + rename), with both
//! the playlist count and per-playlist track count bounded at write time (priority #1:
//! flat memory).

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::{
    Song, sanitize_album, sanitize_artist, sanitize_duration, sanitize_provider_id, sanitize_title,
};
use crate::util::safe_fs;

/// Caps (bounded memory).
const PLAYLISTS_MAX: usize = 999;
pub(crate) const SONGS_PER_PLAYLIST_MAX: usize = 999;
/// Upper bound on `playlists.json` when loading. With ≤999 playlists × ≤999 tracks written,
/// a legitimate file is comfortably under this; a larger one is corrupt/hostile and set aside.
const MAX_PLAYLISTS_BYTES: u64 = 50 * 1024 * 1024;

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

/// Summary of repairs applied to a loaded playlists file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PlaylistRepairReport {
    pub playlists_removed: usize,
    pub songs_removed: usize,
    pub names_repaired: usize,
    pub ids_repaired: usize,
    pub songs_sanitized: usize,
}

impl PlaylistRepairReport {
    pub fn changed(&self) -> bool {
        self.playlists_removed > 0
            || self.songs_removed > 0
            || self.names_repaired > 0
            || self.ids_repaired > 0
            || self.songs_sanitized > 0
    }

    pub fn truncated(&self) -> bool {
        self.playlists_removed > 0 || self.songs_removed > 0
    }
}

/// All local playlists, persisted to `<data dir>/playlists.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Playlists {
    pub playlists: Vec<Playlist>,
    /// In-process mutation generation used by Local Find projections. It is deliberately
    /// absent from playlists.json; persisted playlist shape and compatibility stay unchanged.
    #[serde(skip)]
    pub(crate) revision: u64,
}

impl Playlists {
    pub(crate) fn preflight_persistence_recovery()
    -> Result<(), crate::persist::StartupRecoveryError> {
        let Some(path) = playlists_path() else {
            return Ok(());
        };
        crate::persist::preflight_journal_recovery::<Self>(
            crate::persist::StoreKind::Playlists,
            &path,
            MAX_PLAYLISTS_BYTES,
        )
    }

    /// Load from disk, falling back to empty if absent or unreadable.
    pub fn load() -> Self {
        Self::load_with_repair_report().0
    }

    /// Load from disk and report any shape repair applied to the in-memory result.
    pub fn load_with_repair_report() -> (Self, PlaylistRepairReport) {
        let Some(path) = playlists_path() else {
            return (Playlists::default(), PlaylistRepairReport::default());
        };
        // Schema-drift tolerant *and* size-bounded: one changed field no longer discards saved
        // playlists, and a corrupt/hostile oversized file is set aside rather than read whole
        // into memory. Loaded entries are then count-repaired to the same caps as mutations.
        let mut playlists = crate::persist::load_with_journal_recovery(
            crate::persist::StoreKind::Playlists,
            &path,
            MAX_PLAYLISTS_BYTES,
            || safe_fs::load_json_or_default_limited::<Playlists>(&path, MAX_PLAYLISTS_BYTES),
        );
        let report = playlists.repair_loaded();
        (playlists, report)
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        crate::persist::ensure_persistence_writes_allowed()?;
        let Some(path) = playlists_path() else {
            return Ok(());
        };
        crate::persist::write_store_json(&path, self)
    }

    pub fn list(&self) -> &[Playlist] {
        &self.playlists
    }

    /// Monotonic in-process generation for immutable Local Find projections.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replace a live store after an external writer changed playlists.json while preserving a
    /// monotonic in-process generation for Local Find corpus invalidation.
    pub(crate) fn replace_reloaded(&mut self, mut reloaded: Self) {
        reloaded.revision = self.revision.wrapping_add(1);
        *self = reloaded;
    }

    /// Repair a deserialized playlists file so it obeys the same bounded shape as create/add.
    pub fn repair_loaded(&mut self) -> PlaylistRepairReport {
        let mut report = PlaylistRepairReport::default();
        if self.playlists.len() > PLAYLISTS_MAX {
            report.playlists_removed = self.playlists.len() - PLAYLISTS_MAX;
            self.playlists.truncate(PLAYLISTS_MAX);
        }

        let mut used_ids = HashSet::with_capacity(self.playlists.len());
        for playlist in &mut self.playlists {
            let trimmed_name = playlist.name.trim();
            if trimmed_name.is_empty() {
                playlist.name = "Playlist".to_owned();
                report.names_repaired += 1;
            } else if trimmed_name != playlist.name {
                playlist.name = trimmed_name.to_owned();
                report.names_repaired += 1;
            }

            let trimmed_id = playlist.id.trim().to_owned();
            if is_sane_playlist_id(&trimmed_id) && used_ids.insert(trimmed_id.clone()) {
                if trimmed_id != playlist.id {
                    playlist.id = trimmed_id;
                    report.ids_repaired += 1;
                }
            } else {
                playlist.id = unique_repaired_id(&playlist.name, &mut used_ids);
                report.ids_repaired += 1;
            }

            if playlist.songs.len() > SONGS_PER_PLAYLIST_MAX {
                report.songs_removed += playlist.songs.len() - SONGS_PER_PLAYLIST_MAX;
                playlist.songs.truncate(SONGS_PER_PLAYLIST_MAX);
            }
            for song in &mut playlist.songs {
                if sanitize_loaded_song(song) {
                    report.songs_sanitized += 1;
                }
            }
        }
        if report.changed() {
            self.bump_revision();
        }
        report
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
        self.bump_revision();
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
        let result = match idx {
            Some(i) => Self::push_song(&mut self.playlists[i], song),
            None => AddResult::NotFound,
        };
        if result == AddResult::Added {
            self.bump_revision();
        }
        result
    }

    /// Bulk append with one destination lookup and one de-duplication set. Results stay
    /// aligned with `songs`, allowing resumable import callers to checkpoint each row.
    pub(crate) fn add_many(&mut self, key: &str, songs: Vec<Song>) -> Vec<AddResult> {
        let key = key.trim();
        let idx = self.playlists.iter().position(|p| p.id == key).or_else(|| {
            self.playlists
                .iter()
                .position(|p| p.name.eq_ignore_ascii_case(key))
        });
        let Some(idx) = idx else {
            return vec![AddResult::NotFound; songs.len()];
        };
        let playlist = &mut self.playlists[idx];
        let mut present = playlist
            .songs
            .iter()
            .map(|song| song.video_id.clone())
            .collect::<HashSet<_>>();
        let mut results = Vec::with_capacity(songs.len());
        for song in songs {
            if present.contains(&song.video_id) {
                results.push(AddResult::Duplicate);
            } else if playlist.songs.len() >= SONGS_PER_PLAYLIST_MAX {
                results.push(AddResult::Full);
            } else {
                present.insert(song.video_id.clone());
                playlist.songs.push(song);
                results.push(AddResult::Added);
            }
        }
        if results.contains(&AddResult::Added) {
            self.bump_revision();
        }
        results
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
        let removed = self.playlists.remove(idx);
        self.bump_revision();
        Some(removed)
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
        let changed = self.playlists[i].songs.len() != before;
        if changed {
            self.bump_revision();
        }
        changed
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

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
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

fn is_sane_playlist_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn unique_repaired_id(name: &str, used: &mut HashSet<String>) -> String {
    let base = slug(name);
    let base = if base.is_empty() {
        "playlist".to_owned()
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut suffix = 2usize;
    while used.contains(&candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    used.insert(candidate.clone());
    candidate
}

fn sanitize_loaded_song(song: &mut Song) -> bool {
    let video_id = sanitize_provider_id(&song.video_id);
    let title = sanitize_title(&song.title);
    let artist = sanitize_artist(&song.artist);
    let duration = sanitize_duration(&song.duration);
    let album = song
        .album
        .as_deref()
        .map(sanitize_album)
        .filter(|album| !album.is_empty());
    let yt_video_id = song
        .yt_video_id
        .as_deref()
        .map(sanitize_provider_id)
        .filter(|id| !id.is_empty());
    let changed = video_id != song.video_id
        || title != song.title
        || artist != song.artist
        || duration != song.duration
        || album != song.album
        || yt_video_id != song.yt_video_id;
    if changed {
        song.video_id = video_id;
        song.title = title;
        song.artist = artist;
        song.duration = duration;
        song.album = album;
        song.yt_video_id = yt_video_id;
    }
    changed
}

pub(crate) fn playlists_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("playlists.json"))
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
        let revision = p.revision();
        let id = p.create("Chill Vibes!").unwrap();
        assert_eq!(id, "chill-vibes");
        assert_eq!(p.list().len(), 1);
        assert_ne!(p.revision(), revision);
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
        let before_add = p.revision();
        assert_eq!(p.add("Mix", song("a")), AddResult::Added);
        assert_ne!(p.revision(), before_add);
        let after_add = p.revision();
        assert_eq!(p.add("mix", song("a")), AddResult::Duplicate); // same id, by name
        assert_eq!(p.add("missing", song("b")), AddResult::NotFound);
        assert_eq!(p.revision(), after_add, "no-op mutations keep the revision");
        assert_eq!(p.find("mix").unwrap().songs.len(), 1);
    }

    #[test]
    fn add_many_preserves_order_and_dedupes_in_one_pass() {
        let mut playlists = Playlists::default();
        playlists.create("Mix").unwrap();
        assert_eq!(playlists.add("Mix", song("existing")), AddResult::Added);

        let results = playlists.add_many(
            "Mix",
            vec![song("existing"), song("new"), song("new"), song("last")],
        );

        assert_eq!(
            results,
            vec![
                AddResult::Duplicate,
                AddResult::Added,
                AddResult::Duplicate,
                AddResult::Added,
            ]
        );
        assert_eq!(
            playlists
                .find("Mix")
                .unwrap()
                .songs
                .iter()
                .map(|song| song.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["existing", "new", "last"]
        );
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
        let before_remove = p.revision();
        assert!(p.remove_song(&id, "a"));
        assert_ne!(p.revision(), before_remove);
        let after_remove = p.revision();
        assert!(!p.remove_song(&id, "a")); // already removed
        assert!(!p.remove_song("missing", "b")); // unknown playlist
        assert_eq!(p.revision(), after_remove);
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

    #[test]
    fn repair_loaded_truncates_playlist_and_song_caps() {
        let mut playlists = Playlists {
            playlists: (0..PLAYLISTS_MAX + 2)
                .map(|i| Playlist {
                    id: format!("p{i}"),
                    name: format!("P {i}"),
                    songs: if i == 0 {
                        (0..SONGS_PER_PLAYLIST_MAX + 3)
                            .map(|n| song(&format!("s{n}")))
                            .collect()
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            revision: 0,
        };

        let report = playlists.repair_loaded();

        assert_eq!(playlists.list().len(), PLAYLISTS_MAX);
        assert_eq!(playlists.list()[0].songs.len(), SONGS_PER_PLAYLIST_MAX);
        assert_eq!(report.playlists_removed, 2);
        assert_eq!(report.songs_removed, 3);
        assert!(report.changed());
        assert!(report.truncated());
    }

    #[test]
    fn repair_loaded_repairs_blank_names_and_duplicate_or_bad_ids() {
        let mut playlists = Playlists {
            playlists: vec![
                Playlist {
                    id: "mix".to_owned(),
                    name: "  Mix  ".to_owned(),
                    songs: Vec::new(),
                },
                Playlist {
                    id: "mix".to_owned(),
                    name: "Second Mix".to_owned(),
                    songs: Vec::new(),
                },
                Playlist {
                    id: "../bad".to_owned(),
                    name: "   ".to_owned(),
                    songs: Vec::new(),
                },
            ],
            revision: 0,
        };

        let report = playlists.repair_loaded();

        assert_eq!(playlists.list()[0].name, "Mix");
        assert_eq!(playlists.list()[0].id, "mix");
        assert_eq!(playlists.list()[1].id, "second-mix");
        assert_eq!(playlists.list()[2].name, "Playlist");
        assert_eq!(playlists.list()[2].id, "playlist");
        assert_eq!(report.names_repaired, 2);
        assert_eq!(report.ids_repaired, 2);
    }

    #[test]
    fn repair_loaded_sanitizes_deserialized_song_metadata() {
        let mut dirty = song("dirty");
        dirty.video_id = " dirty\u{202e}id ".to_owned();
        dirty.title = "  title\u{7}  ".to_owned();
        dirty.artist = " artist\u{200f} ".to_owned();
        dirty.duration = "  3:00\u{0}  ".to_owned();
        dirty.album = Some(" album\u{202d} ".to_owned());
        dirty.yt_video_id = Some(" yt\u{202e}id ".to_owned());
        let mut playlists = Playlists {
            playlists: vec![Playlist {
                id: "mix".to_owned(),
                name: "Mix".to_owned(),
                songs: vec![dirty],
            }],
            revision: 0,
        };

        let report = playlists.repair_loaded();
        let repaired = &playlists.list()[0].songs[0];

        assert_eq!(repaired.video_id, "dirtyid");
        assert_eq!(repaired.title, "title");
        assert_eq!(repaired.artist, "artist");
        assert_eq!(repaired.duration, "3:00");
        assert_eq!(repaired.album.as_deref(), Some("album"));
        assert_eq!(repaired.yt_video_id.as_deref(), Some("ytid"));
        assert_eq!(report.songs_sanitized, 1);
    }

    #[test]
    fn repair_loaded_leaves_normal_playlist_unchanged() {
        let mut playlists = Playlists::default();
        let id = playlists.create("Faves").unwrap();
        playlists.add(&id, song("x"));
        let before = serde_json::to_string(&playlists).unwrap();

        let report = playlists.repair_loaded();

        assert_eq!(report, PlaylistRepairReport::default());
        assert_eq!(serde_json::to_string(&playlists).unwrap(), before);
    }
}
