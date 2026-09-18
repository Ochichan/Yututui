use std::collections::HashSet;

use crate::api::Song;
use crate::signals::normalize_artist;
use crate::streaming::candidate::Candidate;

pub const SEED_TERMS_MAX: usize = 12;

const SEED_TERM_CHARS_MAX: usize = 48;

pub const SEED_BIAS_WEIGHT: f32 = 0.22;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackId(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtistKey(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeedTerm(String);

impl TrackId {
    pub fn new(raw: &str) -> Option<Self> {
        let id = raw.trim();
        if id.is_empty() {
            return None;
        }
        Some(Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ArtistKey {
    pub fn from_song(song: &Song) -> Option<Self> {
        Self::from_raw(&song.artist)
    }

    fn from_raw(raw: &str) -> Option<Self> {
        let key = normalize_artist(raw);
        if key.is_empty() {
            return None;
        }
        Some(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl SeedTerm {
    pub fn new(raw: &str) -> Option<Self> {
        let collapsed = raw
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if collapsed.is_empty() || collapsed.chars().count() > SEED_TERM_CHARS_MAX {
            return None;
        }
        Some(Self(collapsed))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn matches(&self, haystack: &str) -> bool {
        haystack.to_lowercase().contains(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TasteError {
    MissingTrackId,
    MissingArtist,
    EmptyTerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedPolarity {
    MoreLike,
    Exclude,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannedTrack {
    pub id: TrackId,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannedArtist {
    pub key: ArtistKey,
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub term: SeedTerm,
    pub polarity: SeedPolarity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionTaste {
    banned_tracks: Vec<BannedTrack>,
    banned_artists: Vec<BannedArtist>,
    seeds: Vec<Seed>,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TasteEdit {
    BanTrack(BannedTrack),
    BanArtist(BannedArtist),
    SetSeed(Seed),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum TasteOutcome {
    Applied,
    AlreadyApplied,
    SeedLimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TasteEntry<'a> {
    BannedTrack(&'a BannedTrack),
    BannedArtist(&'a BannedArtist),
    Seed(&'a Seed),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TasteCounts {
    pub banned: usize,
    pub seeds: usize,
}

impl TasteCounts {
    pub fn is_empty(self) -> bool {
        self.banned == 0 && self.seeds == 0
    }
}

fn track_label(song: &Song) -> String {
    format!("{} — {}", song.title, song.artist)
}

impl TasteEdit {
    pub fn ban_track(song: &Song) -> Result<Self, TasteError> {
        let id = TrackId::new(&song.video_id).ok_or(TasteError::MissingTrackId)?;
        Ok(Self::BanTrack(BannedTrack {
            id,
            label: track_label(song),
        }))
    }

    pub fn ban_artist(song: &Song) -> Result<Self, TasteError> {
        let key = ArtistKey::from_song(song).ok_or(TasteError::MissingArtist)?;
        Ok(Self::BanArtist(BannedArtist {
            key,
            display: song.artist.clone(),
        }))
    }

    pub fn seed_current_artist(song: &Song) -> Result<Self, TasteError> {
        let term = SeedTerm::new(&song.artist).ok_or(TasteError::MissingArtist)?;
        Ok(Self::SetSeed(Seed {
            term,
            polarity: SeedPolarity::MoreLike,
        }))
    }

    pub fn parse_seed(raw: &str) -> Result<Self, TasteError> {
        let trimmed = raw.trim();
        let (polarity, rest) = match trimmed.strip_prefix('-') {
            Some(rest) => (SeedPolarity::Exclude, rest),
            None => (SeedPolarity::MoreLike, trimmed),
        };
        let term = SeedTerm::new(rest).ok_or(TasteError::EmptyTerm)?;
        Ok(Self::SetSeed(Seed { term, polarity }))
    }

    pub fn rejects(&self, song: &Song) -> bool {
        match self {
            Self::BanTrack(banned) => TrackId::new(&song.video_id).as_ref() == Some(&banned.id),
            Self::BanArtist(banned) => ArtistKey::from_song(song).as_ref() == Some(&banned.key),
            Self::SetSeed(_) => false,
        }
    }
}

impl SessionTaste {
    fn bump_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    pub fn apply(&mut self, edit: TasteEdit) -> TasteOutcome {
        match edit {
            TasteEdit::BanTrack(banned) => {
                if self.banned_tracks.iter().any(|row| row.id == banned.id) {
                    return TasteOutcome::AlreadyApplied;
                }
                self.banned_tracks.push(banned);
                self.bump_epoch();
                TasteOutcome::Applied
            }
            TasteEdit::BanArtist(banned) => {
                if self.banned_artists.iter().any(|row| row.key == banned.key) {
                    return TasteOutcome::AlreadyApplied;
                }
                self.banned_artists.push(banned);
                self.bump_epoch();
                TasteOutcome::Applied
            }
            TasteEdit::SetSeed(seed) => {
                if let Some(existing) = self.seeds.iter_mut().find(|row| row.term == seed.term) {
                    if existing.polarity == seed.polarity {
                        return TasteOutcome::AlreadyApplied;
                    }
                    existing.polarity = seed.polarity;
                    self.bump_epoch();
                    return TasteOutcome::Applied;
                }
                if self.seeds.len() >= SEED_TERMS_MAX {
                    return TasteOutcome::SeedLimitReached;
                }
                self.seeds.push(seed);
                self.bump_epoch();
                TasteOutcome::Applied
            }
        }
    }

    pub fn rejects_song(&self, song: &Song) -> bool {
        if let Some(id) = TrackId::new(&song.video_id)
            && self.banned_tracks.iter().any(|row| row.id == id)
        {
            return true;
        }
        if let Some(key) = ArtistKey::from_song(song)
            && self.banned_artists.iter().any(|row| row.key == key)
        {
            return true;
        }
        false
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = TasteEntry<'_>> {
        let mut rows = Vec::with_capacity(
            self.banned_tracks.len() + self.banned_artists.len() + self.seeds.len(),
        );
        rows.extend(self.banned_tracks.iter().map(TasteEntry::BannedTrack));
        rows.extend(self.banned_artists.iter().map(TasteEntry::BannedArtist));
        rows.extend(self.seeds.iter().map(TasteEntry::Seed));
        rows.into_iter()
    }

    pub fn forget_at(&mut self, index: usize) -> bool {
        if index < self.banned_tracks.len() {
            self.banned_tracks.remove(index);
            self.bump_epoch();
            return true;
        }
        let artist_index = index - self.banned_tracks.len();
        if artist_index < self.banned_artists.len() {
            self.banned_artists.remove(artist_index);
            self.bump_epoch();
            return true;
        }
        let seed_index = artist_index - self.banned_artists.len();
        if seed_index < self.seeds.len() {
            self.seeds.remove(seed_index);
            self.bump_epoch();
            return true;
        }
        false
    }

    pub fn counts(&self) -> TasteCounts {
        TasteCounts {
            banned: self.banned_tracks.len() + self.banned_artists.len(),
            seeds: self.seeds.len(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn banned_track_ids(&self) -> impl Iterator<Item = &str> {
        self.banned_tracks.iter().map(|row| row.id.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeedBias {
    terms: Vec<Seed>,
}

impl SeedBias {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn score(&self, candidate: &Candidate) -> f32 {
        if self.terms.is_empty() {
            return 0.0;
        }
        let title = candidate.song.title.as_str();
        let artist = candidate.song.artist.as_str();
        let album = candidate
            .album
            .as_deref()
            .or(candidate.song.album.as_deref())
            .unwrap_or("");
        let mut net = 0.0_f32;
        for seed in &self.terms {
            let hit = seed.term.matches(title)
                || seed.term.matches(artist)
                || (!album.is_empty() && seed.term.matches(album));
            if !hit {
                continue;
            }
            net += match seed.polarity {
                SeedPolarity::MoreLike => 1.0,
                SeedPolarity::Exclude => -1.0,
            };
        }
        (net / 2.0).clamp(-1.0, 1.0)
    }
}

pub struct TasteProjection {
    pub banned_track_ids: HashSet<String>,
    pub banned_artist_keys: HashSet<String>,
    pub seed_bias: SeedBias,
}

pub fn project_taste(taste: &SessionTaste, avoid_artist_keys: &[String]) -> TasteProjection {
    let banned_track_ids = taste
        .banned_tracks
        .iter()
        .map(|row| row.id.as_str().to_owned())
        .collect();
    let mut banned_artist_keys: HashSet<String> = taste
        .banned_artists
        .iter()
        .map(|row| row.key.as_str().to_owned())
        .collect();
    for key in avoid_artist_keys {
        if key.is_empty() {
            continue;
        }
        banned_artist_keys.insert(key.clone());
    }
    TasteProjection {
        banned_track_ids,
        banned_artist_keys,
        seed_bias: SeedBias {
            terms: taste.seeds.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::candidate::CandidateSource;

    fn song(id: &str, title: &str, artist: &str) -> Song {
        Song::remote(id, title, artist, "3:00")
    }

    fn cand(id: &str, title: &str, artist: &str) -> Candidate {
        Candidate::from_song(song(id, title, artist), CandidateSource::YtdlpStreaming, 0)
    }

    #[test]
    fn apply_is_idempotent_for_the_same_ban() {
        let track = song("vid1", "Night", "Slowdive");
        let edit = TasteEdit::ban_track(&track).expect("usable id");
        let mut taste = SessionTaste::default();
        assert_eq!(taste.apply(edit.clone()), TasteOutcome::Applied);
        assert_eq!(taste.apply(edit), TasteOutcome::AlreadyApplied);
        assert_eq!(
            taste.counts(),
            TasteCounts {
                banned: 1,
                seeds: 0
            }
        );
        assert_eq!(taste.entries().len(), 1);
    }

    #[test]
    fn empty_artist_is_refused_at_construction() {
        let untagged = song("vid2", "Untitled", "   ");
        assert_eq!(
            TasteEdit::ban_artist(&untagged),
            Err(TasteError::MissingArtist)
        );
        assert_eq!(ArtistKey::from_song(&untagged), None);
        let taste = SessionTaste::default();
        assert!(!taste.rejects_song(&untagged));
    }

    #[test]
    fn empty_track_id_is_refused_at_construction() {
        let mut missing = song("keep", "Ghost", "Band");
        missing.video_id.clear();
        assert_eq!(
            TasteEdit::ban_track(&missing),
            Err(TasteError::MissingTrackId)
        );
        assert_eq!(TrackId::new(""), None);
        assert_eq!(TrackId::new("   "), None);
    }

    #[test]
    fn polarity_flip_keeps_the_term_in_place() {
        let mut taste = SessionTaste::default();
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("jazz").expect("term")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("rock").expect("term")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("-jazz").expect("term")),
            TasteOutcome::Applied
        );
        let entries: Vec<_> = taste.entries().collect();
        match entries.as_slice() {
            [TasteEntry::Seed(first), TasteEntry::Seed(second)] => {
                assert_eq!(first.term.as_str(), "jazz");
                assert_eq!(first.polarity, SeedPolarity::Exclude);
                assert_eq!(second.term.as_str(), "rock");
                assert_eq!(second.polarity, SeedPolarity::MoreLike);
            }
            other => panic!("expected two seeds oldest-first, got {other:?}"),
        }
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("-jazz").expect("term")),
            TasteOutcome::AlreadyApplied
        );
    }

    #[test]
    fn seed_cap_refuses_without_mutating() {
        let mut taste = SessionTaste::default();
        for i in 0..SEED_TERMS_MAX {
            let edit = TasteEdit::parse_seed(&format!("term{i}")).expect("term");
            assert_eq!(taste.apply(edit), TasteOutcome::Applied);
        }
        let before = taste.clone();
        let overflow = TasteEdit::parse_seed("overflow").expect("term");
        assert_eq!(taste.apply(overflow), TasteOutcome::SeedLimitReached);
        assert_eq!(taste, before);
        assert_eq!(taste.counts().seeds, SEED_TERMS_MAX);
        let flip = TasteEdit::parse_seed("-term0").expect("term");
        assert_eq!(taste.apply(flip), TasteOutcome::Applied);
        assert_eq!(taste.counts().seeds, SEED_TERMS_MAX);
    }

    #[test]
    fn counts_match_entries_len() {
        let mut taste = SessionTaste::default();
        let track = song("t1", "Song", "Artist One");
        let other = song("t2", "Other", "Artist Two");
        assert_eq!(
            taste.apply(TasteEdit::ban_track(&track).expect("id")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::ban_artist(&other).expect("artist")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("dream pop").expect("term")),
            TasteOutcome::Applied
        );
        let counts = taste.counts();
        assert_eq!(counts.banned, 2);
        assert_eq!(counts.seeds, 1);
        assert_eq!(counts.banned + counts.seeds, taste.entries().len());
        assert!(!counts.is_empty());
    }

    #[test]
    fn rejects_song_matches_track_and_artist_bans_only() {
        let banned_track = song("nope", "Skip Me", "Keep Artist");
        let same_artist = song("ok-id", "Another", "Keep Artist");
        let other = song("fine", "Fine", "Someone Else");
        let mut taste = SessionTaste::default();
        assert_eq!(
            taste.apply(TasteEdit::ban_track(&banned_track).expect("id")),
            TasteOutcome::Applied
        );
        assert!(taste.rejects_song(&banned_track));
        assert!(!taste.rejects_song(&same_artist));
        assert!(!taste.rejects_song(&other));
        assert_eq!(
            taste.apply(TasteEdit::ban_artist(&same_artist).expect("artist")),
            TasteOutcome::Applied
        );
        assert!(taste.rejects_song(&same_artist));
        assert!(!taste.rejects_song(&other));
        let seed = TasteEdit::parse_seed("fine").expect("term");
        assert!(!seed.rejects(&other));
        assert_eq!(taste.apply(seed), TasteOutcome::Applied);
        assert!(!taste.rejects_song(&other));
    }

    #[test]
    fn parse_seed_rejects_empty_and_strips_one_leading_dash() {
        assert_eq!(TasteEdit::parse_seed("   "), Err(TasteError::EmptyTerm));
        assert_eq!(TasteEdit::parse_seed("-"), Err(TasteError::EmptyTerm));
        assert_eq!(TasteEdit::parse_seed("-   "), Err(TasteError::EmptyTerm));
        let too_long = "x".repeat(SEED_TERM_CHARS_MAX + 1);
        assert_eq!(TasteEdit::parse_seed(&too_long), Err(TasteError::EmptyTerm));
        match TasteEdit::parse_seed("- Dream Pop ").expect("term") {
            TasteEdit::SetSeed(seed) => {
                assert_eq!(seed.term.as_str(), "dream pop");
                assert_eq!(seed.polarity, SeedPolarity::Exclude);
            }
            other => panic!("expected a seed, got {other:?}"),
        }
    }

    #[test]
    fn seed_bias_promotes_and_demotes_without_rejecting() {
        let more = Seed {
            term: SeedTerm::new("jazz").expect("term"),
            polarity: SeedPolarity::MoreLike,
        };
        let exclude = Seed {
            term: SeedTerm::new("jazz").expect("term"),
            polarity: SeedPolarity::Exclude,
        };
        let hit = cand("a", "Late Jazz Night", "Trio");
        let miss = cand("b", "Rock Anthem", "Band");
        let more_bias = SeedBias { terms: vec![more] };
        let exclude_bias = SeedBias {
            terms: vec![exclude],
        };
        assert_eq!(more_bias.score(&hit), 0.5);
        assert_eq!(more_bias.score(&miss), 0.0);
        assert_eq!(exclude_bias.score(&hit), -0.5);
        assert_eq!(exclude_bias.score(&miss), 0.0);
        let two = SeedBias {
            terms: vec![
                Seed {
                    term: SeedTerm::new("jazz").expect("term"),
                    polarity: SeedPolarity::MoreLike,
                },
                Seed {
                    term: SeedTerm::new("night").expect("term"),
                    polarity: SeedPolarity::MoreLike,
                },
            ],
        };
        assert_eq!(two.score(&hit), 1.0);
    }

    #[test]
    fn project_taste_unions_session_bans_and_brands_empty_avoid_keys_out() {
        let mut taste = SessionTaste::default();
        let track = song("banned-id", "Gone", "Session Band");
        let artist = song("other", "Stay", "Banned Act");
        assert_eq!(
            taste.apply(TasteEdit::ban_track(&track).expect("id")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::ban_artist(&artist).expect("artist")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("ambient").expect("term")),
            TasteOutcome::Applied
        );
        let projected = project_taste(&taste, &["durable-act".to_owned(), String::new()]);
        assert!(projected.banned_track_ids.contains("banned-id"));
        assert!(projected.banned_artist_keys.contains("banned act"));
        assert!(projected.banned_artist_keys.contains("durable-act"));
        assert!(!projected.banned_artist_keys.contains(""));
        assert!(!projected.seed_bias.is_empty());
    }

    #[test]
    fn forget_at_lifts_by_entries_order_and_ignores_stale_indexes() {
        let mut taste = SessionTaste::default();
        assert_eq!(
            taste.apply(TasteEdit::ban_track(&song("t", "T", "A")).expect("id")),
            TasteOutcome::Applied
        );
        assert_eq!(
            taste.apply(TasteEdit::parse_seed("tag").expect("term")),
            TasteOutcome::Applied
        );
        assert!(taste.forget_at(0));
        assert_eq!(taste.counts().banned, 0);
        assert_eq!(taste.counts().seeds, 1);
        assert!(!taste.forget_at(9));
        assert_eq!(taste.counts().seeds, 1);
        assert!(taste.forget_at(0));
        assert!(taste.counts().is_empty());
    }
}
