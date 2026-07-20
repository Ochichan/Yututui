//! Session-only provenance for recommendation-owned queue rows.
//!
//! Both playback owners use this ledger so a `video_id` has one current WhyGem answer,
//! bounded independently from the queue implementation. The wire model remains owned by
//! `remote::proto`; this module only owns lifecycle and revision semantics.

use std::collections::HashSet;

/// Keep the provenance store bounded to the same maximum cardinality as the play queue.
pub const WHY_GEM_MAX: usize = 999;

#[derive(Debug, Clone)]
pub struct WhyGemLedger<T> {
    entries: Vec<(String, T)>,
    revision: u64,
    reconciled_queue_revision: Option<u64>,
}

impl<T> Default for WhyGemLedger<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            revision: 0,
            reconciled_queue_revision: None,
        }
    }
}

impl<T> WhyGemLedger<T> {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, video_id: &str) -> bool {
        self.get(video_id).is_some()
    }

    pub fn get(&self, video_id: &str) -> Option<&T> {
        self.entries
            .iter()
            .find_map(|(id, model)| (id == video_id).then_some(model))
    }

    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|(id, _)| id.clone()).collect()
    }

    fn bump_revision(&mut self) {
        // A session cannot realistically exhaust u64, but saturation keeps the advertised
        // revision monotonic instead of allowing a wrap to look older to subscribers.
        self.revision = self.revision.saturating_add(1);
    }
}

impl<T: PartialEq> WhyGemLedger<T> {
    /// Insert or replace the latest explanation for `video_id`.
    ///
    /// Existing keys retain their insertion position. New keys evict the oldest entry when
    /// the fixed session cap is full. An identical value is an exact no-op, including revision.
    pub fn upsert(&mut self, video_id: String, model: T) -> bool {
        if !self.upsert_without_revision(video_id, model) {
            return false;
        }
        self.bump_revision();
        true
    }

    /// Apply one recommendation batch with one observable revision transition.
    pub fn upsert_many<I>(&mut self, entries: I) -> bool
    where
        I: IntoIterator<Item = (String, T)>,
        T: Clone,
    {
        // Replay the exact ordered batch against a bounded candidate. Comparing only its final
        // state preserves cap/eviction semantics (including keys repeated around an eviction)
        // while keeping a round trip back to the original ledger revision-neutral.
        let mut candidate = self.clone();
        for (video_id, model) in entries {
            candidate.upsert_without_revision(video_id, model);
        }
        if candidate.entries == self.entries {
            return false;
        }
        self.entries = candidate.entries;
        self.reconciled_queue_revision = candidate.reconciled_queue_revision;
        self.bump_revision();
        true
    }

    /// Forget one video's recommendation origin. Duplicate queue rows intentionally share this
    /// decision because the v1 contract is keyed by `video_id`, not queue occurrence.
    pub fn forget(&mut self, video_id: &str) -> bool {
        let Some(index) = self.entries.iter().position(|(id, _)| id == video_id) else {
            return false;
        };
        self.entries.remove(index);
        self.bump_revision();
        true
    }

    /// Forget a batch of manual additions with one observable revision transition.
    pub fn forget_many<I, S>(&mut self, video_ids: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let forgotten: HashSet<String> = video_ids
            .into_iter()
            .map(|id| id.as_ref().to_owned())
            .collect();
        if forgotten.is_empty() {
            return false;
        }
        let before = self.entries.len();
        self.entries
            .retain(|(video_id, _)| !forgotten.contains(video_id));
        if self.entries.len() == before {
            return false;
        }
        self.bump_revision();
        true
    }

    pub fn clear(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.entries.clear();
        self.bump_revision();
        true
    }

    /// Replace the complete ledger, deduplicating by `video_id` with the last value winning.
    pub fn replace<I>(&mut self, entries: I) -> bool
    where
        I: IntoIterator<Item = (String, T)>,
    {
        let mut replacement = Self::default();
        for (video_id, model) in entries {
            replacement.upsert_without_revision(video_id, model);
        }
        if self.entries == replacement.entries {
            return false;
        }
        self.entries = replacement.entries;
        self.reconciled_queue_revision = None;
        self.bump_revision();
        true
    }

    /// Drop explanations whose video is no longer represented anywhere in the live queue.
    /// Repeated owner turns at the same queue revision are allocation-free no-ops.
    pub fn retain_video_ids<'a, I>(&mut self, queue_revision: u64, video_ids: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        if self.reconciled_queue_revision == Some(queue_revision) {
            return false;
        }
        self.reconciled_queue_revision = Some(queue_revision);
        if self.entries.is_empty() {
            return false;
        }
        let live: HashSet<&str> = video_ids.into_iter().collect();
        let before = self.entries.len();
        self.entries
            .retain(|(video_id, _)| live.contains(video_id.as_str()));
        if self.entries.len() == before {
            return false;
        }
        self.bump_revision();
        true
    }

    fn upsert_without_revision(&mut self, video_id: String, model: T) -> bool {
        if let Some((_, current)) = self.entries.iter_mut().find(|(id, _)| *id == video_id) {
            if *current == model {
                return false;
            }
            *current = model;
            return true;
        }
        if self.entries.len() >= WHY_GEM_MAX {
            self.entries.remove(0);
        }
        self.reconciled_queue_revision = None;
        self.entries.push((video_id, model));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_upsert_is_not_an_observable_mutation() {
        let mut ledger = WhyGemLedger::default();
        assert!(ledger.upsert("a".to_owned(), 1));
        let revision = ledger.revision();
        assert!(!ledger.upsert("a".to_owned(), 1));
        assert_eq!(ledger.revision(), revision);

        assert!(ledger.upsert("a".to_owned(), 2));
        assert!(ledger.revision() > revision);
        assert_eq!(ledger.get("a"), Some(&2));
        assert_eq!(ledger.ids(), vec!["a"]);
    }

    #[test]
    fn batches_bump_revision_once_and_last_duplicate_wins() {
        let mut ledger = WhyGemLedger::default();
        assert!(ledger.upsert_many([
            ("a".to_owned(), 1),
            ("b".to_owned(), 2),
            ("a".to_owned(), 3),
        ]));
        assert_eq!(ledger.revision(), 1);
        assert_eq!(ledger.ids(), vec!["a", "b"]);
        assert_eq!(ledger.get("a"), Some(&3));
    }

    #[test]
    fn ledger_evicts_the_oldest_entry_at_the_queue_cap() {
        let mut ledger = WhyGemLedger::default();
        ledger.upsert_many((0..WHY_GEM_MAX).map(|i| (format!("v{i}"), i)));
        let revision = ledger.revision();
        assert!(ledger.upsert("new".to_owned(), WHY_GEM_MAX));
        assert_eq!(ledger.len(), WHY_GEM_MAX);
        assert!(!ledger.contains("v0"));
        assert!(ledger.contains("new"));
        assert_eq!(ledger.revision(), revision + 1);
    }

    #[test]
    fn retain_and_forget_only_bump_when_an_entry_disappears() {
        let mut ledger = WhyGemLedger::default();
        ledger.upsert_many([("a".to_owned(), 1), ("b".to_owned(), 2)]);
        let revision = ledger.revision();

        assert!(!ledger.retain_video_ids(1, ["a", "b", "manual"]));
        assert!(!ledger.retain_video_ids(1, std::iter::empty()));
        assert!(!ledger.forget("missing"));
        assert_eq!(ledger.revision(), revision);

        assert!(ledger.retain_video_ids(2, ["b"]));
        assert_eq!(ledger.ids(), vec!["b"]);
        assert!(ledger.forget_many(["b", "b"]));
        assert!(ledger.is_empty());
    }

    #[test]
    fn replace_is_atomic_and_deduplicates_with_the_latest_value() {
        let mut ledger = WhyGemLedger::default();
        ledger.upsert("old".to_owned(), 0);
        let revision = ledger.revision();
        assert!(ledger.replace([
            ("a".to_owned(), 1),
            ("a".to_owned(), 2),
            ("b".to_owned(), 3),
        ]));
        assert_eq!(ledger.revision(), revision + 1);
        assert_eq!(ledger.ids(), vec!["a", "b"]);
        assert_eq!(ledger.get("a"), Some(&2));

        let revision = ledger.revision();
        assert!(!ledger.replace([("a".to_owned(), 2), ("b".to_owned(), 3)]));
        assert_eq!(ledger.revision(), revision);
    }

    #[test]
    fn batch_round_trip_is_not_an_observable_mutation() {
        let mut ledger = WhyGemLedger::default();
        ledger.upsert("a".to_owned(), 1);
        let revision = ledger.revision();

        assert!(!ledger.upsert_many([("a".to_owned(), 2), ("a".to_owned(), 1)]));
        assert_eq!(ledger.get("a"), Some(&1));
        assert_eq!(ledger.revision(), revision);
    }

    #[test]
    fn repeated_key_around_cap_eviction_keeps_the_last_value() {
        let mut ledger = WhyGemLedger::default();
        ledger.upsert_many((0..WHY_GEM_MAX).map(|index| (format!("v{index}"), index)));

        assert!(ledger.upsert_many([
            ("v0".to_owned(), WHY_GEM_MAX + 1),
            ("new".to_owned(), WHY_GEM_MAX + 2),
            ("v0".to_owned(), WHY_GEM_MAX + 3),
        ]));
        assert_eq!(ledger.get("v0"), Some(&(WHY_GEM_MAX + 3)));
        assert!(ledger.contains("new"));
        assert!(!ledger.contains("v1"));
        assert_eq!(ledger.len(), WHY_GEM_MAX);
    }

    #[test]
    fn full_cap_rotation_back_to_the_original_is_revision_neutral() {
        let mut ledger = WhyGemLedger::default();
        ledger.upsert_many((0..WHY_GEM_MAX).map(|index| (format!("v{index}"), index)));
        let revision = ledger.revision();
        let original_ids = ledger.ids();
        let batch = std::iter::once(("new".to_owned(), WHY_GEM_MAX))
            .chain((0..WHY_GEM_MAX).map(|index| (format!("v{index}"), index)));

        assert!(!ledger.upsert_many(batch));
        assert_eq!(ledger.revision(), revision);
        assert_eq!(ledger.ids(), original_ids);
    }
}
