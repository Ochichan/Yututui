//! Diversity-aware final selection: MMR re-ranking + artist/album cooldown + softmax
//! sampling. Turns the scored pool into the actual `n` picks.
//!
//! - **MMR**: each step trades relevance against similarity to what's already picked
//!   (`λ·base − (1−λ)·max_sim`), so the station doesn't collapse onto one sound.
//! - **Cooldown**: a hard minimum spacing between same-artist (and same-album) picks.
//! - **Softmax**: the next pick is *sampled* (not argmax) from the MMR scores at a per-mode
//!   temperature, so identical seeds don't always yield an identical station.

use crate::api::Song;
use crate::radio::StationState;
use crate::radio::candidate::Candidate;
use crate::radio::config::{RadioConfig, SimWeights};
use crate::radio::cooccurrence::Cooc;

/// Select up to `n` tracks from the scored pool, applying MMR diversity, artist/album
/// cooldown, and softmax sampling. Uses the global `fastrand` RNG (seed it for determinism
/// in tests).
pub fn select(
    mut scored: Vec<Candidate>,
    st: &StationState,
    cooc: &Cooc,
    cfg: &RadioConfig,
    n: usize,
) -> Vec<Song> {
    if n == 0 || scored.is_empty() {
        return Vec::new();
    }
    // Sample only from the strongest candidates.
    scored.sort_by(|a, b| b.base_score.total_cmp(&a.base_score));
    let profile = st.mode.profile(cfg);
    scored.truncate(profile.sample_top_k.max(n));

    let lambda = profile.mmr_lambda;
    let temp = profile.temperature;
    let artist_gap = profile.artist_gap;
    let album_gap = profile.album_gap;
    let simw = &cfg.sim_weights;

    // Cooldown windows seeded with what was recently played (so the first picks don't clash
    // with the tail of the current queue/history).
    let mut artist_window: Vec<String> = st.recent_artist_keys.clone();
    let mut album_window: Vec<String> = Vec::new();
    let mut selected: Vec<Candidate> = Vec::new();
    let mut remaining = scored;

    while selected.len() < n && !remaining.is_empty() {
        let mmr: Vec<f32> = remaining
            .iter()
            .map(|c| {
                let diversity = selected
                    .iter()
                    .map(|s| similarity(c, s, cooc, simw))
                    .fold(0.0, f32::max);
                lambda * c.base_score - (1.0 - lambda) * diversity
            })
            .collect();

        // Prefer candidates that clear the cooldown; relax only if every one violates it.
        let eligible: Vec<usize> = (0..remaining.len())
            .filter(|&i| {
                cooldown_ok(
                    &remaining[i],
                    &artist_window,
                    artist_gap,
                    &album_window,
                    album_gap,
                )
            })
            .collect();
        let pool: Vec<usize> = if eligible.is_empty() {
            (0..remaining.len()).collect()
        } else {
            eligible
        };

        let scores: Vec<f32> = pool.iter().map(|&i| mmr[i]).collect();
        let idx = pool[softmax_sample(&scores, temp)];
        let chosen = remaining.remove(idx);
        artist_window.push(chosen.artist_key.clone());
        if let Some(album) = &chosen.album {
            album_window.push(album.clone());
        }
        selected.push(chosen);
    }

    selected.into_iter().map(|c| c.song).collect()
}

/// Deterministically pick the top `k` candidates by greedy MMR (relevance traded against
/// diversity), with NO softmax sampling. This is the shortlist handed to the AI reranker — it
/// should be the engine's best diverse set, not a sampled one, so the model reranks a stable,
/// non-redundant list. Returns the candidates (the AI needs their metadata), highest-ranked first.
pub fn mmr_topk(
    mut scored: Vec<Candidate>,
    st: &StationState,
    cooc: &Cooc,
    cfg: &RadioConfig,
    k: usize,
) -> Vec<Candidate> {
    if k == 0 || scored.is_empty() {
        return Vec::new();
    }
    scored.sort_by(|a, b| b.base_score.total_cmp(&a.base_score));
    let profile = st.mode.profile(cfg);
    scored.truncate(profile.sample_top_k.max(k));

    let lambda = profile.mmr_lambda;
    let simw = &cfg.sim_weights;
    let mut selected: Vec<Candidate> = Vec::new();
    let mut remaining = scored;
    while selected.len() < k && !remaining.is_empty() {
        let best = remaining
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let diversity = selected
                    .iter()
                    .map(|s| similarity(c, s, cooc, simw))
                    .fold(0.0, f32::max);
                (i, lambda * c.base_score - (1.0 - lambda) * diversity)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
            .unwrap_or(0);
        selected.push(remaining.remove(best));
    }
    selected
}

/// Behavioral + metadata similarity in [0,1] (weights sum to 1). Title-token overlap is
/// intentionally excluded as noisy; co-occurrence is the primary signal.
fn similarity(a: &Candidate, b: &Candidate, cooc: &Cooc, simw: &SimWeights) -> f32 {
    let w = cooc
        .weight(a.video_id(), b.video_id())
        .max(cooc.weight(b.video_id(), a.video_id()));
    let cooc_sim = 1.0 - 1.0 / (1.0 + w); // 0 when unrelated, →1 as affinity grows
    let artist_sim = if a.artist_key == b.artist_key {
        1.0
    } else {
        0.0
    };
    let album_sim = match (&a.album, &b.album) {
        (Some(x), Some(y)) if x == y => 1.0,
        _ => 0.0,
    };
    simw.cooc * cooc_sim + simw.artist * artist_sim + simw.album * album_sim
}

/// Whether `c` clears the artist/album spacing against the recent windows.
fn cooldown_ok(
    c: &Candidate,
    artist_window: &[String],
    artist_gap: usize,
    album_window: &[String],
    album_gap: usize,
) -> bool {
    if artist_gap > 0
        && artist_window
            .iter()
            .rev()
            .take(artist_gap)
            .any(|a| a == &c.artist_key)
    {
        return false;
    }
    if album_gap > 0
        && let Some(album) = &c.album
        && album_window
            .iter()
            .rev()
            .take(album_gap)
            .any(|a| a == album)
    {
        return false;
    }
    true
}

/// Sample an index from `scores` via a temperature-softmax (numerically stable). Low
/// temperature → near-greedy; high → more exploratory.
fn softmax_sample(scores: &[f32], temp: f32) -> usize {
    if scores.len() <= 1 {
        return 0;
    }
    let temp = temp.max(1e-3);
    let max = scores.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = scores.iter().map(|s| ((s - max) / temp).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum <= 0.0 {
        return 0;
    }
    let mut r = fastrand::f32() * sum;
    for (i, &e) in exps.iter().enumerate() {
        r -= e;
        if r <= 0.0 {
            return i;
        }
    }
    scores.len() - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;
    use crate::radio::candidate::CandidateSource;
    use std::collections::HashSet;

    fn scored(id: &str, artist: &str, base: f32) -> Candidate {
        let mut c = Candidate::from_song(
            Song::remote(id, format!("t-{id}"), artist, "3:00"),
            CandidateSource::YtdlpRadio,
            0,
        );
        c.base_score = base;
        c
    }

    fn station() -> StationState {
        StationState {
            mode: Default::default(),
            seed_video_id: "seed".to_owned(),
            seed_artist_key: "seed".to_owned(),
            recent_track_ids: Vec::new(),
            recent_artist_keys: Vec::new(),
            banned_track_ids: HashSet::new(),
            banned_artist_keys: HashSet::new(),
            favorite_artist_keys: HashSet::new(),
            session_artist_bias: std::collections::HashMap::new(),
            temporary_novelty_boost: 0.0,
            temporary_familiarity_boost: 0.0,
        }
    }

    #[test]
    fn returns_requested_count_when_pool_is_large_enough() {
        fastrand::seed(1);
        let pool: Vec<Candidate> = (0..10)
            .map(|i| scored(&format!("v{i}"), &format!("a{i}"), 1.0 - i as f32 * 0.05))
            .collect();
        let picks = select(
            pool,
            &station(),
            &Cooc::default(),
            &RadioConfig::default(),
            5,
        );
        assert_eq!(picks.len(), 5);
        // No duplicate ids.
        let unique: HashSet<&str> = picks.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(unique.len(), 5);
    }

    #[test]
    fn artist_cooldown_spaces_same_artist_picks() {
        fastrand::seed(7);
        // Five tracks by "dominant" (high score) + plenty of alternatives.
        let mut pool: Vec<Candidate> = (0..5)
            .map(|i| scored(&format!("d{i}"), "dominant", 1.0))
            .collect();
        pool.extend((0..15).map(|i| scored(&format!("o{i}"), &format!("other{i}"), 0.9)));
        let picks = select(
            pool,
            &station(),
            &Cooc::default(),
            &RadioConfig::default(),
            5,
        );
        // With gap 8 and 5 picks, "dominant" can appear at most once.
        let dominant = picks.iter().filter(|s| s.artist == "dominant").count();
        assert!(dominant <= 1, "artist clumped: {dominant} dominant picks");
    }

    #[test]
    fn empty_or_zero_n_returns_empty() {
        assert!(
            select(
                Vec::new(),
                &station(),
                &Cooc::default(),
                &RadioConfig::default(),
                5
            )
            .is_empty()
        );
        let pool = vec![scored("v0", "a", 1.0)];
        assert!(
            select(
                pool,
                &station(),
                &Cooc::default(),
                &RadioConfig::default(),
                0
            )
            .is_empty()
        );
    }

    #[test]
    fn fewer_candidates_than_requested_returns_all() {
        fastrand::seed(3);
        let pool = vec![scored("v0", "a", 1.0), scored("v1", "b", 0.9)];
        let picks = select(
            pool,
            &station(),
            &Cooc::default(),
            &RadioConfig::default(),
            5,
        );
        assert_eq!(picks.len(), 2);
    }

    #[test]
    fn low_temperature_is_near_greedy() {
        // A peaked distribution at temp→0 returns the argmax deterministically.
        let scores = [0.1, 0.9, 0.2];
        for seed in 0..20 {
            fastrand::seed(seed);
            assert_eq!(softmax_sample(&scores, 1e-3), 1);
        }
    }

    #[test]
    fn mmr_topk_is_deterministic_and_relevance_led() {
        // Distinct artists so diversity never penalizes; top-k must be the highest base scores,
        // in order, with no RNG dependence.
        let pool: Vec<Candidate> = (0..10)
            .map(|i| scored(&format!("v{i}"), &format!("a{i}"), 1.0 - i as f32 * 0.05))
            .collect();
        let first = mmr_topk(
            pool.clone(),
            &station(),
            &Cooc::default(),
            &RadioConfig::default(),
            4,
        );
        let again = mmr_topk(
            pool,
            &station(),
            &Cooc::default(),
            &RadioConfig::default(),
            4,
        );
        let ids: Vec<&str> = first.iter().map(|c| c.video_id()).collect();
        let ids2: Vec<&str> = again.iter().map(|c| c.video_id()).collect();
        assert_eq!(ids, ids2, "no softmax → identical every call");
        assert_eq!(
            ids,
            vec!["v0", "v1", "v2", "v3"],
            "leads with the strongest base scores"
        );
    }

    #[test]
    fn mmr_topk_demotes_a_redundant_high_scorer() {
        // Two near-top tracks share an artist; MMR should not place both before a slightly-weaker
        // but diverse alternative.
        let pool = vec![
            scored("dup1", "same", 1.00),
            scored("dup2", "same", 0.98),
            scored("div", "other", 0.95),
        ];
        let picks = mmr_topk(
            pool,
            &station(),
            &Cooc::default(),
            &RadioConfig::default(),
            2,
        );
        let ids: Vec<&str> = picks.iter().map(|c| c.video_id()).collect();
        assert_eq!(
            ids,
            vec!["dup1", "div"],
            "diverse track beats the redundant same-artist one"
        );
    }

    #[test]
    fn mmr_topk_empty_or_zero_k_is_empty() {
        assert!(
            mmr_topk(
                Vec::new(),
                &station(),
                &Cooc::default(),
                &RadioConfig::default(),
                4
            )
            .is_empty()
        );
        let pool = vec![scored("v0", "a", 1.0)];
        assert!(
            mmr_topk(
                pool,
                &station(),
                &Cooc::default(),
                &RadioConfig::default(),
                0
            )
            .is_empty()
        );
    }
}
