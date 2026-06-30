//! Hard filters + the additive base score over the *computable* signals.
//!
//! The scoring is a deliberate two-pass: gather each candidate's raw feature values, then
//! min-max **normalize every feature to [0,1] across the batch** before the weighted sum.
//! Without this a raw count feature (e.g. popularity) on a different scale would dominate
//! and drown out everything else. The combination is strictly additive — a single zero
//! feature must never zero the whole score.

use std::collections::HashSet;

use crate::radio::StationState;
use crate::radio::candidate::{Candidate, CandidateSource, FeatureScores};
use crate::radio::canonical;
use crate::radio::config::{RadioConfig, RadioMode};
use crate::radio::cooccurrence::Cooc;
use crate::radio::musicgate;
use crate::signals::Signals;

const SECS_PER_DAY: f32 = 86_400.0;
/// Days credited to a never-played track (so its recency factor is effectively zero).
const NEVER_PLAYED_DAYS: f32 = 3650.0;
/// Below this many survivors after the hard filter, gimmick-blocking is skipped so the gate
/// can't starve the (already thin) candidate pool.
const GATE_MIN_POOL: usize = 6;

/// Raw (un-normalized) feature values for one candidate.
struct RawFeatures {
    cooc: f32,
    seed_affinity: f32,
    novelty: f32,
    continuation: f32,
    completion: f32,
    /// Positive [0,1] "official music" signal — added raw (not normalized).
    music_tier: f32,
    version_penalty: f32,
    /// Co-occurrence affinity vs the *immediately-preceding* (seed) track. Evidence-only;
    /// retained on `Candidate.features` for the AI reranker, never summed into `base_score`.
    transition: f32,
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
    let mut kept: Vec<Candidate> = pool
        .into_iter()
        .filter(|c| passes(c, st, sig, cfg, &recent))
        .collect();
    block_gimmicks(&mut kept, st, cfg);
    dedup_by_canonical(&mut kept);
    if kept.is_empty() {
        return kept;
    }

    let feats: Vec<RawFeatures> = kept
        .iter()
        .map(|c| raw_features(c, st, sig, cooc, cfg, now))
        .collect();
    let cooc_n = normalize(&column(&feats, |f| f.cooc));
    let aff_n = normalize(&column(&feats, |f| f.seed_affinity));
    let nov_n = normalize(&column(&feats, |f| f.novelty));
    let cont_n = normalize(&column(&feats, |f| f.continuation));
    let comp_n = normalize(&column(&feats, |f| f.completion));
    let trans_n = normalize(&column(&feats, |f| f.transition));

    let w = &cfg.weights;
    for (i, c) in kept.iter_mut().enumerate() {
        c.novelty = nov_n[i];
        c.base_score = w.cooccurrence * cooc_n[i]
            + w.seed_affinity * aff_n[i]
            + w.novelty * nov_n[i]
            + w.ytm_continuation * cont_n[i]
            + w.completion * comp_n[i]
            + w.music_tier * feats[i].music_tier
            - w.dislike_penalty * feats[i].version_penalty;
        // Retain the per-feature evidence for the AI reranker. `transition` is intentionally
        // NOT part of `base_score` above — it only informs the model's slotting.
        c.features = FeatureScores {
            cooc: cooc_n[i],
            seed_affinity: aff_n[i],
            novelty: nov_n[i],
            continuation: cont_n[i],
            completion: comp_n[i],
            music_tier: feats[i].music_tier,
            version_penalty: feats[i].version_penalty,
            transition: trans_n[i],
        };
    }
    kept
}

/// A per-candidate verdict for the radio debug log: whether the candidate survived the gate, and
/// (when dropped) the reason. Built by [`classify_pool`].
#[derive(Debug, Clone)]
pub struct GateVerdict {
    pub video_id: String,
    pub title: String,
    pub source: CandidateSource,
    pub kept: bool,
    /// `"kept"` when the candidate survives; otherwise a short reject reason.
    pub reason: &'static str,
}

impl GateVerdict {
    fn kept(c: &Candidate) -> Self {
        Self {
            video_id: c.video_id().to_owned(),
            title: c.song.title.clone(),
            source: c.source,
            kept: true,
            reason: "kept",
        }
    }

    fn rejected(c: &Candidate, reason: &'static str) -> Self {
        Self {
            video_id: c.video_id().to_owned(),
            title: c.song.title.clone(),
            source: c.source,
            kept: false,
            reason,
        }
    }
}

/// Explain the gate: classify every candidate through the same hard-filter → gimmick → dedup
/// passes as [`filter_and_score`], returning one verdict per candidate. Used only for the radio
/// debug log; the live pipeline still ranks via [`filter_and_score`]. The dedup pass keeps the
/// first occurrence in pool order (rather than re-sorting by provenance as production does), so a
/// `"duplicate"` verdict flags a collapsed copy without asserting which copy production keeps.
pub fn classify_pool(
    pool: &[Candidate],
    st: &StationState,
    sig: &Signals,
    cfg: &RadioConfig,
) -> Vec<GateVerdict> {
    let recent: HashSet<&str> = st.recent_track_ids.iter().map(String::as_str).collect();
    let mut verdicts = Vec::with_capacity(pool.len());

    // Phase 1: hard filter.
    let mut survivors: Vec<&Candidate> = Vec::new();
    for c in pool {
        match reject_reason(c, st, sig, cfg, &recent) {
            Some(reason) => verdicts.push(GateVerdict::rejected(c, reason)),
            None => survivors.push(c),
        }
    }

    // Phase 2: gimmick reject (only when in force and not pool-starving — mirrors `block_gimmicks`).
    if gimmick_block_active(survivors.len(), st, cfg)
        && survivors
            .iter()
            .any(|c| musicgate::gimmick_reason(&c.song.title).is_none())
    {
        let mut kept_survivors = Vec::with_capacity(survivors.len());
        for c in survivors {
            match musicgate::gimmick_reason(&c.song.title) {
                Some(reason) => verdicts.push(GateVerdict::rejected(c, reason)),
                None => kept_survivors.push(c),
            }
        }
        survivors = kept_survivors;
    }

    // Phase 3: dedup by canonical key.
    let mut seen: HashSet<&str> = HashSet::new();
    for c in survivors {
        if seen.insert(&c.canonical_key) {
            verdicts.push(GateVerdict::kept(c));
        } else {
            verdicts.push(GateVerdict::rejected(c, "duplicate"));
        }
    }

    verdicts
}

/// Hard filters — a candidate must clear all of these to be rankable. The boolean view used by
/// [`filter_and_score`]; [`reject_reason`] is the same logic with the *why* surfaced.
fn passes(
    c: &Candidate,
    st: &StationState,
    sig: &Signals,
    cfg: &RadioConfig,
    recent: &HashSet<&str>,
) -> bool {
    reject_reason(c, st, sig, cfg, recent).is_none()
}

/// The first hard-filter reason `c` fails, or `None` if it clears them all. Single source of
/// truth for the hard filter: [`passes`] is the boolean wrapper, [`classify_pool`] surfaces the
/// reason for the radio debug log so the two can never drift.
fn reject_reason(
    c: &Candidate,
    st: &StationState,
    sig: &Signals,
    cfg: &RadioConfig,
    recent: &HashSet<&str>,
) -> Option<&'static str> {
    let id = c.video_id();
    if id.is_empty() {
        return Some("no id");
    }
    if id == st.seed_video_id {
        return Some("seed");
    }
    if recent.contains(id) {
        return Some("already heard");
    }
    if st.banned_track_ids.contains(id) {
        return Some("banned track");
    }
    if sig.is_disliked(id) {
        return Some("disliked");
    }
    if st.banned_artist_keys.contains(&c.artist_key) {
        return Some("banned artist");
    }
    // Drop abnormally short/long items (interludes, mixes) when the duration is known.
    if let Some(d) = c.duration_secs
        && (d < cfg.min_duration_secs || d > cfg.max_duration_secs)
    {
        return Some("bad duration");
    }
    // MusicGate: hard-reject obvious non-music (reactions, podcasts, tutorials, …). The clean
    // WatchPlaylist source is gated too by default (the strong-reject list rarely trips on it),
    // but can be exempted via `gate.gate_watch_playlist = false`.
    if cfg.gate.enabled
        && (c.source != CandidateSource::WatchPlaylist || cfg.gate.gate_watch_playlist)
        && let Some(reason) = musicgate::non_music_reason(&c.song.title, &c.song.artist)
    {
        return Some(reason);
    }
    None
}

/// MusicGate phase 2: drop gimmick re-uploads (karaoke / nightcore / 8D / sped-up /
/// slowed+reverb). Mode-tied — always on in Focused, opt-in via `block_altered_versions`
/// otherwise — and self-disabling when the pool is too thin to spare them (so it can never
/// starve the station). Runs after the hard filter, before dedup.
fn block_gimmicks(kept: &mut Vec<Candidate>, st: &StationState, cfg: &RadioConfig) {
    if !gimmick_block_active(kept.len(), st, cfg) {
        return;
    }
    // Never let gimmick-blocking empty the pool — keep them all if nothing else survives.
    if kept
        .iter()
        .any(|c| musicgate::gimmick_reason(&c.song.title).is_none())
    {
        kept.retain(|c| musicgate::gimmick_reason(&c.song.title).is_none());
    }
}

/// Whether the gimmick reject is currently in force: gate on, mode/flag opts in, and the
/// post-hard-filter pool is healthy enough to spare the gimmicks. Shared by [`block_gimmicks`]
/// and [`classify_pool`] so the live filter and the debug log agree on when gimmicks are dropped.
fn gimmick_block_active(survivor_count: usize, st: &StationState, cfg: &RadioConfig) -> bool {
    cfg.gate.enabled
        && (st.mode == RadioMode::Focused || cfg.gate.block_altered_versions)
        && survivor_count >= GATE_MIN_POOL
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

    // Transition: how naturally this candidate follows the *immediately-preceding* (seed)
    // track specifically — narrower than `cooc_aff` (which spans the whole recent window). A
    // same-artist-as-seed continuation reads as a smooth transition. Evidence-only.
    let mut transition = cooc.affinity(id, std::slice::from_ref(&st.seed_video_id));
    if c.artist_key == st.seed_artist_key {
        transition += 0.5;
    }

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
    let music_tier = musicgate::music_tier_score(&c.song.title, &c.song.artist);
    let version_penalty = canonical::version_penalty(&c.song.title);

    RawFeatures {
        cooc: cooc_aff,
        seed_affinity,
        novelty,
        continuation,
        completion,
        music_tier,
        version_penalty,
        transition,
    }
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
            cand("seed", "Seed", "a", 0),         // == seed
            cand("banned", "B", "a", 0),          // banned id
            cand("hated", "H", "a", 0),           // disliked
            cand("byblocked", "X", "Blocked", 0), // banned artist
            cand("tiny", "Skit", "a", 0),         // too short (set below)
            cand("ok", "Good", "a", 0),           // survives
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
    fn feature_scores_are_populated_and_in_range() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        // A graph where 'seed' is strongly followed by 'related' so transition/cooc are non-flat.
        let mut pl: VecDeque<(String, i64)> = VecDeque::new();
        for i in 0..5 {
            pl.push_back(("seed".to_owned(), i * 100));
            pl.push_back(("related".to_owned(), i * 100 + 10));
        }
        let cooc = Cooc::build(&pl, &cfg.cooc);
        let pool = vec![
            cand("related", "R", "seedartist", 0),
            cand("other", "O", "b", 0),
        ];
        let scored = filter_and_score(pool, &st, &Signals::default(), &cooc, &cfg, 0);
        for c in &scored {
            let f = &c.features;
            for v in [
                f.cooc,
                f.seed_affinity,
                f.novelty,
                f.continuation,
                f.completion,
                f.music_tier,
                f.transition,
            ] {
                assert!((0.0..=1.0).contains(&v), "feature out of [0,1]: {v}");
            }
            // `novelty` field mirrors `features.novelty`.
            assert_eq!(c.novelty, f.novelty);
        }
        // The same-seed-artist related track has the stronger transition evidence.
        let related = scored.iter().find(|c| c.video_id() == "related").unwrap();
        let other = scored.iter().find(|c| c.video_id() == "other").unwrap();
        assert!(related.features.transition > other.features.transition);
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

    #[test]
    fn music_tier_lifts_official_audio_over_plain_upload() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        // Identical in every behavioral signal; only the title's "official audio" tier differs.
        // Distinct base titles so they don't collapse under canonical dedup.
        let pool = vec![
            cand("off", "Alpha (Official Audio)", "Same Artist", 0),
            cand("plain", "Beta", "Same Artist", 0),
        ];
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        let off = scored.iter().find(|c| c.video_id() == "off").unwrap();
        let plain = scored.iter().find(|c| c.video_id() == "plain").unwrap();
        assert!(
            off.base_score > plain.base_score,
            "official-audio tier outranks a plain upload"
        );
    }

    #[test]
    fn musicgate_drops_non_music_titles() {
        let cfg = RadioConfig::default(); // gate enabled by default
        let st = station("seed");
        let pool = vec![
            cand("r", "Artist Reaction Video", "Some Channel", 0),
            cand("ok", "Real Song", "Band", 0),
        ];
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        let ids: Vec<&str> = scored.iter().map(Candidate::video_id).collect();
        assert_eq!(ids, vec!["ok"]);
    }

    #[test]
    fn musicgate_disabled_keeps_non_music() {
        let mut cfg = RadioConfig::default();
        cfg.gate.enabled = false;
        let st = station("seed");
        let pool = vec![cand("r", "Big Reaction Video", "Chan", 0)];
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        assert_eq!(scored.len(), 1, "gate off → non-music passes");
    }

    #[test]
    fn musicgate_exempts_watch_playlist_when_flag_off() {
        let mut cfg = RadioConfig::default();
        cfg.gate.gate_watch_playlist = false;
        let st = station("seed");
        // Same non-music marker from both sources: the exempt WatchPlaylist copy survives,
        // the yt-dlp one is dropped.
        let wp = Candidate::from_song(
            Song::remote("wp", "Tour Vlog", "X", "3:00"),
            CandidateSource::WatchPlaylist,
            0,
        );
        let yt = Candidate::from_song(
            Song::remote("yt", "Other Vlog", "Y", "3:00"),
            CandidateSource::YtdlpRadio,
            0,
        );
        let scored = filter_and_score(
            vec![wp, yt],
            &st,
            &Signals::default(),
            &Cooc::default(),
            &cfg,
            0,
        );
        let ids: Vec<&str> = scored.iter().map(Candidate::video_id).collect();
        assert_eq!(ids, vec!["wp"]);
    }

    #[test]
    fn gimmick_blocking_drops_nightcore_in_focused_mode() {
        let cfg = RadioConfig::default();
        let mut st = station("seed");
        st.mode = RadioMode::Focused;
        // Pool large enough to clear GATE_MIN_POOL; one gimmick among real songs.
        let mut pool = vec![cand("ng", "Song (Nightcore)", "A", 0)];
        pool.extend(
            (0..7).map(|i| cand(&format!("s{i}"), &format!("Track {i}"), &format!("B{i}"), 0)),
        );
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        let ids: Vec<&str> = scored.iter().map(Candidate::video_id).collect();
        assert!(!ids.contains(&"ng"), "nightcore dropped in Focused mode");
        assert_eq!(ids.len(), 7);
    }

    #[test]
    fn gimmick_blocking_kept_in_balanced_mode() {
        let cfg = RadioConfig::default(); // Balanced default, block_altered_versions = false
        let st = station("seed");
        let mut pool = vec![cand("ng", "Song (Nightcore)", "A", 0)];
        pool.extend(
            (0..7).map(|i| cand(&format!("s{i}"), &format!("Track {i}"), &format!("B{i}"), 0)),
        );
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        let ids: Vec<&str> = scored.iter().map(Candidate::video_id).collect();
        assert!(
            ids.contains(&"ng"),
            "gimmicks kept in Balanced (soft-penalized, not gated)"
        );
    }

    #[test]
    fn classify_pool_labels_kept_and_rejected_with_reasons() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        let pool = vec![
            cand("seed", "Seed", "a", 0),                   // == seed
            cand("react", "Big Reaction Video", "Chan", 0), // non-music
            cand("ok", "Real Song", "Band", 0),             // survives
        ];
        let verdicts = classify_pool(&pool, &st, &Signals::default(), &cfg);
        let by_id = |id: &str| verdicts.iter().find(|v| v.video_id == id).unwrap();
        assert_eq!(
            verdicts.len(),
            pool.len(),
            "exactly one verdict per candidate"
        );
        assert!(!by_id("seed").kept);
        assert_eq!(by_id("seed").reason, "seed");
        assert!(!by_id("react").kept);
        assert_eq!(by_id("react").reason, "reaction video");
        assert!(by_id("ok").kept);
        assert_eq!(by_id("ok").reason, "kept");
    }

    #[test]
    fn classify_pool_flags_canonical_duplicates() {
        let cfg = RadioConfig::default();
        let st = station("seed");
        let pool = vec![
            cand("a", "Hit Song", "Band", 0),
            cand("b", "Hit Song (Official Video)", "Band", 1), // same canonical key
        ];
        let verdicts = classify_pool(&pool, &st, &Signals::default(), &cfg);
        let kept: Vec<&str> = verdicts
            .iter()
            .filter(|v| v.kept)
            .map(|v| v.video_id.as_str())
            .collect();
        let dups: Vec<&str> = verdicts
            .iter()
            .filter(|v| v.reason == "duplicate")
            .map(|v| v.video_id.as_str())
            .collect();
        assert_eq!(kept, vec!["a"]);
        assert_eq!(dups, vec!["b"]);
    }

    #[test]
    fn gimmick_blocking_skipped_when_pool_starved() {
        let cfg = RadioConfig::default();
        let mut st = station("seed");
        st.mode = RadioMode::Focused;
        // Fewer than GATE_MIN_POOL candidates → gimmick blocking is skipped.
        let pool = vec![
            cand("ng1", "A (Nightcore)", "A", 0),
            cand("ng2", "B (Karaoke)", "B", 0),
        ];
        let scored = filter_and_score(pool, &st, &Signals::default(), &Cooc::default(), &cfg, 0);
        assert_eq!(scored.len(), 2, "gimmicks kept when the pool would starve");
    }
}
