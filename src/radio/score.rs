//! Hard filters + the additive base score over the *computable* signals.
//!
//! The scoring is a deliberate two-pass: gather each candidate's raw feature values, then
//! min-max **normalize every feature to [0,1] across the batch** before the weighted sum.
//! Without this a raw count feature (e.g. popularity) on a different scale would dominate
//! and drown out everything else. The combination is strictly additive — a single zero
//! feature must never zero the whole score.

use std::collections::HashSet;

use crate::radio::StationState;
use crate::radio::canonical;
use crate::radio::candidate::Candidate;
use crate::radio::config::RadioConfig;
use crate::radio::cooccurrence::Cooc;
use crate::signals::Signals;

const SECS_PER_DAY: f32 = 86_400.0;
/// Days credited to a never-played track (so its recency factor is effectively zero).
const NEVER_PLAYED_DAYS: f32 = 3650.0;

/// Raw (un-normalized) feature values for one candidate.
struct RawFeatures {
    cooc: f32,
    seed_affinity: f32,
    novelty: f32,
    continuation: f32,
    completion: f32,
    version_penalty: f32,
}

/// Apply hard filters, dedup by canonical key, then fill `base_score`/`novelty` on the
/// survivors. Returns the scored candidates (unsorted; the reranker orders them).
pub fn filter_and_score(
    pool: Vec<Candidate>,
    st: &StationState,
    sig: &Signals,
    cooc: &Cooc,
    cfg: &RadioConfig,
    now: i64,
) -> Vec<Candidate> {
    let recent: HashSet<&str> = st.recent_track_ids.iter().map(String::as_str).collect();
    let mut kept: Vec<Candidate> =
        pool.into_iter().filter(|c| passes(c, st, sig, cfg, &recent)).collect();
    dedup_by_canonical(&mut kept);
    if kept.is_empty() {
        return kept;
    }

    let feats: Vec<RawFeatures> =
        kept.iter().map(|c| raw_features(c, st, sig, cooc, cfg, now)).collect();
    let cooc_n = normalize(&column(&feats, |f| f.cooc));
    let aff_n = normalize(&column(&feats, |f| f.seed_affinity));
    let nov_n = normalize(&column(&feats, |f| f.novelty));
    let cont_n = normalize(&column(&feats, |f| f.continuation));
    let comp_n = normalize(&column(&feats, |f| f.completion));

    let w = &cfg.weights;
    for (i, c) in kept.iter_mut().enumerate() {
        c.novelty = nov_n[i];
        c.base_score = w.cooccurrence * cooc_n[i]
            + w.seed_affinity * aff_n[i]
            + w.novelty * nov_n[i]
            + w.ytm_continuation * cont_n[i]
            + w.completion * comp_n[i]
            - w.dislike_penalty * feats[i].version_penalty;
    }
    kept
}

/// Hard filters — a candidate must clear all of these to be rankable.
fn passes(
    c: &Candidate,
    st: &StationState,
    sig: &Signals,
    cfg: &RadioConfig,
    recent: &HashSet<&str>,
) -> bool {
    let id = c.video_id();
    if id.is_empty()
        || id == st.seed_video_id
        || recent.contains(id)
        || st.banned_track_ids.contains(id)
        || sig.is_disliked(id)
        || st.banned_artist_keys.contains(&c.artist_key)
    {
        return false;
    }
    // Drop abnormally short/long items (interludes, mixes) when the duration is known.
    if let Some(d) = c.duration_secs
        && (d < cfg.min_duration_secs || d > cfg.max_duration_secs)
    {
        return false;
    }
    true
}

/// Keep the single best candidate per canonical key (highest provenance, then lowest rank).
fn dedup_by_canonical(kept: &mut Vec<Candidate>) {
    kept.sort_by(|a, b| {
        b.source
            .provenance_weight()
            .total_cmp(&a.source.provenance_weight())
            .then(a.source_rank.cmp(&b.source_rank))
    });
    let mut seen: HashSet<String> = HashSet::new();
    kept.retain(|c| seen.insert(c.canonical_key.clone()));
}

fn raw_features(
    c: &Candidate,
    st: &StationState,
    sig: &Signals,
    cooc: &Cooc,
    cfg: &RadioConfig,
    now: i64,
) -> RawFeatures {
    let id = c.video_id();
    let cooc_aff = cooc.affinity(id, &st.recent_track_ids);

    // Behavioral artist affinity + a boost for favorite / seed artists.
    let mut seed_affinity = sig.artist_weight(&c.artist_key);
    if st.favorite_artist_keys.contains(&c.artist_key) {
        seed_affinity += 1.0;
    }
    if c.artist_key == st.seed_artist_key {
        seed_affinity += 0.5;
    }

    // Novelty: unfamiliar (low play count) and not recently over-played.
    let play_count = sig.play_count(id);
    let unfamiliarity = 1.0 / (1.0 + (1.0 + play_count as f32).ln());
    let days_since = match sig.last_played_at(id) {
        Some(t) => ((now - t).max(0) as f32) / SECS_PER_DAY,
        None => NEVER_PLAYED_DAYS,
    };
    let recency_of_play = recency_factor(days_since, cfg.recency_half_life_days);
    let novelty = unfamiliarity * (1.0 - recency_of_play);

    let continuation = c.source.provenance_weight() / (1.0 + c.source_rank as f32);
    let completion = sig.completion_rate(id);
    let version_penalty = canonical::version_penalty(&c.song.title);

    RawFeatures { cooc: cooc_aff, seed_affinity, novelty, continuation, completion, version_penalty }
}

/// `0.5^(days / half_life)`; 1.0 when half-life is non-positive (decay disabled).
fn recency_factor(days: f32, half_life_days: f32) -> f32 {
    if half_life_days <= 0.0 {
        1.0
    } else {
        0.5f32.powf(days / half_life_days)
    }
}

fn column(feats: &[RawFeatures], f: impl Fn(&RawFeatures) -> f32) -> Vec<f32> {
    feats.iter().map(f).collect()
}

/// Min-max normalize to [0,1]. A constant column maps to a neutral 0.5 (so it contributes a
/// flat offset that doesn't distort the ranking).
fn normalize(vals: &[f32]) -> Vec<f32> {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &v in vals {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let range = hi - lo;
    if range <= f32::EPSILON {
        return vec![0.5; vals.len()];
    }
    vals.iter().map(|&v| (v - lo) / range).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;
    use crate::radio::candidate::CandidateSource;
    use std::collections::VecDeque;

    fn cand(id: &str, title: &str, artist: &str, rank: usize) -> Candidate {
        Candidate::from_song(
            Song::remote(id, title, artist, "3:00"),
            CandidateSource::YtdlpRadio,
            rank,
        )
    }

    fn station(seed: &str) -> StationState {
        StationState {
            mode: Default::default(),
            seed_video_id: seed.to_owned(),
            seed_artist_key: "seedartist".to_owned(),
            recent_track_ids: vec![seed.to_owned()],
            recent_artist_keys: Vec::new(),
            banned_track_ids: HashSet::new(),
            banned_artist_keys: HashSet::new(),
            favorite_artist_keys: HashSet::new(),
        }
    }

    #[test]
    fn hard_filters_drop_disqualified_candidates() {
        let cfg = RadioConfig::default();
        let mut st = station("seed");
        st.banned_track_ids.insert("banned".to_owned());
        st.banned_artist_keys.insert("blocked".to_owned());
        let mut sig = Signals::default();
        sig.toggle_dislike("hated", "z", 0);

        let pool = vec![
            cand("seed", "Seed", "a", 0),        // == seed
            cand("banned", "B", "a", 0),         // banned id
            cand("hated", "H", "a", 0),          // disliked
            cand("byblocked", "X", "Blocked", 0), // banned artist
            cand("tiny", "Skit", "a", 0),        // too short (set below)
            cand("ok", "Good", "a", 0),          // survives
        ];
        // Make "tiny" too short.
        let mut pool = pool;
        pool[4].duration_secs = Some(5);

        let scored = filter_and_score(pool, &st, &sig, &Cooc::default(), &cfg, 1000);
        let ids: Vec<&str> = scored.iter().map(Candidate::video_id).collect();
        assert_eq!(ids, vec!["ok"]);
    }

    #[test]
    fn dedup_keeps_one_per_canonical_key() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        let pool = vec![
            cand("v1", "Hit Song (Official Video)", "Band", 3),
            cand("v2", "Hit Song", "Band", 0), // same canonical, better rank
        ];
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].video_id(), "v2"); // lower source_rank wins
    }

    #[test]
    fn co_occurrence_lifts_the_related_candidate() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        // Build a graph where 'seed' is strongly followed by 'related'.
        let mut pl: VecDeque<(String, i64)> = VecDeque::new();
        for i in 0..5 {
            pl.push_back(("seed".to_owned(), i * 100));
            pl.push_back(("related".to_owned(), i * 100 + 10));
        }
        let cooc = Cooc::build(&pl, &cfg.cooc);
        let pool = vec![cand("related", "R", "a", 0), cand("unrelated", "U", "b", 0)];
        let scored = filter_and_score(pool, &st, &Signals::default(), &cooc, &cfg, 0);
        let related = scored.iter().find(|c| c.video_id() == "related").unwrap();
        let unrelated = scored.iter().find(|c| c.video_id() == "unrelated").unwrap();
        assert!(related.base_score > unrelated.base_score);
    }

    #[test]
    fn never_played_is_more_novel_than_recently_overplayed() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        let mut sig = Signals::default();
        // 'stale' was played a lot, just now.
        for _ in 0..20 {
            sig.record_play("stale", "a", 1.0, 1000);
        }
        let pool = vec![cand("fresh", "F", "a", 0), cand("stale", "S", "a", 0)];
        let scored = filter_and_score(pool, &st, &sig, &Cooc::default(), &cfg, 1000);
        let fresh = scored.iter().find(|c| c.video_id() == "fresh").unwrap();
        let stale = scored.iter().find(|c| c.video_id() == "stale").unwrap();
        assert!(fresh.novelty > stale.novelty);
    }
}
