//! The local radio engine: turn a pool of playable candidates into the next few tracks
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
mod musicgate;
pub mod pack;
mod rerank;
mod score;

use std::collections::HashMap;
use std::collections::HashSet;

use crate::api::Song;
use crate::signals::Signals;

pub use candidate::{Candidate, CandidateSource};
pub use config::{RadioConfig, RadioMode};
pub use cooccurrence::Cooc;
pub use pack::PackedCand;
pub use score::{GateVerdict, classify_pool};

/// The live station context the ranking reads: the seed, what was recently heard (for
/// "already played" filtering, co-occurrence context, and cooldown), and the user's
/// blocks/favorites.
#[derive(Debug, Clone)]
pub struct StationState {
    pub mode: RadioMode,
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
}

/// Run the full local pipeline — hard filter → normalized base score → MMR + cooldown →
/// softmax sample — and return up to `n` picks. The guaranteed-available radio path; also
/// the fallback when an AI rerank is unavailable.
pub fn plan_local(
    pool: Vec<Candidate>,
    st: &StationState,
    sig: &Signals,
    cooc: &Cooc,
    cfg: &RadioConfig,
    n: usize,
    now: i64,
) -> Vec<Song> {
    let scored = score::filter_and_score(pool, st, sig, cooc, cfg, now);
    rerank::select(scored, st, cooc, cfg, n)
}

/// The diverse top-`k` candidates to hand the AI reranker — same hard-filter + score pipeline
/// as [`plan_local`], but stopping at a deterministic MMR shortlist (no softmax). The AI picks
/// from these by id; it never sees a track that isn't already a playable local candidate.
pub fn shortlist_for_ai(
    pool: Vec<Candidate>,
    st: &StationState,
    sig: &Signals,
    cooc: &Cooc,
    cfg: &RadioConfig,
    k: usize,
    now: i64,
) -> Vec<Candidate> {
    let scored = score::filter_and_score(pool, st, sig, cooc, cfg, now);
    rerank::mmr_topk(scored, st, cooc, cfg, k)
}

/// Turn the AI reranker's chosen ids into the final pick list. Every id is validated against the
/// shortlist the model was shown (so a hallucinated/altered id is silently dropped), de-duped,
/// then any shortfall is topped up from the engine's own `local_pick` order. The result is
/// always a subset of real, playable candidates — AI failure degrades to pure-local with no
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
    // Backfill from the engine's own ranked picks for anything the AI dropped/omitted.
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

/// Whether an autoplay refill is worth an AI rerank call. With `smart_gate` off it's always
/// `true` (spend the call whenever the reranker is enabled). With it on, we still call when the
/// listener is unsettled (a trailing skip streak) or when the local pick is *ambiguous* — the top
/// two candidates' `base_score`s are within `ambiguity_gap`. A clearly-best local pick with a
/// content listener skips the call (and its cost + latency); the local pick is already good, and
/// fewer than two candidates leaves nothing to rerank.
pub fn should_call_ai(
    shortlist: &[Candidate],
    skip_streak: usize,
    cfg: &config::AiRerankConfig,
) -> bool {
    if !cfg.smart_gate || skip_streak >= 1 {
        return true;
    }
    match top_two_base_score_gap(shortlist) {
        Some(gap) => gap < cfg.ambiguity_gap,
        None => false,
    }
}

/// A stable key for caching an AI rerank's result. The same `(seed_artist, mode, recent ids,
/// candidate set)` produces the same key, so a rapid identical refill can replay the cached
/// ordering instead of spending another call. The candidate set is order-independent (sorted
/// before hashing) because the shortlist's *contents*, not its incidental order, define the query.
/// Uses a fixed-seed hasher (`DefaultHasher`) so the key is deterministic, not per-process random.
pub fn ai_cache_key(
    seed_artist: &str,
    mode: RadioMode,
    recent_ids: &[String],
    candidate_ids: &[String],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    seed_artist.hash(&mut h);
    (mode as u8).hash(&mut h);
    recent_ids.hash(&mut h);
    let mut sorted: Vec<&String> = candidate_ids.iter().collect();
    sorted.sort();
    sorted.hash(&mut h);
    h.finish()
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
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn song(id: &str, artist: &str) -> Song {
        Song::remote(id, format!("t-{id}"), artist, "3:00")
    }

    fn station(seed: &str) -> StationState {
        StationState {
            mode: RadioMode::Balanced,
            seed_video_id: seed.to_owned(),
            seed_artist_key: "seed artist".to_owned(),
            recent_track_ids: vec![seed.to_owned()],
            recent_artist_keys: Vec::new(),
            banned_track_ids: HashSet::new(),
            banned_artist_keys: HashSet::new(),
            favorite_artist_keys: HashSet::new(),
        }
    }

    #[test]
    fn plan_local_filters_and_returns_requested_count() {
        fastrand::seed(42);
        let cfg = RadioConfig::default();
        let mut st = station("seed");
        st.banned_track_ids.insert("blocked".to_owned());

        // A pool with the seed, a blocked track, and several playable distinct-artist tracks.
        let mut songs = vec![song("seed", "seed artist"), song("blocked", "x")];
        songs.extend((0..12).map(|i| song(&format!("c{i}"), &format!("artist{i}"))));
        let pool = pool_from_songs(songs, CandidateSource::YtdlpRadio);

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
        let cfg = RadioConfig::default();
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
        let pool = pool_from_songs(songs, CandidateSource::YtdlpRadio);

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
            (song("y0", "b"), CandidateSource::YtdlpRadio),
            (song("w1", "c"), CandidateSource::WatchPlaylist),
            (song("y1", "d"), CandidateSource::YtdlpRadio),
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
        let cfg = RadioConfig::default();
        let st = station("seed");
        let tagged = vec![
            (
                Song::remote("yt", "Hit Song", "Band", "3:00"),
                CandidateSource::YtdlpRadio,
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
            &RadioConfig::default(),
            5,
            0,
        );
        assert!(picks.is_empty());
    }

    #[test]
    fn shortlist_for_ai_returns_filtered_diverse_topk() {
        let cfg = RadioConfig::default();
        let mut st = station("seed");
        st.banned_track_ids.insert("blocked".to_owned());

        let mut songs = vec![song("seed", "seed artist"), song("blocked", "x")];
        songs.extend((0..12).map(|i| song(&format!("c{i}"), &format!("artist{i}"))));
        let pool = pool_from_songs(songs, CandidateSource::YtdlpRadio);

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
        // AI returns: one valid, one duplicate of it, one hallucinated id not in the shortlist.
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
        assert_eq!(order, vec!["a", "b"], "no AI ids → pure local pick");
    }

    /// A scored candidate with a chosen `base_score` (the only field the gate reads).
    fn scored(id: &str, base: f32) -> Candidate {
        let mut c = Candidate::from_song(song(id, "a"), CandidateSource::YtdlpRadio, 0);
        c.base_score = base;
        c
    }

    #[test]
    fn smart_gate_off_always_calls_the_model() {
        let cfg = config::AiRerankConfig {
            smart_gate: false,
            ..Default::default()
        };
        // Even a runaway-clear local winner with a settled listener still spends the call.
        let shortlist = vec![scored("a", 0.9), scored("b", 0.1)];
        assert!(should_call_ai(&shortlist, 0, &cfg));
    }

    #[test]
    fn gate_calls_the_model_on_a_skip_streak_even_when_local_is_clear() {
        let cfg = config::AiRerankConfig::default();
        let shortlist = vec![scored("a", 0.9), scored("b", 0.1)]; // gap 0.8 >> ambiguity_gap
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
        let cfg = config::AiRerankConfig::default(); // ambiguity_gap 0.15
        let clear = vec![scored("a", 0.80), scored("b", 0.50)]; // gap 0.30 → confident local
        assert!(
            !should_call_ai(&clear, 0, &cfg),
            "clear local winner → gated"
        );
        let close = vec![scored("a", 0.80), scored("b", 0.74)]; // gap 0.06 → ambiguous
        assert!(should_call_ai(&close, 0, &cfg), "ambiguous top two → call");
    }

    #[test]
    fn gate_skips_when_there_is_nothing_to_rerank() {
        let cfg = config::AiRerankConfig::default();
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
        let key = ai_cache_key("seed", RadioMode::Balanced, &recent, &cands);
        // Same query → same key; candidate *order* doesn't matter (the set does).
        assert_eq!(
            key,
            ai_cache_key("seed", RadioMode::Balanced, &recent, &cands)
        );
        let reordered = vec!["z".to_owned(), "x".to_owned(), "y".to_owned()];
        assert_eq!(
            key,
            ai_cache_key("seed", RadioMode::Balanced, &recent, &reordered)
        );
        // Every query dimension is part of the key.
        assert_ne!(
            key,
            ai_cache_key("other", RadioMode::Balanced, &recent, &cands)
        );
        assert_ne!(
            key,
            ai_cache_key("seed", RadioMode::Discovery, &recent, &cands)
        );
        assert_ne!(key, ai_cache_key("seed", RadioMode::Balanced, &[], &cands));
        assert_ne!(
            key,
            ai_cache_key("seed", RadioMode::Balanced, &recent, &["x".to_owned()])
        );
    }
}
