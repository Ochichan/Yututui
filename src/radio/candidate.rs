//! A scored radio candidate: a playable [`Song`] plus provenance and the metadata the pure
//! ranking core needs. Candidates are built from whatever source produced them (today only
//! the anonymous yt-dlp radio search; authenticated sources land in a later stage).

use crate::api::Song;
use crate::radio::canonical;
use crate::signals;

/// Where a candidate came from. Provenance is a ranking prior — the real YTM radio
/// continuation is trusted most, a blind text search least.
///
/// Only `YtdlpRadio` is produced today; the authenticated/local sources are wired by the
/// candidate-fetch stage (v3). The `provenance_weight` table already ranks them all, so the
/// variants are intentionally defined ahead of their producers.
#[allow(dead_code, reason = "ArtistTop/MoodPlaylist/HistoryCooc/LikedNeighbor not yet sourced")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateSource {
    /// The real YTM radio continuation (`get_watch_playlist_from_video_id`). Strongest.
    WatchPlaylist,
    /// An artist's top songs (rich metadata: plays/album/explicit).
    ArtistTop,
    /// A mood/genre playlist (carries a tag).
    MoodPlaylist,
    /// Anonymous yt-dlp text search ("… radio/mix/similar"). Weakest.
    YtdlpRadio,
    /// A neighbor from the local co-occurrence graph.
    HistoryCooc,
    /// A neighbor derived from favorites.
    LikedNeighbor,
}

impl CandidateSource {
    /// A [0,1] prior on how much to trust this source's ordering.
    pub fn provenance_weight(self) -> f32 {
        match self {
            CandidateSource::WatchPlaylist => 1.00,
            CandidateSource::ArtistTop => 0.80,
            CandidateSource::MoodPlaylist => 0.65,
            CandidateSource::LikedNeighbor => 0.60,
            CandidateSource::HistoryCooc => 0.55,
            CandidateSource::YtdlpRadio => 0.45,
        }
    }
}

/// A ranking candidate. `base_score`/`novelty` are filled in by the scoring pass; the rest
/// is set at construction.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub song: Song,
    pub source: CandidateSource,
    /// Position within its source's list (0 = first); drives the continuation prior.
    pub source_rank: usize,
    /// Normalized artist key (matches [`signals::normalize_artist`]).
    pub artist_key: String,
    /// Normalized `title+artist` key for dedup / similarity.
    pub canonical_key: String,
    pub album: Option<String>,
    // Rich metadata only the authenticated sources provide; populated + read by the scoring
    // pass in v3. Defined now so the candidate shape is stable across stages.
    #[allow(dead_code, reason = "populated and scored once authenticated sources land (v3)")]
    pub plays: Option<u64>,
    #[allow(dead_code, reason = "populated and scored once authenticated sources land (v3)")]
    pub explicit: Option<bool>,
    #[allow(dead_code, reason = "populated and scored once authenticated sources land (v3)")]
    pub mood_tag: Option<String>,
    pub duration_secs: Option<u32>,
    pub base_score: f32,
    pub novelty: f32,
}

impl Candidate {
    /// Build a candidate from a bare [`Song`] (the anonymous-search case: no album/plays/
    /// explicit metadata available). Richer constructors land with the authenticated sources.
    pub fn from_song(song: Song, source: CandidateSource, source_rank: usize) -> Self {
        let artist_key = signals::normalize_artist(&song.artist);
        let canonical_key = canonical::canonical_key(&song.title, &song.artist);
        let duration_secs = parse_duration_secs(&song.duration);
        Self {
            song,
            source,
            source_rank,
            artist_key,
            canonical_key,
            album: None,
            plays: None,
            explicit: None,
            mood_tag: None,
            duration_secs,
            base_score: 0.0,
            novelty: 0.0,
        }
    }

    pub fn video_id(&self) -> &str {
        &self.song.video_id
    }
}

/// Parse a `"M:SS"` / `"H:MM:SS"` duration string into seconds. Returns `None` for empty or
/// malformed input (e.g. a live "•").
pub fn parse_duration_secs(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: u32 = 0;
    for part in s.split(':') {
        let n: u32 = part.trim().parse().ok()?;
        total = total.checked_mul(60)?.checked_add(n)?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, title: &str, artist: &str, dur: &str) -> Song {
        Song::remote(id, title, artist, dur)
    }

    #[test]
    fn from_song_derives_keys_and_duration() {
        let c = Candidate::from_song(
            song("v1", "My Song (Live)", "The Band", "3:45"),
            CandidateSource::YtdlpRadio,
            2,
        );
        assert_eq!(c.artist_key, "the band");
        assert_eq!(c.duration_secs, Some(225));
        assert_eq!(c.source_rank, 2);
        // The canonical key ignores the "(Live)" qualifier.
        assert_eq!(
            c.canonical_key,
            canonical::canonical_key("My Song", "The Band")
        );
    }

    #[test]
    fn provenance_ranks_watch_playlist_over_text_search() {
        assert!(
            CandidateSource::WatchPlaylist.provenance_weight()
                > CandidateSource::YtdlpRadio.provenance_weight()
        );
    }

    #[test]
    fn duration_parsing_handles_minutes_and_hours() {
        assert_eq!(parse_duration_secs("0:30"), Some(30));
        assert_eq!(parse_duration_secs("3:45"), Some(225));
        assert_eq!(parse_duration_secs("1:02:03"), Some(3723));
        assert_eq!(parse_duration_secs(""), None);
        assert_eq!(parse_duration_secs("--:--"), None);
    }
}
