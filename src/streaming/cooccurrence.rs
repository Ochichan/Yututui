//! A behavioral co-occurrence graph (SPPMI) built in-memory from the raw play log.
//!
//! The signal is "which tracks the user plays near each other" — the strongest available
//! cue for what to play next. It is computed from the *raw* play sequence **with repeats**
//! (not the recency-deduped history): de-duping would destroy the repeat-play affinity that
//! dominates real listening. Edges are Shifted Positive PMI: `max(PMI − ln k, 0)` (k=1 is
//! plain PPMI). Forward edges (earlier→later) carry full weight; reverse edges are damped.

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::streaming::config::CoocConfig;

/// A directed SPPMI graph: `edges[a][b]` is the affinity of playing `b` given `a`.
#[derive(Debug, Default)]
pub struct Cooc {
    edges: HashMap<String, HashMap<String, f32>>,
}

impl Cooc {
    /// Build the graph from the raw play log (ordered `(video_id, unix_ts)` events). The log
    /// is split into sessions on inactivity gaps and bounded windows, co-occurrences are
    /// distance-weighted (`1/distance`), then converted to SPPMI edges.
    pub fn build(play_log: &VecDeque<(String, i64)>, cfg: &CoocConfig) -> Self {
        let gap = cfg.session_gap_min.max(0).saturating_mul(60);
        let window = cfg.window.max(1);

        // Weighted co-occurrence counts: co[a][b].
        let mut co: HashMap<String, HashMap<String, f32>> = HashMap::new();
        let add = |co: &mut HashMap<String, HashMap<String, f32>>, a: &str, b: &str, w: f32| {
            *co.entry(a.to_owned())
                .or_default()
                .entry(b.to_owned())
                .or_insert(0.0) += w;
        };

        // Walk sessions: a session breaks on a long idle gap or once it reaches session_max.
        let events: Vec<&(String, i64)> = play_log.iter().collect();
        let mut start = 0usize;
        while start < events.len() {
            let mut end = start + 1;
            while end < events.len()
                && end - start < cfg.session_max.max(1)
                && events[end].1.saturating_sub(events[end - 1].1) <= gap
            {
                end += 1;
            }
            let session = &events[start..end];
            for i in 0..session.len() {
                let a = session[i].0.as_str();
                for (off, ev) in session.iter().enumerate().skip(i + 1).take(window) {
                    let b = ev.0.as_str();
                    if a == b {
                        continue;
                    }
                    let dist = (off - i) as f32;
                    let w = 1.0 / dist;
                    add(&mut co, a, b, w); // forward (a precedes b)
                    add(&mut co, b, a, w * cfg.reverse.max(0.0)); // damped reverse
                }
            }
            start = end;
        }

        Self::from_counts(co, cfg.sppmi_k.max(1.0))
    }

    /// Convert raw co-occurrence counts into SPPMI edges.
    fn from_counts(co: HashMap<String, HashMap<String, f32>>, k: f32) -> Self {
        let mut total = 0.0f32;
        let mut rowsum: HashMap<&str, f32> = HashMap::new();
        let mut colsum: HashMap<&str, f32> = HashMap::new();
        for (a, row) in &co {
            for (b, &w) in row {
                total += w;
                *rowsum.entry(a.as_str()).or_insert(0.0) += w;
                *colsum.entry(b.as_str()).or_insert(0.0) += w;
            }
        }
        let shift = k.ln();
        let mut edges: HashMap<String, HashMap<String, f32>> = HashMap::new();
        if total > 0.0 {
            for (a, row) in &co {
                for (b, &w) in row {
                    let (ra, cb) = (rowsum[a.as_str()], colsum[b.as_str()]);
                    if ra <= 0.0 || cb <= 0.0 {
                        continue;
                    }
                    // PMI = ln( P(a,b) / (P(a)P(b)) ) = ln( w * total / (rowsum * colsum) ).
                    let pmi = (w * total / (ra * cb)).ln();
                    let sppmi = (pmi - shift).max(0.0);
                    if sppmi > 0.0 {
                        edges.entry(a.clone()).or_default().insert(b.clone(), sppmi);
                    }
                }
            }
        }
        Cooc { edges }
    }

    /// The SPPMI affinity of playing `b` given `a` (0 if unknown).
    pub fn weight(&self, a: &str, b: &str) -> f32 {
        self.edges
            .get(a)
            .and_then(|m| m.get(b))
            .copied()
            .unwrap_or(0.0)
    }

    /// Affinity of candidate `id` to a set of recent context tracks: the strongest forward
    /// edge from any context track into `id` (context played first → `id` follows).
    pub fn affinity(&self, id: &str, context: &[String]) -> f32 {
        context
            .iter()
            .map(|c| self.weight(c, id))
            .fold(0.0, f32::max)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(events: &[(&str, i64)]) -> VecDeque<(String, i64)> {
        events.iter().map(|(v, t)| ((*v).to_owned(), *t)).collect()
    }

    #[test]
    fn co_occurring_tracks_get_positive_edges() {
        // a→b appears repeatedly in tight succession; both directions should link positively.
        // (Note: SPPMI normalizes by marginal frequency, so for symmetric alternating data the
        // forward edge is NOT guaranteed ≥ the reverse — only that co-occurring pairs are > 0.)
        let cfg = CoocConfig::default();
        let pl = log(&[
            ("a", 0),
            ("b", 10),
            ("a", 100),
            ("b", 110),
            ("a", 200),
            ("b", 210),
        ]);
        let c = Cooc::build(&pl, &cfg);
        assert!(c.weight("a", "b") > 0.0);
        assert!(c.weight("b", "a") > 0.0);
        // A track never seen has no edge.
        assert_eq!(c.weight("a", "zzz"), 0.0);
    }

    #[test]
    fn session_gap_splits_the_sequence() {
        // 'a' and 'b' are separated by a > 20-min gap → never co-occur.
        let cfg = CoocConfig::default();
        let pl = log(&[("a", 0), ("b", 30 * 60)]);
        let c = Cooc::build(&pl, &cfg);
        assert_eq!(c.weight("a", "b"), 0.0);
    }

    #[test]
    fn window_bounds_co_occurrence() {
        let cfg = CoocConfig {
            window: 1,
            ..CoocConfig::default()
        };
        // Within one session: a,b,c. window=1 → a-b and b-c pair, but a-c do not.
        let pl = log(&[("a", 0), ("b", 10), ("c", 20)]);
        let c = Cooc::build(&pl, &cfg);
        assert!(c.weight("a", "b") > 0.0);
        assert!(c.weight("b", "c") > 0.0);
        assert_eq!(c.weight("a", "c"), 0.0);
    }

    #[test]
    fn affinity_takes_the_strongest_context_edge() {
        let cfg = CoocConfig::default();
        let pl = log(&[("seed", 0), ("target", 10), ("seed", 100), ("target", 110)]);
        let c = Cooc::build(&pl, &cfg);
        let direct = c.weight("seed", "target");
        assert!(direct > 0.0);
        // affinity over a context containing the seed equals the seed→target edge.
        assert_eq!(
            c.affinity("target", &["seed".to_owned(), "other".to_owned()]),
            direct
        );
    }

    #[test]
    fn empty_log_yields_empty_graph() {
        let c = Cooc::build(&VecDeque::new(), &CoocConfig::default());
        assert!(c.is_empty());
        assert_eq!(c.weight("a", "b"), 0.0);
    }
}
