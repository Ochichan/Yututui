//! Natural-language "station" profiles: a free-text vibe distilled (by the assistant, from a
//! `start_streaming` call) into the handful of engine knobs the local streaming can actually act on.
//!
//! Honest scope: a [`crate::api::Song`] carries no genre / mood / audio-feature tags, so a
//! profile never pretends to set "energy" or "valence". It maps to what *does* move the engine:
//! adventurousness ([`Explore`] → [`StreamingMode`]) and a set of artists to keep out (folded into
//! [`crate::streaming::StationState`]'s `banned_artist_keys`). Persisted to `<data dir>/station.json`,
//! mirroring [`crate::playlists`], so a later feedback pass can refine it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::streaming::StreamingMode;
use crate::util::safe_fs;

/// How adventurous the listener asked the station to be — a small, stable, model-facing scale
/// that maps onto the engine's [`StreamingMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Explore {
    /// Stay tight to the seed.
    Tight,
    /// The shipped default: balance familiarity and exploration.
    #[default]
    Balanced,
    /// Lean into discovery.
    Wide,
}

impl Explore {
    /// The engine mode this explore level drives (λ / temperature / artist-gap live there).
    pub fn to_mode(self) -> StreamingMode {
        match self {
            Explore::Tight => StreamingMode::Focused,
            Explore::Balanced => StreamingMode::Balanced,
            Explore::Wide => StreamingMode::Discovery,
        }
    }

    /// Tolerantly parse the model's string (the enum value or a loose synonym); unknown →
    /// Balanced, so a vague vibe degrades to the safe default rather than erroring.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "tight" | "focused" | "narrow" | "close" | "similar" => Explore::Tight,
            "wide" | "discovery" | "adventurous" | "explore" | "broad" | "diverse" => Explore::Wide,
            _ => Explore::Balanced,
        }
    }
}

/// A durable station distilled from a free-text vibe. Applied to the live engine and persisted so
/// a later feedback pass can refine it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationProfile {
    /// The seed / vibe the station was started from (for display + re-interpretation).
    pub query: String,
    /// Adventurousness → [`StreamingMode`].
    pub explore: Explore,
    /// Normalized artist keys to keep out of the station (folded into `banned_artist_keys`).
    #[serde(default)]
    pub avoid_artist_keys: Vec<String>,
}

impl StationProfile {
    /// Build a profile from the assistant's raw `start_streaming` arguments, normalizing the avoid
    /// list to engine artist keys. `explore` is the model's (possibly absent) string; an
    /// absent / unrecognized value falls back to Balanced.
    pub fn from_intent(query: &str, explore: Option<&str>, avoid_artists: &[String]) -> Self {
        Self {
            query: query.trim().to_owned(),
            explore: explore.map(Explore::parse).unwrap_or_default(),
            avoid_artist_keys: normalize_keys(avoid_artists),
        }
    }

    /// Fold a feedback pass into the avoid list: artists the listener kept skipping
    /// (`down_artists`) are added; artists they warmed to (`boost_artists`) are removed. Both are
    /// normalized to engine keys. Deliberately conservative — it never touches the explore level,
    /// since a handful of skips shouldn't silently flip (and persist) the station's streaming mode.
    /// Returns whether the avoid list actually changed (so the caller can skip a no-op save).
    pub fn apply_feedback(&mut self, down_artists: &[String], boost_artists: &[String]) -> bool {
        let before = self.avoid_artist_keys.clone();
        for k in normalize_keys(down_artists) {
            if !self.avoid_artist_keys.contains(&k) {
                self.avoid_artist_keys.push(k);
            }
        }
        let boosted = normalize_keys(boost_artists);
        self.avoid_artist_keys.retain(|k| !boosted.contains(k));
        self.avoid_artist_keys != before
    }
}

/// Persisted holder for the single active station, mirroring [`crate::playlists::Playlists`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StationStore {
    /// The active station, if one was started from a vibe (None -> plain seed-based streaming).
    pub active: Option<StationProfile>,
}

impl StationStore {
    /// Load from disk, falling back to empty if absent or unreadable.
    pub fn load() -> Self {
        let Some(path) = station_path() else {
            return StationStore::default();
        };
        // Schema-drift tolerant: preserves the active station across incompatible changes.
        // Size-capped like the sibling Playlists load.
        const MAX_BYTES: u64 = 16 * 1024 * 1024;
        safe_fs::load_json_or_default_limited::<StationStore>(&path, MAX_BYTES)
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = station_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    /// The active station's avoid list as engine artist keys (empty when no station is set).
    pub fn avoid_artist_keys(&self) -> Vec<String> {
        self.active
            .as_ref()
            .map(|p| p.avoid_artist_keys.clone())
            .unwrap_or_default()
    }
}

/// Normalize + de-dupe raw artist names into engine artist keys, dropping blanks.
fn normalize_keys(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let key = crate::signals::normalize_artist(n);
        if !key.is_empty() && !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

fn station_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.data_dir().join("station.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explore_maps_to_engine_modes() {
        assert_eq!(Explore::Tight.to_mode(), StreamingMode::Focused);
        assert_eq!(Explore::Balanced.to_mode(), StreamingMode::Balanced);
        assert_eq!(Explore::Wide.to_mode(), StreamingMode::Discovery);
    }

    #[test]
    fn explore_parse_is_tolerant_and_defaults_to_balanced() {
        assert_eq!(Explore::parse("tight"), Explore::Tight);
        assert_eq!(Explore::parse("  Focused "), Explore::Tight);
        assert_eq!(Explore::parse("WIDE"), Explore::Wide);
        assert_eq!(Explore::parse("adventurous"), Explore::Wide);
        assert_eq!(Explore::parse("balanced"), Explore::Balanced);
        assert_eq!(Explore::parse("nonsense"), Explore::Balanced);
    }

    #[test]
    fn from_intent_normalizes_and_dedupes_avoid_artists() {
        let _guard = crate::i18n::lock_for_test();
        let p = StationProfile::from_intent(
            "  late night drive  ",
            Some("wide"),
            &[
                "The Beatles".to_owned(),
                "the  beatles".to_owned(),
                "  ".to_owned(),
            ],
        );
        assert_eq!(p.query, "late night drive");
        assert_eq!(p.explore, Explore::Wide);
        // Case/space-collapsed to one key; the blank is dropped.
        assert_eq!(
            p.avoid_artist_keys,
            vec![crate::signals::normalize_artist("The Beatles")]
        );
    }

    #[test]
    fn from_intent_without_explore_defaults_to_balanced() {
        let p = StationProfile::from_intent("anything", None, &[]);
        assert_eq!(p.explore, Explore::Balanced);
        assert!(p.avoid_artist_keys.is_empty());
    }

    #[test]
    fn apply_feedback_adds_down_removes_boost_and_reports_change() {
        let _guard = crate::i18n::lock_for_test();
        let mut p = StationProfile {
            query: "q".to_owned(),
            explore: Explore::Balanced,
            avoid_artist_keys: vec![crate::signals::normalize_artist("Already Avoided")],
        };
        // Add a down-voted artist (deduped/normalized), and un-avoid the boosted one.
        let changed = p.apply_feedback(
            &["Nickelback".to_owned(), "nickelback".to_owned()],
            &["Already Avoided".to_owned()],
        );
        assert!(changed, "the avoid list changed");
        assert_eq!(
            p.avoid_artist_keys,
            vec![crate::signals::normalize_artist("Nickelback")]
        );
        // It never touches the explore level (no surprise mode flips).
        assert_eq!(p.explore, Explore::Balanced);
        // A no-op pass reports no change.
        assert!(!p.apply_feedback(&[], &["someone not avoided".to_owned()]));
    }

    #[test]
    fn store_round_trips_and_exposes_avoid_keys() {
        let store = StationStore {
            active: Some(StationProfile {
                query: "rainy day".to_owned(),
                explore: Explore::Tight,
                avoid_artist_keys: vec!["nickelback".to_owned()],
            }),
        };
        let json = serde_json::to_string(&store).unwrap();
        let back: StationStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.avoid_artist_keys(), vec!["nickelback".to_owned()]);
        assert_eq!(back.active.unwrap().explore, Explore::Tight);
        // An empty store yields no avoid keys, and a bare object fills from defaults.
        assert!(StationStore::default().avoid_artist_keys().is_empty());
        let bare: StationStore = serde_json::from_str("{}").unwrap();
        assert!(bare.active.is_none());
    }
}
