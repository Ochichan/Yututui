use super::{Queue, QueueSnapshot, Repeat};
use crate::api::Song;

/// Typed input for an admission-atomic queue replacement. `shuffle_override` models the
/// existing eager sequence `set(songs, start)` followed by an optional `set_shuffle(value)`;
/// preparation computes that sequence without touching the live queue or global revision.
#[derive(Clone)]
pub(crate) struct QueueReplacementDraft {
    songs: Vec<Song>,
    start: usize,
    shuffle_override: Option<bool>,
}

impl QueueReplacementDraft {
    pub(crate) fn new(songs: Vec<Song>, start: usize, shuffle_override: Option<bool>) -> Self {
        Self {
            songs,
            start,
            shuffle_override,
        }
    }
}

/// The observable result of an insertion-style queue mutation.
///
/// `requested` deliberately includes tracks rejected by the hard cap, while `added` reports
/// what the prepared state actually contains. `selected_cursor` is the play-order position the
/// mutation selects before invalid-track filtering by the App transition layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueMutationOutcome {
    requested: usize,
    added: usize,
    selected_cursor: Option<usize>,
}

impl QueueMutationOutcome {
    pub(crate) fn requested(self) -> usize {
        self.requested
    }

    pub(crate) fn added(self) -> usize {
        self.added
    }

    pub(crate) fn selected_cursor(self) -> Option<usize> {
        self.selected_cursor
    }
}

/// Playback work required after a prepared removal is committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueRemovalPlayback {
    /// The current song survives, so no player command is required.
    Unchanged,
    /// The selected survivor must be loaded after admission.
    LoadSelected,
    /// The removed range reached the effective end and playback must stop.
    Stop,
}

/// Observable facts calculated while preparing an inclusive range removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueRemovalOutcome {
    removed: usize,
    popup_cursor: usize,
    playback: QueueRemovalPlayback,
}

impl QueueRemovalOutcome {
    pub(crate) fn removed(self) -> usize {
        self.removed
    }

    pub(crate) fn popup_cursor(self) -> usize {
        self.popup_cursor
    }

    pub(crate) fn playback(self) -> QueueRemovalPlayback {
        self.playback
    }
}

/// Fully prepared queue state plus the live-queue identity it was derived from.
///
/// The RNG is part of the plan: shuffled preparation advances only a clone, and successful
/// commit installs that exact advanced state so subsequent shuffles remain deterministic.
#[derive(Clone)]
pub(crate) struct QueueMutationPlan {
    expected_rev: u64,
    expected_cursor: usize,
    expected_video_id: Option<String>,
    expected_repeat: Repeat,
    songs: Vec<Song>,
    order: Vec<usize>,
    cursor: usize,
    shuffle: bool,
    repeat: Repeat,
    rng: fastrand::Rng,
}

impl QueueMutationPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.songs.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.songs.len()
    }

    pub(crate) fn cursor_pos(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn current(&self) -> Option<&Song> {
        self.song_at_cursor(self.cursor)
    }

    pub(crate) fn song_at_cursor(&self, cursor: usize) -> Option<&Song> {
        let idx = *self.order.get(cursor)?;
        self.songs.get(idx)
    }

    #[cfg(test)]
    pub(crate) fn ordered_iter(&self) -> impl Iterator<Item = &Song> {
        self.order.iter().filter_map(|&idx| self.songs.get(idx))
    }

    #[cfg(test)]
    pub(crate) fn repeat(&self) -> Repeat {
        self.repeat
    }

    #[cfg(test)]
    pub(crate) fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub(crate) fn plan_next_cursor(&self, from: usize) -> Option<usize> {
        if self.songs.is_empty() || from >= self.order.len() {
            return None;
        }
        if from + 1 < self.order.len() {
            Some(from + 1)
        } else if self.repeat == Repeat::All {
            Some(0)
        } else {
            None
        }
    }

    pub(crate) fn select_cursor(&mut self, cursor: usize) {
        assert!(
            cursor < self.order.len(),
            "prepared queue cursor is out of range"
        );
        self.cursor = cursor;
    }
}

impl Queue {
    /// Clone the complete live queue for revision-free mutation preparation. Constructing the
    /// scratch state directly (never through [`Default`]) leaves the owner-global revision
    /// source untouched, while cloning the RNG makes shuffled preparation deterministic and
    /// rollback-safe.
    fn mutation_scratch(&self) -> Self {
        Self {
            songs: self.songs.clone(),
            order: self.order.clone(),
            cursor: self.cursor,
            shuffle: self.shuffle,
            repeat: self.repeat,
            rev: self.rev,
            rng: self.rng.clone(),
            #[cfg(test)]
            revision_bumps: self.revision_bumps,
        }
    }

    fn mutation_plan(&self, scratch: Self) -> QueueMutationPlan {
        QueueMutationPlan {
            expected_rev: self.rev,
            expected_cursor: self.cursor,
            expected_video_id: self.current().map(|song| song.video_id.clone()),
            expected_repeat: self.repeat,
            songs: scratch.songs,
            order: scratch.order,
            cursor: scratch.cursor,
            shuffle: scratch.shuffle,
            repeat: scratch.repeat,
            rng: scratch.rng,
        }
    }

    /// Prepare the exact final state of `set` plus an optional shuffle override without
    /// mutating the live queue, its revision, its RNG, or the global revision source.
    pub(crate) fn prepare_replacement(&self, draft: QueueReplacementDraft) -> QueueMutationPlan {
        let QueueReplacementDraft {
            songs,
            start,
            shuffle_override,
        } = draft;
        let mut scratch = self.mutation_scratch();
        scratch.replace_without_revision(songs, start);
        if let Some(shuffle) = shuffle_override {
            scratch.set_shuffle_without_revision(shuffle);
        }
        self.mutation_plan(scratch)
    }

    /// Prepare `play_now_many` without changing any live state. The accepted prefix is inserted
    /// after the current play-order position and its first track becomes selected, exactly like
    /// the eager operation.
    pub(crate) fn prepare_play_now_many(
        &self,
        more: Vec<Song>,
    ) -> (QueueMutationPlan, QueueMutationOutcome) {
        let requested = more.len();
        let mut scratch = self.mutation_scratch();
        let added = scratch.play_now_many_without_revision(more);
        let selected_cursor = (added > 0).then_some(scratch.cursor);
        let outcome = QueueMutationOutcome {
            requested,
            added,
            selected_cursor,
        };
        (self.mutation_plan(scratch), outcome)
    }

    /// Prepare the idle-enqueue sequence `extend(more); goto(old_len)` without mutating the live
    /// queue. With shuffle enabled, only the appended block consumes the cloned RNG and the
    /// selected cursor remains the first play-order position in that shuffled block.
    pub(crate) fn prepare_idle_enqueue(
        &self,
        more: Vec<Song>,
    ) -> (QueueMutationPlan, QueueMutationOutcome) {
        let requested = more.len();
        let old_len = self.songs.len();
        let mut scratch = self.mutation_scratch();
        let added = scratch.extend_without_revision(more);
        let selected_cursor = if added == 0 {
            None
        } else {
            let cursor = old_len.min(scratch.order.len().saturating_sub(1));
            scratch.cursor = cursor;
            Some(cursor)
        };
        let outcome = QueueMutationOutcome {
            requested,
            added,
            selected_cursor,
        };
        (self.mutation_plan(scratch), outcome)
    }

    /// Prepare one inclusive play-order range removal without changing live membership, cursor,
    /// RNG, or revision state. A current-inclusive tail removal wraps only for repeat-all;
    /// otherwise its prepared queue is retained but playback is stopped after admission.
    pub(crate) fn prepare_remove_range(
        &self,
        lo: usize,
        hi: usize,
    ) -> Option<(QueueMutationPlan, QueueRemovalOutcome)> {
        let len = self.order.len();
        if len == 0 || lo > hi {
            return None;
        }
        let lo = lo.min(len - 1);
        let hi = hi.min(len - 1);
        let removed = hi - lo + 1;
        let removes_current = lo <= self.cursor && self.cursor <= hi;
        let (playback, selected_cursor) = if !removes_current {
            (QueueRemovalPlayback::Unchanged, None)
        } else if hi + 1 < len {
            (QueueRemovalPlayback::LoadSelected, Some(lo))
        } else if removed < len && self.repeat == Repeat::All {
            (QueueRemovalPlayback::LoadSelected, Some(0))
        } else {
            (QueueRemovalPlayback::Stop, None)
        };

        let mut scratch = self.mutation_scratch();
        scratch.remove_range_without_revision(lo, hi);
        if let Some(cursor) = selected_cursor {
            scratch.cursor = cursor;
        }
        let popup_cursor = lo.min(scratch.order.len().saturating_sub(1));
        Some((
            self.mutation_plan(scratch),
            QueueRemovalOutcome {
                removed,
                popup_cursor,
                playback,
            },
        ))
    }

    /// Prepare snapshot validation/restoration as a pure queue mutation. Mode-switch callers can
    /// inspect and admission-gate this plan later without advancing the live shuffle RNG or rev.
    pub(crate) fn prepare_snapshot_restore(&self, snapshot: QueueSnapshot) -> QueueMutationPlan {
        let mut scratch = self.mutation_scratch();
        scratch.restore_snapshot_without_revision(snapshot);
        self.mutation_plan(scratch)
    }

    /// Validate a mutation against the exact queue identity captured during preparation.
    /// Callers which have other pre-commit mutations can establish this guard before any of
    /// them; [`commit_mutation`](Self::commit_mutation) validates it again defensively.
    pub(crate) fn validate_mutation(&self, plan: &QueueMutationPlan) {
        self.validate_planned_transition(
            plan.expected_rev,
            plan.expected_cursor,
            plan.expected_video_id.as_deref(),
            None,
        );
        assert_eq!(
            self.repeat, plan.expected_repeat,
            "queue repeat mode changed before prepared mutation commit"
        );
    }

    /// Check whether a prepared mutation still belongs to this live queue without panicking.
    /// Runtime admission uses this before mpv sees the paired command batch; commit retains the
    /// asserting validator above as a defensive owner-lane invariant.
    pub(crate) fn mutation_matches(&self, plan: &QueueMutationPlan) -> bool {
        self.planned_transition_matches(
            plan.expected_rev,
            plan.expected_cursor,
            plan.expected_video_id.as_deref(),
            None,
        ) && self.repeat == plan.expected_repeat
    }

    /// Atomically install a prepared queue state and mint exactly one membership/order revision.
    pub(crate) fn commit_mutation(&mut self, plan: QueueMutationPlan) {
        self.validate_mutation(&plan);
        let QueueMutationPlan {
            songs,
            order,
            cursor,
            shuffle,
            repeat,
            rng,
            ..
        } = plan;
        self.songs = songs;
        self.order = order;
        self.cursor = cursor;
        self.shuffle = shuffle;
        self.repeat = repeat;
        self.rng = rng;
        self.bump_rev();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn songs(n: usize) -> Vec<Song> {
        (0..n)
            .map(|i| Song::remote(i.to_string(), format!("title-{i}"), "artist", "3:00"))
            .collect()
    }

    fn ordered_ids(queue: &Queue) -> Vec<String> {
        queue
            .ordered()
            .into_iter()
            .map(|song| song.video_id.clone())
            .collect()
    }

    #[test]
    fn remove_range_is_pure_and_commits_multiple_rows_with_one_revision() {
        let mut queue = Queue::default();
        queue.set(songs(6), 2);
        let before_ids = ordered_ids(&queue);
        let before_rev = queue.rev();
        let before_bumps = queue.revision_bumps;

        let (plan, outcome) = queue.prepare_remove_range(1, 3).expect("valid range");

        assert_eq!(ordered_ids(&queue), before_ids);
        assert_eq!(queue.rev(), before_rev);
        assert_eq!(queue.revision_bumps, before_bumps);
        assert_eq!(outcome.removed(), 3);
        assert_eq!(outcome.popup_cursor(), 1);
        assert_eq!(outcome.playback(), QueueRemovalPlayback::LoadSelected);
        assert_eq!(plan.current().map(|song| song.video_id.as_str()), Some("4"));

        queue.commit_mutation(plan);
        assert_eq!(ordered_ids(&queue), vec!["0", "4", "5"]);
        assert_eq!(
            queue.current().map(|song| song.video_id.as_str()),
            Some("4")
        );
        assert_eq!(queue.revision_bumps, before_bumps + 1);
    }

    #[test]
    fn remove_range_classifies_unchanged_repeat_wrap_and_full_delete() {
        let mut queue = Queue::default();
        queue.set(songs(4), 1);
        let (_, unchanged) = queue.prepare_remove_range(3, 3).expect("valid range");
        assert_eq!(unchanged.playback(), QueueRemovalPlayback::Unchanged);

        queue.repeat = Repeat::All;
        queue.goto(3);
        let (wrapped, wrap) = queue.prepare_remove_range(3, 3).expect("valid range");
        assert_eq!(wrap.playback(), QueueRemovalPlayback::LoadSelected);
        assert_eq!(
            wrapped.current().map(|song| song.video_id.as_str()),
            Some("0")
        );

        let (empty, full) = queue.prepare_remove_range(0, 99).expect("clamped range");
        assert_eq!(full.playback(), QueueRemovalPlayback::Stop);
        assert!(empty.is_empty());
        assert_eq!(full.popup_cursor(), 0);
    }

    #[test]
    fn snapshot_restore_preparation_is_pure_and_commit_bumps_once() {
        let mut queue = Queue::default();
        queue.set(songs(2), 0);
        queue.seed_rng(77);
        let before_ids = ordered_ids(&queue);
        let before_rev = queue.rev();
        let before_bumps = queue.revision_bumps;
        let plan = queue.prepare_snapshot_restore(QueueSnapshot {
            songs: songs(3),
            order: vec![0, 0, 2],
            cursor: 2,
            shuffle: true,
            repeat: Repeat::All,
        });

        assert_eq!(ordered_ids(&queue), before_ids);
        assert_eq!(queue.rev(), before_rev);
        assert_eq!(queue.revision_bumps, before_bumps);
        assert_eq!(plan.len(), 3);
        assert!(plan.shuffle());
        assert_eq!(plan.repeat(), Repeat::All);

        queue.commit_mutation(plan);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.revision_bumps, before_bumps + 1);
        let mut ids = ordered_ids(&queue);
        ids.sort();
        assert_eq!(ids, vec!["0", "1", "2"]);
    }

    #[test]
    fn repeat_change_rejects_stale_removal_plan_without_overwriting_new_mode() {
        let mut queue = Queue::default();
        queue.set(songs(3), 2);
        let before_ids = ordered_ids(&queue);
        let (plan, outcome) = queue.prepare_remove_range(2, 2).expect("valid range");
        assert_eq!(outcome.playback(), QueueRemovalPlayback::Stop);

        queue.cycle_repeat();
        assert_eq!(queue.repeat, Repeat::All);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                queue.commit_mutation(plan);
            }))
            .is_err(),
            "plan prepared under Repeat::Off must become stale"
        );

        assert_eq!(queue.repeat, Repeat::All);
        assert_eq!(ordered_ids(&queue), before_ids);
    }
}
