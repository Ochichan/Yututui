//! The compact, position-bias-resistant candidate pack handed to the DJ Gem reranker.
//!
//! Instead of a plain `video_id | title | artist` list, each candidate becomes one terse line
//! of *evidence* — the per-feature scores the local engine already computed (see
//! [`crate::radio::candidate::FeatureScores`]) as 0-100 integers, plus source and version —
//! under a short opaque `cid`. Two anti-bias measures matter for LLM rerankers:
//!   * **Opaque ids**: a hashed base36 `cid` (not the score rank, not the video id), so the
//!     model can't read "candidate 1 is best" off the id.
//!   * **Shuffled order**: candidates are bucket-interleaved (strong / middle / exploratory by
//!     `base_score`) and shuffled within each bucket by a seed-derived hash — fully
//!     deterministic and testable, with no dependence on input order. The prompt flags the set
//!     as unordered so the model ranks on the evidence, not the position.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::radio::candidate::Candidate;

/// A candidate's opaque pack id paired with the real video id it stands for. The reducer uses
/// this to resolve the model's chosen `cid`s back to playable tracks.
#[derive(Debug, Clone)]
pub struct PackedCand {
    pub cid: String,
    pub video_id: String,
}

/// Build the `CANDS` block (one evidence line per candidate) plus the `cid → video_id` map.
/// `seed` (the current track's video id) seeds the within-bucket shuffle so the order is stable
/// for a given station refill but does not track `base_score`.
pub fn build_cands_block(shortlist: &[Candidate], seed: &str) -> (String, Vec<PackedCand>) {
    let order = interleaved_order(shortlist, seed);

    let mut taken: HashSet<String> = HashSet::new();
    let mut map: Vec<PackedCand> = Vec::with_capacity(shortlist.len());
    let mut s = String::from("CANDS\n");
    for &i in &order {
        let c = &shortlist[i];
        let cid = unique_cid(c.video_id(), &mut taken);
        let f = &c.features;
        s.push_str(&format!(
            "{cid}|a={artist}|t={title}|src={src}|co={co}|tr={tr}|u={u}|nov={nov}|cont={cont}|comp={comp}|m={m}|ver={ver}\n",
            artist = clean(&c.song.artist, 40),
            title = clean(&c.song.title, 60),
            src = source_tag(c.source),
            co = pct(f.cooc),
            tr = pct(f.transition),
            u = pct(f.seed_affinity),
            nov = pct(f.novelty),
            cont = pct(f.continuation),
            comp = pct(f.completion),
            m = pct(f.music_tier),
            ver = version_label(&c.song.title),
        ));
        map.push(PackedCand {
            cid,
            video_id: c.video_id().to_owned(),
        });
    }
    (s, map)
}

/// Indices into `shortlist`, bucket-interleaved: split into thirds by `base_score`, shuffle each
/// third by a `seed`-derived hash, then round-robin across the thirds. Strong/middle/exploratory
/// candidates alternate, so no contiguous run reflects the score ranking.
fn interleaved_order(shortlist: &[Candidate], seed: &str) -> Vec<usize> {
    let n = shortlist.len();
    if n == 0 {
        return Vec::new();
    }
    let mut by_score: Vec<usize> = (0..n).collect();
    by_score.sort_by(|&a, &b| shortlist[b].base_score.total_cmp(&shortlist[a].base_score));

    let chunk = n.div_ceil(3).max(1);
    let buckets: Vec<Vec<usize>> = by_score
        .chunks(chunk)
        .map(|c| {
            let mut v = c.to_vec();
            // Shuffle within the bucket by a stable seed-derived hash (no input-order leak).
            v.sort_by_key(|&i| hash64(&format!("{seed}|{}", shortlist[i].video_id())));
            v
        })
        .collect();

    let mut order = Vec::with_capacity(n);
    let mut cursor = vec![0usize; buckets.len()];
    while order.len() < n {
        for (bi, b) in buckets.iter().enumerate() {
            if let Some(&idx) = b.get(cursor[bi]) {
                order.push(idx);
                cursor[bi] += 1;
            }
        }
    }
    order
}

/// A short opaque base36 id for a video id, unique within `taken` (rehash with a salt on the
/// rare collision).
fn unique_cid(video_id: &str, taken: &mut HashSet<String>) -> String {
    for k in 0..64 {
        let key = if k == 0 {
            video_id.to_owned()
        } else {
            format!("{video_id}#{k}")
        };
        let cid = base36(hash64(&key), 3);
        if taken.insert(cid.clone()) {
            return cid;
        }
    }
    // Astronomically unlikely; fall back to a longer id derived from the video id.
    let cid = base36(hash64(video_id), 6);
    taken.insert(cid.clone());
    cid
}

fn hash64(s: &str) -> u64 {
    // `DefaultHasher::new()` uses fixed keys → deterministic across runs (unlike `RandomState`).
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn base36(mut n: u64, len: usize) -> String {
    const ALPH: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![b'0'; len];
    for slot in out.iter_mut().rev() {
        *slot = ALPH[(n % 36) as usize];
        n /= 36;
    }
    // ALPH is ASCII, so the bytes are always valid UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// Clamp a [0,1] feature to a 0-100 integer for the wire format.
fn pct(x: f32) -> i32 {
    (x.clamp(0.0, 1.0) * 100.0).round() as i32
}

/// Strip the line-protocol delimiter / newlines and truncate, so a title can't break the pack.
fn clean(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .collect::<String>()
        .replace(['|', '\n', '\r'], " ")
        .trim()
        .to_owned()
}

fn source_tag(src: crate::radio::candidate::CandidateSource) -> &'static str {
    use crate::radio::candidate::CandidateSource as S;
    match src {
        S::WatchPlaylist => "ytm_radio",
        S::ArtistTop => "artist_top",
        S::MoodPlaylist => "mood",
        S::LikedNeighbor => "liked",
        S::HistoryCooc => "cooc",
        S::YtdlpRadio => "search",
    }
}

fn version_label(title: &str) -> &'static str {
    let t = title.to_lowercase();
    if t.contains("live") {
        "live"
    } else if t.contains("acoustic") {
        "acoustic"
    } else if t.contains("remix") {
        "remix"
    } else if t.contains("cover") {
        "cover"
    } else if t.contains("instrumental") {
        "instrumental"
    } else {
        "song"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;
    use crate::radio::candidate::CandidateSource;

    fn cand(id: &str, title: &str, artist: &str, base: f32) -> Candidate {
        let mut c = Candidate::from_song(
            Song::remote(id, title, artist, "3:00"),
            CandidateSource::YtdlpRadio,
            0,
        );
        c.base_score = base;
        c
    }

    #[test]
    fn every_candidate_appears_with_a_unique_cid() {
        let pool: Vec<Candidate> = (0..12)
            .map(|i| {
                cand(
                    &format!("vid{i}"),
                    &format!("Title {i}"),
                    &format!("Artist {i}"),
                    i as f32,
                )
            })
            .collect();
        let (block, map) = build_cands_block(&pool, "seed");

        assert_eq!(map.len(), pool.len());
        // cids are unique.
        let cids: HashSet<&str> = map.iter().map(|p| p.cid.as_str()).collect();
        assert_eq!(cids.len(), pool.len(), "cids must be unique");
        // Every cid appears in the block, and maps back to a real video id.
        for p in &map {
            assert!(block.contains(&format!("{}|", p.cid)));
            assert!(p.video_id.starts_with("vid"));
        }
        // cids are not the raw video ids (opacity) and not sequential c01/c02.
        assert!(!map.iter().any(|p| p.cid == p.video_id));
    }

    #[test]
    fn order_is_deterministic_for_a_given_seed() {
        let pool: Vec<Candidate> = (0..9)
            .map(|i| {
                cand(
                    &format!("v{i}"),
                    &format!("T{i}"),
                    &format!("A{i}"),
                    (9 - i) as f32,
                )
            })
            .collect();
        let (a, _) = build_cands_block(&pool, "seed-x");
        let (b, _) = build_cands_block(&pool, "seed-x");
        assert_eq!(a, b, "same input + seed → identical pack");
    }

    #[test]
    fn pack_does_not_emit_in_descending_score_order() {
        // base_score strictly increasing with index; the emitted order must not be the plain
        // score sort (interleaving breaks the position-bias signal).
        let pool: Vec<Candidate> = (0..9)
            .map(|i| {
                cand(
                    &format!("v{i}"),
                    &format!("T{i}"),
                    &format!("A{i}"),
                    i as f32,
                )
            })
            .collect();
        let (block, map) = build_cands_block(&pool, "seed");
        // The first emitted candidate should not always be the single highest base_score one.
        // (With interleaving, the top-bucket leader is shuffled, so v8 rarely lands first.)
        let first_cid = block.lines().nth(1).unwrap().split('|').next().unwrap();
        let first_vid = &map.iter().find(|p| p.cid == first_cid).unwrap().video_id;
        assert_ne!(
            first_vid, "v8",
            "highest score should not deterministically lead"
        );
    }

    #[test]
    fn evidence_line_has_scores_and_is_pipe_safe() {
        let mut c = cand("v1", "Song | With Pipe", "Artist", 0.5);
        c.features.cooc = 0.83;
        c.features.transition = 0.5;
        let (block, _) = build_cands_block(std::slice::from_ref(&c), "seed");
        assert!(block.contains("co=83"));
        assert!(block.contains("tr=50"));
        // The pipe inside the title was stripped so it can't break the protocol: the line must
        // have exactly the 12 fields' worth of delimiters (11 pipes), not one extra.
        let line = block.lines().nth(1).unwrap();
        assert_eq!(
            line.matches('|').count(),
            11,
            "title pipe must not leak as a delimiter"
        );
        assert!(
            !line.contains("t=Song |"),
            "raw pipe must be stripped from the title"
        );
    }
}
