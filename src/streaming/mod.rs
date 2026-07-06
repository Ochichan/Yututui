//! The local streaming engine: turn a pool of playable candidates into the next few tracks
//! using only locally-computable signals — co-occurrence, learned affinity, novelty,
//! provenance, completion — plus MMR diversity, artist/album cooldown, and softmax sampling.
//!
//! Everything here is **pure and synchronous** (no network, no clock beyond an injected
//! `now`, RNG only via the global `fastrand`), so it is fully unit-tested. The async
//! candidate *retrieval* lives in the API layer; this module just ranks what it's handed.

pub mod candidate;
mod canonical;
pub mod config;
mod cooccurrence;
pub(crate) mod musicgate;
pub mod pack;
mod rerank;
mod score;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::api::Song;
use crate::library::Library;
use crate::queue::Queue;
use crate::signals::Signals;
use crate::streaming::musicgate::GateAction;

pub use candidate::{Candidate, CandidateSource};
pub use config::{CuratingMode, ModeProfile, StreamingConfig, StreamingMode};
pub use cooccurrence::Cooc;
pub use pack::PackedCand;
pub use score::{GateVerdict, classify_pool};

/// Build the "recently heard" context the station scorer filters against: recent track ids
/// (the current queue, then a slice of history) and recent artist keys (the history cooldown
/// window, then the current + upcoming queue artists). Single-sourced here so the interactive
/// App reducer and the headless daemon engine can't drift — the same discipline as
/// [`exclude_ids`] and [`crate::playback_policy`]. Returns `(recent_track_ids, recent_artist_keys)`.
pub fn station_recent_context(
    queue: &Queue,
    library: &Library,
    profile: &ModeProfile,
) -> (Vec<String>, Vec<String>) {
    let mut recent_track_ids: Vec<String> = queue
        .ordered_iter()
        .filter(|song| !song.is_radio_station())
        .map(|song| song.video_id.clone())
        .collect();
    recent_track_ids.extend(
        library
            .history
            .iter()
            .filter(|s| !s.is_radio_station())
            .take(profile.history_block_horizon)
            .map(|s| s.video_id.clone()),
    );

    // Cooldown window wants most-recent *last*. History is newest-first, so reverse it, then
    // append current/upcoming queue artists so newly appended picks don't repeat them.
    let mut recent_artist_keys: Vec<String> = library
        .history
        .iter()
        .filter(|s| !s.is_radio_station())
        .take(crate::playback_policy::STREAMING_RECENT_ARTISTS)
        .map(|s| crate::signals::normalize_artist(&s.artist))
        .collect();
    recent_artist_keys.reverse();
    if let Some(cur) = queue.current()
        && !cur.is_radio_station()
    {
        push_artist_key(&mut recent_artist_keys, &cur.artist);
    }
    for song in queue
        .ordered_iter()
        .skip(queue.cursor_pos().saturating_add(1))
        .filter(|song| !song.is_radio_station())
        .take(8)
    {
        push_artist_key(&mut recent_artist_keys, &song.artist);
    }
    (recent_track_ids, recent_artist_keys)
}

fn push_artist_key(keys: &mut Vec<String>, artist: &str) {
    let key = crate::signals::normalize_artist(artist);
    if !key.is_empty() {
        keys.push(key);
    }
}

/// The set of `video_id`s to exclude from an autoplay streaming top-up: everything already
/// queued, the seed itself, and recently-played history within the mode's block horizon
/// (old favourites may be allowed back per `allow_old_liked_repeats`). Extracted so the
/// interactive App reducer and the headless daemon engine share ONE implementation and
/// cannot drift on which repeats they admit. Radio-station pseudo-entries are never excluded.
pub fn exclude_ids(
    streaming: &StreamingConfig,
    queue: &Queue,
    library: &Library,
    seed_video_id: &str,
) -> Vec<String> {
    let profile = streaming.mode.profile(streaming);
    let mut ids: HashSet<String> = queue
        .ordered_iter()
        .filter(|song| !song.is_radio_station())
        .map(|song| song.video_id.clone())
        .collect();
    ids.insert(seed_video_id.to_owned());
    let favorite_ids: HashSet<&str> = library
        .favorites
        .iter()
        .filter(|song| !song.is_radio_station())
        .map(|s| s.video_id.as_str())
        .collect();
    for (idx, song) in library
        .history
        .iter()
        .filter(|song| !song.is_radio_station())
        .enumerate()
    {
        let inside_horizon = idx < profile.history_block_horizon;
        let protected_old_favorite =
            profile.allow_old_liked_repeats && favorite_ids.contains(song.video_id.as_str());
        if inside_horizon || !protected_old_favorite {
            ids.insert(song.video_id.clone());
        }
    }
    ids.into_iter().collect()
}

/// The live station context the ranking reads: the seed, what was recently heard (for
/// "already played" filtering, co-occurrence context, and cooldown), and the user's
/// blocks/favorites.
#[derive(Debug, Clone)]
pub struct StationState {
    pub mode: StreamingMode,
    pub seed_video_id: String,
    pub seed_artist_key: String,
    /// Recently-played track ids (queue + history tail), most-recent last.
    pub recent_track_ids: Vec<String>,
    /// Recently-played artist keys (for the cooldown window), most-recent last.
    pub recent_artist_keys: Vec<String>,
    pub banned_track_ids: HashSet<String>,
    pub banned_artist_keys: HashSet<String>,
    /// Normalized artist keys the user has favorited (a seed-affinity boost).
    pub favorite_artist_keys: HashSet<String>,
    /// Short-lived per-session artist nudges derived from recent streaming outcomes.
    pub session_artist_bias: HashMap<String, f32>,
    /// Temporary novelty/familiarity nudges for skip-streak recovery.
    pub temporary_novelty_boost: f32,
    pub temporary_familiarity_boost: f32,
}

/// Run the full local pipeline — hard filter → normalized base score → MMR + cooldown →
/// softmax sample — and return up to `n` picks. The guaranteed-available streaming path; also
/// the fallback when a DJ Gem rerank is unavailable.
pub fn plan_local(
    pool: Vec<Candidate>,
    st: &StationState,
    sig: &Signals,
    cooc: &Cooc,
    cfg: &StreamingConfig,
    n: usize,
    now: i64,
) -> Vec<Song> {
    let scored = score::filter_and_score(pool, st, sig, cooc, cfg, now);
    rerank::select(scored, st, cooc, cfg, n)
}

/// The diverse top-`k` candidates to hand the DJ Gem reranker — same hard-filter + score pipeline
/// as [`plan_local`], but stopping at a deterministic MMR shortlist (no softmax). The DJ Gem picks
/// from these by id; it never sees a track that isn't already a playable local candidate.
pub fn shortlist_for_ai(
    pool: Vec<Candidate>,
    st: &StationState,
    sig: &Signals,
    cooc: &Cooc,
    cfg: &StreamingConfig,
    k: usize,
    now: i64,
) -> Vec<Candidate> {
    let scored = score::filter_and_score(pool, st, sig, cooc, cfg, now);
    rerank::mmr_topk(scored, st, cooc, cfg, k)
}

/// Turn the DJ Gem reranker's chosen ids into the final pick list. Every id is validated against the
/// shortlist the model was shown (so a hallucinated/altered id is silently dropped), de-duped,
/// then any shortfall is topped up from the engine's own `local_pick` order. The result is
/// always a subset of real, playable candidates — DJ Gem failure degrades to pure-local with no
/// user-visible breakage.
pub fn merge_ai_picks(
    ids: &[String],
    shortlist: &[Song],
    local_pick: &[Song],
    n: usize,
) -> Vec<Song> {
    let valid: HashMap<&str, &Song> = shortlist.iter().map(|s| (s.video_id.as_str(), s)).collect();
    let mut out: Vec<Song> = Vec::new();
    let mut taken: HashSet<String> = HashSet::new();
    for id in ids {
        if out.len() >= n {
            break;
        }
        if let Some(&song) = valid.get(id.as_str())
            && taken.insert(song.video_id.clone())
        {
            out.push(song.clone());
        }
    }
    // Backfill from the engine's own ranked picks for anything the DJ Gem dropped/omitted.
    for song in local_pick {
        if out.len() >= n {
            break;
        }
        if taken.insert(song.video_id.clone()) {
            out.push(song.clone());
        }
    }
    out
}

/// Confidence-aware version of [`merge_ai_picks`]. Low-confidence DJ Gem output is treated as a
/// light nudge over the local engine instead of a full ordering replacement.
pub fn merge_ai_picks_with_confidence(
    ids: &[String],
    shortlist: &[Song],
    local_pick: &[Song],
    n: usize,
    conf: Option<f32>,
) -> Vec<Song> {
    let ai_slots = match conf {
        Some(c) if c < 0.45 => n.div_ceil(3),
        Some(c) if c < 0.75 => n.div_ceil(2),
        _ => n,
    };
    let ai_first = merge_ai_picks(ids, shortlist, local_pick, ai_slots);
    if ai_first.len() >= n {
        return ai_first;
    }
    let ai_ids: Vec<String> = ai_first.iter().map(|s| s.video_id.clone()).collect();
    merge_ai_picks(&ai_ids, shortlist, local_pick, n)
}

/// Last synchronous safety pass before streaming picks are appended to the queue. The scoring pass
/// already filtered candidates, but cached DJ Gem orders and low-context fallbacks can still benefit
/// from a final cheap title/channel/duration check.
pub fn sanitize_final_picks(
    picks: Vec<Song>,
    fallback: &[Song],
    mode: StreamingMode,
    cfg: &StreamingConfig,
) -> Vec<Song> {
    let target = picks.len();
    let mut out = Vec::with_capacity(target);
    let mut taken: HashSet<String> = HashSet::new();
    for song in picks.iter().chain(fallback.iter()) {
        if out.len() >= target {
            break;
        }
        if taken.contains(&song.video_id) || reject_final_song(song, mode, cfg) {
            continue;
        }
        taken.insert(song.video_id.clone());
        out.push(song.clone());
    }
    out
}

fn reject_final_song(song: &Song, mode: StreamingMode, cfg: &StreamingConfig) -> bool {
    if let Some(duration) = candidate::parse_duration_secs(&song.duration)
        && cfg.duration_out_of_bounds(duration)
    {
        return true;
    }
    if !cfg.gate.enabled {
        return false;
    }
    let decision = musicgate::decide(
        &song.title,
        &song.artist,
        CandidateSource::YtdlpStreaming,
        mode,
    );
    if decision.action == GateAction::Reject {
        return true;
    }
    mode == StreamingMode::Focused && musicgate::gimmick_reason(&song.title).is_some()
}

/// Whether the final streaming picks contain a candidate risky enough to justify async metadata
/// preflight before enqueueing. This keeps the common clean path synchronous while still giving
/// public YouTube fallbacks a deeper check when title/channel/duration evidence is weak.
pub fn final_preflight_needed(
    picks: &[Song],
    fallback: &[Song],
    mode: StreamingMode,
    cfg: &StreamingConfig,
) -> bool {
    picks
        .iter()
        .chain(fallback.iter())
        .any(|song| needs_metadata_preflight(song, mode, cfg))
}

/// Cheap title/channel signal used by both the reducer and API actor to decide which final streaming
/// picks need a full yt-dlp metadata extraction.
pub fn needs_metadata_preflight(song: &Song, mode: StreamingMode, cfg: &StreamingConfig) -> bool {
    if !cfg.gate.enabled || song.youtube_id().is_none() || song.is_local() {
        return false;
    }
    let decision = musicgate::decide(
        &song.title,
        &song.artist,
        CandidateSource::YtdlpStreaming,
        mode,
    );
    if decision.action != GateAction::Keep {
        return true;
    }
    if mode == StreamingMode::Focused && musicgate::gimmick_reason(&song.title).is_some() {
        return true;
    }
    let risk = musicgate::non_music_risk_score(&song.title, &song.artist);
    let music_tier = musicgate::music_tier_score(&song.title, &song.artist);
    let duration_unknown = candidate::parse_duration_secs(&song.duration).is_none();
    risk >= 0.35 || (duration_unknown && music_tier <= 0.0 && risk >= 0.15)
}

/// Whether an autoplay refill is worth a DJ Gem rerank call. With `smart_gate` off it's always
/// `true` (spend the call whenever the reranker is enabled). With it on, we still call when the
/// listener is unsettled (a trailing skip streak) or when the local pick is *ambiguous* — the top
/// two candidates' `base_score`s are within `ambiguity_gap`. A clearly-best local pick with a
/// content listener skips the call (and its cost + latency); the local pick is already good, and
/// fewer than two candidates leaves nothing to rerank.
pub fn should_call_ai(
    shortlist: &[Candidate],
    skip_streak: usize,
    cfg: &config::StreamingConfig,
) -> bool {
    if shortlist.len() < 2 {
        return false;
    }
    let profile = cfg.mode.profile(cfg);
    if !cfg.ai.smart_gate || profile.ai_always_call || skip_streak >= 1 {
        return true;
    }
    if cfg.ai.ambiguity_gap >= 0.0 && source_entropy(shortlist) < 0.65 {
        return true;
    }
    match top_two_base_score_gap(shortlist) {
        Some(gap) => gap < cfg.ai.ambiguity_gap,
        None => false,
    }
}

pub struct AiCacheKeyParts<'a> {
    pub seed_artist: &'a str,
    pub mode: StreamingMode,
    pub recent_ids: &'a [String],
    pub candidate_ids: &'a [String],
    pub station_query: &'a str,
    pub avoid_artist_keys: &'a [String],
    pub recovery_line: Option<&'a str>,
    pub skip_streak: usize,
    pub profile_version: u32,
    pub prompt_recipe_hash: u64,
}

/// A stable key for caching a DJ Gem rerank's result. The candidate set is order-independent
/// (sorted before hashing), while station/recovery/profile inputs are included because they
/// change what the same candidates mean.
pub fn ai_cache_key(parts: AiCacheKeyParts<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.seed_artist.hash(&mut h);
    parts.mode.hash(&mut h);
    parts.recent_ids.hash(&mut h);
    parts.station_query.hash(&mut h);
    parts.avoid_artist_keys.hash(&mut h);
    parts.recovery_line.hash(&mut h);
    parts.skip_streak.hash(&mut h);
    parts.profile_version.hash(&mut h);
    parts.prompt_recipe_hash.hash(&mut h);
    let mut sorted: Vec<&String> = parts.candidate_ids.iter().collect();
    sorted.sort();
    sorted.hash(&mut h);
    h.finish()
}

pub fn ai_recipe_hash(recipe: config::AiSlotRecipe) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    recipe.familiar_min.hash(&mut h);
    recipe.bridge_min.hash(&mut h);
    recipe.discovery_min.hash(&mut h);
    recipe.familiar_max.hash(&mut h);
    recipe.discovery_max.hash(&mut h);
    recipe.max_same_artist.hash(&mut h);
    h.finish()
}

pub fn ai_roles_match_recipe(
    roles: &[Option<String>],
    mode: StreamingMode,
    cfg: &StreamingConfig,
) -> bool {
    if roles.is_empty() {
        return false;
    }
    let recipe = mode.profile(cfg).ai_recipe;
    let required = recipe.familiar_min + recipe.bridge_min + recipe.discovery_min;
    if roles.len() < required {
        return true;
    }
    let familiar = roles
        .iter()
        .filter(|r| matches!(r.as_deref(), Some("core" | "stabilizer")))
        .count();
    let bridge = roles
        .iter()
        .filter(|r| matches!(r.as_deref(), Some("bridge" | "adjacent")))
        .count();
    let discovery = roles
        .iter()
        .filter(|r| matches!(r.as_deref(), Some("discovery")))
        .count();
    familiar >= recipe.familiar_min.min(roles.len())
        && bridge >= recipe.bridge_min.min(roles.len())
        && discovery >= recipe.discovery_min.min(roles.len())
        && familiar <= recipe.familiar_max.max(recipe.familiar_min)
        && discovery <= recipe.discovery_max.max(recipe.discovery_min)
}

/// The gap between the two highest `base_score`s in `cands`, or `None` when there are fewer than
/// two candidates (nothing to disambiguate).
fn top_two_base_score_gap(cands: &[Candidate]) -> Option<f32> {
    let (mut top, mut second) = (f32::MIN, f32::MIN);
    let mut seen = 0usize;
    for c in cands {
        seen += 1;
        if c.base_score > top {
            second = top;
            top = c.base_score;
        } else if c.base_score > second {
            second = c.base_score;
        }
    }
    (seen >= 2).then_some(top - second)
}

fn source_entropy(cands: &[Candidate]) -> f32 {
    if cands.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<CandidateSource, usize> = HashMap::new();
    for c in cands {
        *counts.entry(c.source).or_default() += 1;
    }
    if counts.len() <= 1 {
        return 0.0;
    }
    let n = cands.len() as f32;
    let h = counts
        .values()
        .map(|&count| {
            let p = count as f32 / n;
            -p * p.ln()
        })
        .sum::<f32>();
    h / (counts.len() as f32).ln()
}

/// Wrap provenance-tagged songs into ranking candidates. `source_rank` is each song's position
/// *within its own source's list* (not the interleaved order), so the continuation prior
/// reflects how each source itself ranked the track.
pub fn pool_from_tagged(tagged: Vec<(Song, CandidateSource)>) -> Vec<Candidate> {
    let mut ranks: HashMap<CandidateSource, usize> = HashMap::new();
    tagged
        .into_iter()
        .map(|(song, source)| {
            let rank = ranks.entry(source).or_insert(0);
            let cand = Candidate::from_song(song, source, *rank);
            *rank += 1;
            cand
        })
        .collect()
}

/// Wrap raw [`Song`]s from one source into ranking candidates (rank = list position). Test
/// helper; production builds the pool from provenance-tagged candidates via
/// [`pool_from_tagged`].
#[cfg(test)]
pub fn pool_from_songs(songs: Vec<Song>, source: CandidateSource) -> Vec<Candidate> {
    songs
        .into_iter()
        .enumerate()
        .map(|(rank, song)| Candidate::from_song(song, source, rank))
        .collect()
}

#[cfg(test)]
mod exclude_ids_tests {
    use super::*;
    use crate::library::Library;
    use crate::queue::Queue;
    use std::collections::VecDeque;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("t-{id}"), "artist", "3:00")
    }

    #[test]
    fn exclude_ids_covers_queue_seed_and_recent_history() {
        let mut queue = Queue::default();
        queue.set(vec![song("q1"), song("q2")], 0);
        let library = Library {
            favorites: vec![song("fav")],
            history: VecDeque::from(vec![song("h1"), song("h2")]),
            ..Library::default()
        };
        let ids = exclude_ids(&StreamingConfig::default(), &queue, &library, "seed");
        for expected in ["q1", "q2", "seed", "h1", "h2"] {
            assert!(
                ids.iter().any(|i| i == expected),
                "missing {expected}: {ids:?}"
            );
        }
        // The returned set is deduplicated.
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids must be unique");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn song(id: &str, artist: &str) -> Song {
        Song::remote(id, format!("t-{id}"), artist, "3:00")
    }

    fn station(seed: &str) -> StationState {
        StationState {
            mode: StreamingMode::Balanced,
            seed_video_id: seed.to_owned(),
            seed_artist_key: "seed artist".to_owned(),
            recent_track_ids: vec![seed.to_owned()],
            recent_artist_keys: Vec::new(),
            banned_track_ids: HashSet::new(),
            banned_artist_keys: HashSet::new(),
            favorite_artist_keys: HashSet::new(),
            session_artist_bias: HashMap::new(),
            temporary_novelty_boost: 0.0,
            temporary_familiarity_boost: 0.0,
        }
    }

    #[test]
    fn plan_local_filters_and_returns_requested_count() {
        fastrand::seed(42);
        let cfg = StreamingConfig::default();
        let mut st = station("seed");
        st.banned_track_ids.insert("blocked".to_owned());

        // A pool with the seed, a blocked track, and several playable distinct-artist tracks.
        let mut songs = vec![song("seed", "seed artist"), song("blocked", "x")];
        songs.extend((0..12).map(|i| song(&format!("c{i}"), &format!("artist{i}"))));
        let pool = pool_from_songs(songs, CandidateSource::YtdlpStreaming);

        let picks = plan_local(
            pool,
            &st,
            &Signals::default(),
            &Cooc::default(),
            &cfg,
            5,
            1000,
        );
        assert_eq!(picks.len(), 5);
        let ids: HashSet<&str> = picks.iter().map(|s| s.video_id.as_str()).collect();
        assert!(!ids.contains("seed"), "must not replay the seed");
        assert!(!ids.contains("blocked"), "must not pick a banned track");
    }

    #[test]
    fn plan_local_surfaces_a_co_occurring_track_far_above_chance() {
        let cfg = StreamingConfig::default();
        let st = station("seed");

        // 'partner' strongly follows 'seed' in the raw play log.
        let mut pl: VecDeque<(String, i64)> = VecDeque::new();
        for i in 0..8 {
            pl.push_back(("seed".to_owned(), i * 100));
            pl.push_back(("partner".to_owned(), i * 100 + 5));
        }
        let cooc = Cooc::build(&pl, &cfg.cooc);

        let mut songs = vec![song("partner", "partner artist")];
        songs.extend((0..10).map(|i| song(&format!("f{i}"), &format!("filler{i}"))));
        let pool = pool_from_songs(songs, CandidateSource::YtdlpStreaming);

        // The final pick is softmax-sampled (intentionally exploratory), so a single seed is
        // brittle. Assert the aggregate instead: across many seeded trials the co-occurring
        // track is chosen more often than ANY individual non-co-occurring filler.
        let trials = 40u64;
        let mut partner_hits = 0u32;
        let mut filler_hits = std::collections::HashMap::<String, u32>::new();
        for s in 0..trials {
            fastrand::seed(s);
            let picks = plan_local(pool.clone(), &st, &Signals::default(), &cooc, &cfg, 3, 0);
            for p in &picks {
                if p.video_id == "partner" {
                    partner_hits += 1;
                } else {
                    *filler_hits.entry(p.video_id.clone()).or_default() += 1;
                }
            }
        }
        let top_filler = filler_hits.values().copied().max().unwrap_or(0);
        assert!(
            partner_hits > top_filler,
            "co-occurring track ({partner_hits}) should beat the best filler ({top_filler})"
        );
    }

    #[test]
    fn pool_from_tagged_ranks_each_source_independently() {
        let tagged = vec![
            (song("w0", "a"), CandidateSource::WatchPlaylist),
            (song("y0", "b"), CandidateSource::YtdlpStreaming),
            (song("w1", "c"), CandidateSource::WatchPlaylist),
            (song("y1", "d"), CandidateSource::YtdlpStreaming),
            (song("w2", "e"), CandidateSource::WatchPlaylist),
        ];
        let pool = pool_from_tagged(tagged);
        let rank = |id: &str| {
            pool.iter()
                .find(|c| c.video_id() == id)
                .unwrap()
                .source_rank
        };
        // Ranks reset per source (interleaving in the input doesn't bleed across sources).
        assert_eq!((rank("w0"), rank("w1"), rank("w2")), (0, 1, 2));
        assert_eq!((rank("y0"), rank("y1")), (0, 1));
    }

    #[test]
    fn watch_playlist_wins_dedup_over_ytdlp_for_same_track() {
        // Same canonical key from two sources → the higher-provenance WatchPlaylist copy survives.
        let cfg = StreamingConfig::default();
        let st = station("seed");
        let tagged = vec![
            (
                Song::remote("yt", "Hit Song", "Band", "3:00"),
                CandidateSource::YtdlpStreaming,
            ),
            (
                Song::remote("wp", "Hit Song", "Band", "3:00"),
                CandidateSource::WatchPlaylist,
            ),
        ];
        let pool = pool_from_tagged(tagged);
        let scored =
            score::filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        assert_eq!(scored.len(), 1, "the duplicate is collapsed");
        assert_eq!(scored[0].video_id(), "wp", "watch-playlist copy is kept");
    }

    #[test]
    fn plan_local_on_empty_pool_is_empty() {
        let picks = plan_local(
            Vec::new(),
            &station("seed"),
            &Signals::default(),
            &Cooc::default(),
            &StreamingConfig::default(),
            5,
            0,
        );
        assert!(picks.is_empty());
    }

    #[test]
    fn shortlist_for_ai_returns_filtered_diverse_topk() {
        let cfg = StreamingConfig::default();
        let mut st = station("seed");
        st.banned_track_ids.insert("blocked".to_owned());

        let mut songs = vec![song("seed", "seed artist"), song("blocked", "x")];
        songs.extend((0..12).map(|i| song(&format!("c{i}"), &format!("artist{i}"))));
        let pool = pool_from_songs(songs, CandidateSource::YtdlpStreaming);

        let shortlist = shortlist_for_ai(
            pool,
            &st,
            &Signals::default(),
            &Cooc::default(),
            &cfg,
            8,
            1000,
        );
        assert_eq!(shortlist.len(), 8, "returns exactly k diverse candidates");
        let ids: HashSet<&str> = shortlist.iter().map(|c| c.video_id()).collect();
        assert!(!ids.contains("seed"), "filtered: seed excluded");
        assert!(!ids.contains("blocked"), "filtered: banned excluded");
        assert_eq!(ids.len(), 8, "no duplicates in the shortlist");
    }

    #[test]
    fn merge_ai_picks_keeps_only_valid_ids_then_tops_up() {
        let shortlist = vec![
            song("a", "x"),
            song("b", "y"),
            song("c", "z"),
            song("d", "w"),
        ];
        let local_pick = vec![
            song("c", "z"),
            song("d", "w"),
            song("a", "x"),
            song("b", "y"),
        ];
        // DJ Gem returns: one valid, one duplicate of it, one hallucinated id not in the shortlist.
        let ids = vec!["b".to_owned(), "b".to_owned(), "ZZZ".to_owned()];

        let merged = merge_ai_picks(&ids, &shortlist, &local_pick, 3);
        let order: Vec<&str> = merged.iter().map(|s| s.video_id.as_str()).collect();
        // 'b' kept once (dup dropped, hallucination dropped), then top-up from local_pick order.
        assert_eq!(order, vec!["b", "c", "d"]);
    }

    #[test]
    fn merge_ai_picks_empty_ids_falls_back_to_local() {
        let shortlist = vec![song("a", "x"), song("b", "y")];
        let local_pick = vec![song("a", "x"), song("b", "y")];
        let merged = merge_ai_picks(&[], &shortlist, &local_pick, 5);
        let order: Vec<&str> = merged.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(order, vec!["a", "b"], "no DJ Gem ids → pure local pick");
    }

    /// A scored candidate with a chosen `base_score` (the only field the gate reads).
    fn scored(id: &str, base: f32) -> Candidate {
        scored_src(id, base, CandidateSource::YtdlpStreaming)
    }

    fn scored_src(id: &str, base: f32, source: CandidateSource) -> Candidate {
        let mut c = Candidate::from_song(song(id, "a"), source, 0);
        c.base_score = base;
        c
    }

    fn test_cache_key(
        seed_artist: &str,
        mode: StreamingMode,
        recent_ids: &[String],
        candidate_ids: &[String],
    ) -> u64 {
        ai_cache_key(AiCacheKeyParts {
            seed_artist,
            mode,
            recent_ids,
            candidate_ids,
            station_query: "",
            avoid_artist_keys: &[],
            recovery_line: None,
            skip_streak: 0,
            profile_version: mode.profile(&StreamingConfig::default()).profile_version,
            prompt_recipe_hash: ai_recipe_hash(mode.profile(&StreamingConfig::default()).ai_recipe),
        })
    }

    #[test]
    fn smart_gate_off_always_calls_the_model() {
        let cfg = StreamingConfig {
            ai: config::AiRerankConfig {
                smart_gate: false,
                ..Default::default()
            },
            ..Default::default()
        };
        // Even a runaway-clear local winner with a settled listener still spends the call.
        let shortlist = vec![
            scored_src("a", 0.9, CandidateSource::WatchPlaylist),
            scored("b", 0.1),
        ];
        assert!(should_call_ai(&shortlist, 0, &cfg));
    }

    #[test]
    fn final_preflight_needed_only_for_risky_public_candidates() {
        let cfg = StreamingConfig::default();
        let official = Song::remote(
            "official",
            "Artist - Song (Official Audio)",
            "Artist - Topic",
            "3:10",
        );
        assert!(!final_preflight_needed(
            std::slice::from_ref(&official),
            &[],
            StreamingMode::Balanced,
            &cfg
        ));

        let review = Song::remote(
            "review",
            "Artist Song review and analysis",
            "Review Channel",
            "3:10",
        );
        assert!(final_preflight_needed(
            std::slice::from_ref(&review),
            &[],
            StreamingMode::Balanced,
            &cfg
        ));
    }

    #[test]
    fn gate_calls_the_model_on_a_skip_streak_even_when_local_is_clear() {
        let cfg = StreamingConfig::default();
        let shortlist = vec![
            scored_src("a", 0.9, CandidateSource::WatchPlaylist),
            scored("b", 0.1),
        ]; // gap 0.8 >> ambiguity_gap
        assert!(
            !should_call_ai(&shortlist, 0, &cfg),
            "settled + clear → gated"
        );
        assert!(
            should_call_ai(&shortlist, 1, &cfg),
            "any skip streak → call"
        );
    }

    #[test]
    fn gate_skips_a_clear_winner_but_calls_on_an_ambiguous_top_two() {
        let cfg = StreamingConfig::default(); // ambiguity_gap 0.15
        let clear = vec![
            scored_src("a", 0.80, CandidateSource::WatchPlaylist),
            scored("b", 0.50),
        ]; // gap 0.30 → confident local
        assert!(
            !should_call_ai(&clear, 0, &cfg),
            "clear local winner → gated"
        );
        let close = vec![
            scored_src("a", 0.80, CandidateSource::WatchPlaylist),
            scored("b", 0.74),
        ]; // gap 0.06 → ambiguous
        assert!(should_call_ai(&close, 0, &cfg), "ambiguous top two → call");
    }

    #[test]
    fn gate_skips_when_there_is_nothing_to_rerank() {
        let cfg = StreamingConfig::default();
        assert!(
            !should_call_ai(&[], 0, &cfg),
            "empty shortlist → nothing to do"
        );
        assert!(
            !should_call_ai(&[scored("a", 0.5)], 0, &cfg),
            "one candidate → nothing to rerank"
        );
    }

    #[test]
    fn ai_cache_key_is_stable_order_independent_and_query_sensitive() {
        let recent = vec!["r1".to_owned(), "r2".to_owned()];
        let cands = vec!["x".to_owned(), "y".to_owned(), "z".to_owned()];
        let key = test_cache_key("seed", StreamingMode::Balanced, &recent, &cands);
        // Same query → same key; candidate *order* doesn't matter (the set does).
        assert_eq!(
            key,
            test_cache_key("seed", StreamingMode::Balanced, &recent, &cands)
        );
        let reordered = vec!["z".to_owned(), "x".to_owned(), "y".to_owned()];
        assert_eq!(
            key,
            test_cache_key("seed", StreamingMode::Balanced, &recent, &reordered)
        );
        // Every query dimension is part of the key.
        assert_ne!(
            key,
            test_cache_key("other", StreamingMode::Balanced, &recent, &cands)
        );
        assert_ne!(
            key,
            test_cache_key("seed", StreamingMode::Discovery, &recent, &cands)
        );
        assert_ne!(
            key,
            test_cache_key("seed", StreamingMode::Balanced, &[], &cands)
        );
        assert_ne!(
            key,
            test_cache_key("seed", StreamingMode::Balanced, &recent, &["x".to_owned()])
        );
    }
}
