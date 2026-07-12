//! The play queue: ordering, shuffle, repeat, and next/previous navigation.
//!
//! The model separates *what's in the queue* from *the order it plays in*:
//! `songs` holds tracks in the order they were added, while `order` is a permutation of
//! indices into `songs` describing the actual play sequence. `cursor` points at the
//! current track within `order`. Shuffle just reshuffles `order` (keeping the current
//! track current); turning it off restores natural order. This keeps every operation a
//! pure index manipulation — easy to reason about and unit-test.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::api::Song;
use serde::{Deserialize, Serialize};

pub(crate) mod mutation;
pub(crate) use mutation::{QueueMutationPlan, QueueRemovalPlayback, QueueReplacementDraft};

/// Hard cap on queued tracks (priority #1: bounded memory).
const MAX: usize = 999;

/// Owner-global queue revision source (docs/gui/02 §14). One counter per process —
/// deliberately NOT per-`Queue`: radio mode and `--resume` swap whole queues through
/// snapshots, and two independently-counted queues could land on the same rev across a
/// stash/swap, making a change invisible to rev-comparing observers (the v8 publisher).
/// Drawing every assignment from one monotonic source makes every swap observable.
/// The value is runtime-only: it is never serialized ([`QueueSnapshot`] has no rev).
static QUEUE_REV: AtomicU64 = AtomicU64::new(1);

fn next_queue_rev() -> u64 {
    QUEUE_REV.fetch_add(1, Ordering::Relaxed)
}

/// Repeat mode, cycled by the `r` key.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    /// The next mode in the Off → All → One → Off cycle.
    pub fn cycled(self) -> Self {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }

    /// Whether any repeat mode is active (i.e. not `Off`). Named so the streaming⇔repeat
    /// mutual-exclusion invariant reads the same everywhere it is checked — in the App
    /// reducers and, in lockstep, in `daemon::engine` — instead of a bare `!= Repeat::Off`.
    pub fn is_on(self) -> bool {
        self != Repeat::Off
    }

    /// The streaming⇔repeat mutual-exclusion invariant for a **set-to-`self`** action, in one
    /// place: refuse turning repeat on while a station / autoplay feed is active. `true` = the
    /// change must be blocked. Setting repeat Off, or any change while not streaming, is always
    /// allowed. Used by every OS-widget/`set-property` repeat path (App + daemon) so they can't
    /// drift. NB: the App passes its raw `autoplay_streaming` preference here, matching today.
    pub fn set_blocked_by_streaming(self, streaming: bool) -> bool {
        self.is_on() && streaming
    }

    /// The same invariant for a **cycle** action (Off→All→One→Off). The only step that turns
    /// repeat on is Off→All, so the cycle is blocked exactly when it starts from `Off` while
    /// `streaming`. `true` = block the cycle. Used by both cycle paths (App + daemon).
    pub fn cycle_blocked_by_streaming(self, streaming: bool) -> bool {
        self == Repeat::Off && streaming
    }
}

/// A bounded play queue with shuffle and repeat.
pub struct Queue {
    songs: Vec<Song>,
    /// Permutation of `0..songs.len()` giving the play order.
    order: Vec<usize>,
    /// Index into `order` of the current track (meaningless when empty).
    cursor: usize,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Membership/order revision, assigned from [`QUEUE_REV`]. Bumped by every mutation
    /// of `songs`/`order` — including [`restore_snapshot`](Self::restore_snapshot) — and
    /// deliberately NOT by cursor moves (`next`/`prev`/`goto`): the cursor rides
    /// `PlayerModel.queue_pos` on the wire, so a track advance must not look like a
    /// queue change. Private: only mutators may touch it.
    rev: u64,
    rng: fastrand::Rng,
    /// Per-instance instrumentation makes exact revision-mint assertions deterministic even
    /// though the production revision source is process-global and tests run concurrently.
    #[cfg(test)]
    revision_bumps: usize,
}

/// A point-in-time copy of the queue's playable state.
///
/// `SessionCache` persists this so a later TUI or daemon can resume the actual queue, not just
/// the most recent history item. Fields are crate-visible for that persistence boundary; restore
/// still goes through [`Queue::restore_snapshot`], which validates the order/cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueSnapshot {
    pub(crate) songs: Vec<Song>,
    pub(crate) order: Vec<usize>,
    pub(crate) cursor: usize,
    pub(crate) shuffle: bool,
    pub(crate) repeat: Repeat,
}

impl Default for QueueSnapshot {
    fn default() -> Self {
        Self {
            songs: Vec::new(),
            order: Vec::new(),
            cursor: 0,
            shuffle: false,
            repeat: Repeat::Off,
        }
    }
}

impl QueueSnapshot {
    pub fn is_empty(&self) -> bool {
        self.songs.is_empty()
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            songs: Vec::new(),
            order: Vec::new(),
            cursor: 0,
            shuffle: false,
            repeat: Repeat::Off,
            rev: next_queue_rev(),
            rng: fastrand::Rng::new(),
            #[cfg(test)]
            revision_bumps: 0,
        }
    }
}

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.songs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.songs.len()
    }

    pub(crate) fn has_capacity_for(&self, additional: usize) -> bool {
        additional <= MAX.saturating_sub(self.songs.len())
    }

    /// The membership/order revision. Equal revs ⇒ identical contents and play order
    /// (process-wide — safe to compare across queue swaps); cursor moves don't change it.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    #[cfg(test)]
    pub(crate) fn revision_bumps(&self) -> usize {
        self.revision_bumps
    }

    /// Test-only: pin the shuffle RNG so two queues driven by the same command script
    /// produce identical permutations (the App↔Daemon parity harness needs shuffle to
    /// be deterministic across owners; the policy itself is what's under test).
    #[cfg(test)]
    pub(crate) fn seed_rng(&mut self, seed: u64) {
        self.rng = fastrand::Rng::with_seed(seed);
    }

    fn bump_rev(&mut self) {
        self.rev = next_queue_rev();
        #[cfg(test)]
        {
            self.revision_bumps += 1;
        }
    }

    pub fn contains_video_id(&self, video_id: &str) -> bool {
        self.songs.iter().any(|s| s.video_id == video_id)
    }

    /// Capture the current queue exactly as it would play: tracks, play order, cursor,
    /// shuffle, and repeat.
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            songs: self.songs.clone(),
            order: self.order.clone(),
            cursor: self.cursor,
            shuffle: self.shuffle,
            repeat: self.repeat,
        }
    }

    /// Restore a snapshot previously produced by [`snapshot`](Self::snapshot).
    pub fn restore_snapshot(&mut self, snapshot: QueueSnapshot) {
        let plan = self.prepare_snapshot_restore(snapshot);
        self.commit_mutation(plan);
    }

    fn restore_snapshot_without_revision(&mut self, snapshot: QueueSnapshot) {
        self.songs = snapshot.songs;
        // Enforce the same MAX cap every other mutation applies, so a corrupt/tampered session
        // snapshot can't inject an unbounded queue. Over-cap truncation drops the tail; the
        // permutation check below then rebuilds a clean play order for the trimmed songs.
        self.songs.truncate(MAX);
        self.order = snapshot.order;
        self.shuffle = snapshot.shuffle;
        self.repeat = snapshot.repeat;

        // `order` must be a true permutation of `0..songs.len()`. Checking length + upper bound
        // alone would accept a corrupt snapshot with duplicate indices (e.g. `[0, 0, 2]`), which
        // silently repeats one track in the play order and drops another; verify each index
        // appears exactly once and rebuild a clean order otherwise.
        let n = self.songs.len();
        let is_permutation = self.order.len() == n && {
            let mut seen = vec![false; n];
            self.order
                .iter()
                .all(|&i| i < n && !std::mem::replace(&mut seen[i], true))
        };
        if !is_permutation {
            self.rebuild_order(snapshot.cursor.min(n.saturating_sub(1)));
            return;
        }
        self.cursor = if self.order.is_empty() {
            0
        } else {
            snapshot.cursor.min(self.order.len() - 1)
        };
    }

    #[cfg(test)]
    pub fn video_ids(&self) -> impl Iterator<Item = &str> {
        self.songs.iter().map(|s| s.video_id.as_str())
    }

    /// The track currently selected to play, if any.
    pub fn current(&self) -> Option<&Song> {
        let idx = *self.order.get(self.cursor)?;
        self.songs.get(idx)
    }

    /// 1-based `(position, total)` of the current track, for display.
    pub fn position(&self) -> (usize, usize) {
        (self.cursor + 1, self.songs.len())
    }

    /// Every queued track in play order. Tests keep this collecting helper around
    /// while the UI uses [`ordered_iter`](Self::ordered_iter) to avoid a frame allocation.
    #[cfg(test)]
    pub fn ordered(&self) -> Vec<&Song> {
        self.ordered_iter().collect()
    }

    /// Queued tracks in play order without allocating a temporary list.
    pub fn ordered_iter(&self) -> impl Iterator<Item = &Song> {
        self.order.iter().filter_map(|&i| self.songs.get(i))
    }

    /// The current track's 0-based index within the play order, for highlighting the
    /// playing row in the queue window. Aligns with [`ordered_iter`](Self::ordered_iter).
    pub fn cursor_pos(&self) -> usize {
        self.cursor
    }

    /// Plan a next-track cursor move without changing the live queue. Track loads are admitted
    /// to the bounded player lane before [`commit_planned_cursor`](Self::commit_planned_cursor)
    /// applies the move, so a rejected load cannot leave the UI pointing at a song mpv never
    /// accepted.
    pub(crate) fn plan_next_cursor(&self, from: usize, auto: bool) -> Option<usize> {
        if self.songs.is_empty() || from >= self.order.len() {
            return None;
        }
        if auto && self.repeat == Repeat::One {
            return Some(from);
        }
        if from + 1 < self.order.len() {
            Some(from + 1)
        } else if self.repeat == Repeat::All {
            Some(0)
        } else {
            None
        }
    }

    /// Plan a previous-track cursor move without changing the live queue. At the beginning this
    /// deliberately returns the current cursor unless repeat-all is active, matching [`prev`].
    pub(crate) fn plan_prev_cursor(&self, from: usize) -> Option<usize> {
        if self.songs.is_empty() || from >= self.order.len() {
            return None;
        }
        if from > 0 {
            Some(from - 1)
        } else if self.repeat == Repeat::All {
            Some(self.order.len() - 1)
        } else {
            Some(from)
        }
    }

    pub(crate) fn song_at_cursor(&self, cursor: usize) -> Option<&Song> {
        let idx = *self.order.get(cursor)?;
        self.songs.get(idx)
    }

    /// Plan a queue-window jump without mutating the live cursor. Unlike [`goto`](Self::goto),
    /// an out-of-range request is rejected instead of clamped: remote callers validate their
    /// index up front, and a stale UI selection must not silently target a different track.
    pub(crate) fn plan_goto_cursor(&self, target: usize) -> Option<usize> {
        (target < self.order.len()).then_some(target)
    }

    /// Validate that a prepared track transition still refers to the exact queue state it read.
    /// This is deliberately separate from the cursor mutation so callers can establish the
    /// guard before recording outgoing signals or changing any other reducer state.
    pub(crate) fn validate_planned_transition(
        &self,
        expected_rev: u64,
        expected_cursor: usize,
        expected_video_id: Option<&str>,
        target_cursor: Option<usize>,
    ) {
        assert_eq!(
            self.rev, expected_rev,
            "queue changed before track-load commit"
        );
        assert_eq!(
            self.cursor, expected_cursor,
            "queue cursor changed before track-load commit"
        );
        assert_eq!(
            self.current().map(|song| song.video_id.as_str()),
            expected_video_id,
            "current track changed before track-load commit"
        );
        if let Some(target_cursor) = target_cursor {
            assert!(
                target_cursor < self.order.len(),
                "planned track cursor is out of range"
            );
        }
    }

    /// Non-panicking counterpart to [`Self::validate_planned_transition`] for an admission
    /// boundary. A deferred player intent may outlive the queue snapshot it was prepared from;
    /// the runtime must reject that work before sending its stale command batch to mpv.
    pub(crate) fn planned_transition_matches(
        &self,
        expected_rev: u64,
        expected_cursor: usize,
        expected_video_id: Option<&str>,
        target_cursor: Option<usize>,
    ) -> bool {
        self.rev == expected_rev
            && self.cursor == expected_cursor
            && self.current().map(|song| song.video_id.as_str()) == expected_video_id
            && target_cursor.is_none_or(|cursor| cursor < self.order.len())
    }

    /// Commit a cursor move prepared from this exact queue state. Queue contents cannot change
    /// between prepare and commit on the single owner lane; the guards make that invariant
    /// explicit instead of silently loading a stale target if a future caller violates it.
    pub(crate) fn commit_planned_cursor(
        &mut self,
        expected_rev: u64,
        expected_cursor: usize,
        expected_video_id: Option<&str>,
        target_cursor: usize,
    ) {
        self.validate_planned_transition(
            expected_rev,
            expected_cursor,
            expected_video_id,
            Some(target_cursor),
        );
        self.cursor = target_cursor;
    }

    /// How many tracks remain *after* the current one in the play order. Drives the
    /// autoplay/streaming hook (extend when this runs low). Zero when empty or at the end.
    pub fn remaining(&self) -> usize {
        self.order.len().saturating_sub(self.cursor + 1)
    }

    /// Append `more` tracks to the end of the queue, respecting the [`MAX`] cap. Returns
    /// the number actually added — fewer than requested (or zero) when near the cap, so
    /// the caller can report the *real* count rather than what was asked for. The new
    /// tracks are made reachable from the current cursor; with shuffle on they're
    /// randomized among themselves so they don't clump in insertion order.
    pub fn extend(&mut self, more: Vec<Song>) -> usize {
        let added = self.extend_without_revision(more);
        if added > 0 {
            self.bump_rev();
        }
        added
    }

    fn extend_without_revision(&mut self, more: Vec<Song>) -> usize {
        let free = MAX.saturating_sub(self.songs.len());
        if free == 0 {
            return 0;
        }
        let mut new_indices = Vec::new();
        for song in more.into_iter().take(free) {
            new_indices.push(self.songs.len());
            self.songs.push(song);
        }
        if new_indices.is_empty() {
            return 0;
        }
        let added = new_indices.len();
        if self.shuffle {
            self.rng.shuffle(&mut new_indices);
        }
        // If the queue had been empty, the cursor already sits at 0 → the first appended
        // track becomes current, which is what an enqueue-into-empty should do.
        self.order.extend(new_indices);
        added
    }

    /// Insert `more` immediately after the current track without moving the cursor. This powers
    /// the optional "enqueue as next" mode: playback keeps going, but the first inserted song is
    /// what `next` will reach. Into an empty queue it behaves like [`extend`](Self::extend).
    /// Shuffle-agnostic: the inserted block stays directly after the current track so the
    /// "next" promise holds even while shuffle is enabled.
    pub fn insert_next_many(&mut self, more: Vec<Song>) -> usize {
        let free = MAX.saturating_sub(self.songs.len());
        if free == 0 {
            return 0;
        }
        let old_len = self.songs.len();
        let more: Vec<Song> = more.into_iter().take(free).collect();
        if more.is_empty() {
            return 0;
        }
        let added = more.len();
        self.songs.extend(more);
        let new_indices = old_len..old_len + added;
        if self.order.is_empty() {
            self.order.extend(new_indices);
            self.cursor = 0;
        } else {
            let at = self.cursor + 1;
            for (offset, idx) in new_indices.enumerate() {
                self.order.insert(at + offset, idx);
            }
        }
        self.bump_rev();
        added
    }

    /// Insert `song` immediately after the current track in the play order and make it the
    /// new current — "play this now" without disturbing the rest of the queue, which resumes
    /// after this track ends. Into an empty queue it simply becomes the sole track. Returns
    /// `false` (nothing inserted) when the queue is already at the [`MAX`] cap, so the caller
    /// can report it; `true` otherwise. Shuffle-agnostic: it always lands right after the
    /// cursor in play order, so the "now playing next" promise holds either way.
    pub fn play_now(&mut self, song: Song) -> bool {
        self.play_now_many(vec![song]) == 1
    }

    /// Insert `more` immediately after the current track and make the first inserted track
    /// current. Returns the number actually inserted, bounded by [`MAX`].
    pub fn play_now_many(&mut self, more: Vec<Song>) -> usize {
        let added = self.play_now_many_without_revision(more);
        if added > 0 {
            self.bump_rev();
        }
        added
    }

    fn play_now_many_without_revision(&mut self, more: Vec<Song>) -> usize {
        let free = MAX.saturating_sub(self.songs.len());
        if free == 0 {
            return 0;
        }
        let old_len = self.songs.len();
        let more: Vec<Song> = more.into_iter().take(free).collect();
        if more.is_empty() {
            return 0;
        }
        let added = more.len();
        self.songs.extend(more);
        let new_indices = old_len..old_len + added;
        if self.order.is_empty() {
            self.order.extend(new_indices);
            self.cursor = 0;
        } else {
            let at = self.cursor + 1;
            for (offset, idx) in new_indices.enumerate() {
                self.order.insert(at + offset, idx);
            }
            self.cursor = at;
        }
        added
    }

    /// Replace the queue with `songs` and make `start` the current track. Honors the
    /// current shuffle setting (the chosen track plays first, the rest follow randomly).
    pub fn set(&mut self, songs: Vec<Song>, start: usize) {
        let plan = self.prepare_replacement(QueueReplacementDraft::new(songs, start, None));
        self.commit_mutation(plan);
    }

    /// Advance to the next track, returning it. `auto` is true for end-of-track
    /// auto-advance (where repeat-one replays the current track) and false for a manual
    /// "next" (which always moves on). Returns `None` when the queue has ended.
    pub fn next(&mut self, auto: bool) -> Option<&Song> {
        if self.songs.is_empty() {
            return None;
        }
        if auto && self.repeat == Repeat::One {
            return self.current();
        }
        if self.cursor + 1 < self.order.len() {
            self.cursor += 1;
        } else if self.repeat == Repeat::All {
            self.cursor = 0;
        } else {
            return None;
        }
        self.current()
    }

    /// Up to `n` upcoming tracks (those after the current one in play order) — feeds the
    /// DJ Gem assistant's `get_queue` context snapshot.
    pub fn upcoming(&self, n: usize) -> Vec<&Song> {
        self.order
            .iter()
            .skip(self.cursor + 1)
            .take(n)
            .filter_map(|&i| self.songs.get(i))
            .collect()
    }

    /// The track a manual "next" would advance to, *without* moving the cursor — used to
    /// prefetch the upcoming stream. Wraps under repeat-all; `None` at the end otherwise.
    /// (Repeat-one's auto-replay of the current track needs no prefetch, so it's ignored
    /// here — this returns the genuinely *next* track.)
    pub fn peek_next(&self) -> Option<&Song> {
        if self.songs.is_empty() {
            return None;
        }
        let next = if self.cursor + 1 < self.order.len() {
            self.cursor + 1
        } else if self.repeat == Repeat::All {
            0
        } else {
            return None;
        };
        let idx = *self.order.get(next)?;
        self.songs.get(idx)
    }

    /// Step back to the previous track, returning it. At the start, wraps to the end
    /// only when repeat-all is on; otherwise stays put.
    pub fn prev(&mut self) -> Option<&Song> {
        if self.songs.is_empty() {
            return None;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
        } else if self.repeat == Repeat::All {
            self.cursor = self.order.len() - 1;
        }
        self.current()
    }

    /// Jump the cursor to an order position (as listed by [`ordered`](Self::ordered)),
    /// returning the track now current. Out-of-range positions clamp to the last track;
    /// an empty queue is a no-op returning `None`. Used by the queue window's "play this".
    pub fn goto(&mut self, pos: usize) -> Option<&Song> {
        if self.order.is_empty() {
            return None;
        }
        self.cursor = pos.min(self.order.len() - 1);
        self.current()
    }

    /// Remove the track at order position `pos` (as listed by
    /// [`ordered_iter`](Self::ordered_iter)), keeping `songs`, `order`, and `cursor` consistent:
    /// the song is dropped, every later
    /// `songs` index referenced by `order` is shifted down to match, and the cursor is moved
    /// so the same track stays current when possible. Returns `Some(current_changed)` —
    /// `true` when the removed track was the one playing, so the caller loads the new
    /// current track (or stops if the queue is now empty) — or `None` if `pos` is out of
    /// range. Powers the queue window's delete (single row or a drag-selected range).
    pub fn remove_at(&mut self, pos: usize) -> Option<bool> {
        let was_current = self.remove_at_without_revision(pos)?;
        self.bump_rev();
        Some(was_current)
    }

    fn remove_at_without_revision(&mut self, pos: usize) -> Option<bool> {
        let song_idx = *self.order.get(pos)?;
        let was_current = pos == self.cursor;
        self.songs.remove(song_idx);
        self.order.remove(pos);
        // `songs` indices above the removed one all shifted down by one; fix the references.
        for idx in &mut self.order {
            if *idx > song_idx {
                *idx -= 1;
            }
        }
        // Keep the cursor on the same track when removing before it; clamp when removing at
        // or after it leaves the cursor past the (now shorter) end.
        if pos < self.cursor {
            self.cursor -= 1;
        }
        if self.cursor >= self.order.len() {
            self.cursor = self.order.len().saturating_sub(1);
        }
        Some(was_current)
    }

    fn remove_range_without_revision(&mut self, lo: usize, hi: usize) {
        debug_assert!(lo <= hi);
        debug_assert!(hi < self.order.len());
        for pos in (lo..=hi).rev() {
            let removed = self.remove_at_without_revision(pos);
            debug_assert!(
                removed.is_some(),
                "prepared queue range must stay in bounds"
            );
        }
    }

    /// Move the track at order position `from` to order position `to` (queue
    /// drag-reorder), keeping the same track current. Pure order change: membership,
    /// the current track, and playback position are untouched — `position_epoch` never
    /// bumps here. Returns `None` when either position is out of range; a no-op move
    /// (`from == to`) succeeds without a revision bump. Shared by both owners
    /// (App + daemon) so the parity harness compares one implementation.
    pub fn move_item(&mut self, from: usize, to: usize) -> Option<()> {
        if from >= self.order.len() || to >= self.order.len() {
            return None;
        }
        if from == to {
            return Some(());
        }
        let song_idx = self.order.remove(from);
        self.order.insert(to, song_idx);
        if self.cursor == from {
            self.cursor = to;
        } else if from < self.cursor && to >= self.cursor {
            self.cursor -= 1;
        } else if from > self.cursor && to <= self.cursor {
            self.cursor += 1;
        }
        self.bump_rev();
        Some(())
    }

    /// Drop every track after the current one (order positions `cursor+1..`), keeping
    /// the current track playing. Returns how many tracks were removed; zero removals
    /// do not bump the revision.
    pub fn clear_upcoming(&mut self) -> usize {
        let len = self.order.len();
        if len <= self.cursor + 1 {
            return 0;
        }
        let removed = len - self.cursor - 1;
        self.remove_range_without_revision(self.cursor + 1, len - 1);
        self.bump_rev();
        removed
    }

    /// Toggle shuffle, keeping the current track current.
    pub fn toggle_shuffle(&mut self) {
        self.set_shuffle(!self.shuffle);
    }

    /// Set shuffle explicitly, keeping the current track current.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        if self.set_shuffle_without_revision(shuffle) {
            self.bump_rev();
        }
    }

    /// Cycle the repeat mode.
    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.cycled();
    }

    /// Rebuild `order` so that song index `keep` becomes the current track. With shuffle
    /// on, `keep` is moved to the front and the rest are randomized (cursor = 0); off, the
    /// order is natural and the cursor sits on `keep`.
    fn rebuild_order(&mut self, keep: usize) {
        let n = self.songs.len();
        self.order = (0..n).collect();
        if n == 0 {
            self.cursor = 0;
            return;
        }
        if self.shuffle {
            self.rng.shuffle(&mut self.order);
            if let Some(pos) = self.order.iter().position(|&x| x == keep) {
                self.order.swap(0, pos);
            }
            self.cursor = 0;
        } else {
            self.cursor = keep.min(n - 1);
        }
    }

    fn replace_without_revision(&mut self, mut songs: Vec<Song>, start: usize) {
        songs.truncate(MAX);
        let start = start.min(songs.len().saturating_sub(1));
        self.songs = songs;
        self.rebuild_order(start);
    }

    fn set_shuffle_without_revision(&mut self, shuffle: bool) -> bool {
        if self.shuffle == shuffle {
            return false;
        }
        self.shuffle = shuffle;
        if let Some(&current_idx) = self.order.get(self.cursor) {
            self.rebuild_order(current_idx);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("title-{id}"), "a", "0:10")
    }

    fn songs(n: usize) -> Vec<Song> {
        (0..n).map(|i| song(&i.to_string())).collect()
    }

    fn id(q: &Queue) -> &str {
        q.current().unwrap().video_id.as_str()
    }

    fn rng_probe(q: &Queue) -> u64 {
        q.rng.clone().u64(..)
    }

    fn fingerprint(q: &Queue) -> (Vec<String>, Vec<usize>, usize, bool, Repeat, u64, u64) {
        (
            q.video_ids().map(str::to_owned).collect(),
            q.order.clone(),
            q.cursor,
            q.shuffle,
            q.repeat,
            q.rev,
            rng_probe(q),
        )
    }

    fn assert_queue_state_eq(actual: &Queue, expected: &Queue) {
        assert_eq!(
            actual.video_ids().collect::<Vec<_>>(),
            expected.video_ids().collect::<Vec<_>>()
        );
        assert_eq!(actual.order, expected.order);
        assert_eq!(actual.cursor, expected.cursor);
        assert_eq!(actual.shuffle, expected.shuffle);
        assert_eq!(actual.repeat, expected.repeat);
        assert_eq!(rng_probe(actual), rng_probe(expected));
    }

    #[test]
    fn repeat_is_on_is_true_for_every_mode_but_off() {
        assert!(!Repeat::Off.is_on());
        assert!(Repeat::All.is_on());
        assert!(Repeat::One.is_on());
    }

    #[test]
    fn empty_queue_navigation_is_noop() {
        let mut q = Queue::default();
        assert!(q.current().is_none());
        assert!(q.next(true).is_none());
        assert!(q.next(false).is_none());
        assert!(q.prev().is_none());
    }

    #[test]
    fn set_makes_start_current_and_advances_in_order() {
        let mut q = Queue::default();
        q.set(songs(5), 2);
        assert_eq!(id(&q), "2");
        assert_eq!(q.position(), (3, 5));
        assert_eq!(q.next(true).unwrap().video_id, "3");
        assert_eq!(q.next(true).unwrap().video_id, "4");
    }

    #[test]
    fn replacement_preparation_does_not_mutate_live_state_revision_or_rng() {
        let mut q = Queue::default();
        q.set(songs(4), 2);
        q.repeat = Repeat::All;
        q.seed_rng(0x5eed);
        let before = fingerprint(&q);

        let plan = q.prepare_replacement(QueueReplacementDraft::new(songs(7), 3, Some(true)));

        assert_eq!(fingerprint(&q), before);
        assert_eq!(plan.len(), 7);
        assert_eq!(plan.cursor_pos(), 0, "shuffle makes selected track first");
        assert_eq!(plan.current().unwrap().video_id, "3");
        assert_eq!(plan.ordered_iter().next().unwrap().video_id, "3");
        assert_eq!(plan.repeat(), Repeat::All);
    }

    #[test]
    fn replacement_commit_matches_eager_set_and_optional_shuffle_override() {
        for (initial_shuffle, shuffle_override) in [
            (false, None),
            (false, Some(false)),
            (false, Some(true)),
            (true, None),
            (true, Some(false)),
            (true, Some(true)),
        ] {
            let make_base = || {
                let mut q = Queue::default();
                q.set(songs(4), 1);
                q.seed_rng(41);
                q.set_shuffle(initial_shuffle);
                // Compare replacement RNG consumption from an identical known state.
                q.seed_rng(0xdecafbad);
                q.repeat = Repeat::One;
                q
            };
            let mut eager = make_base();
            let mut planned = make_base();
            let replacement = songs(8);
            let start = 5;

            eager.set(replacement.clone(), start);
            if let Some(shuffle) = shuffle_override {
                eager.set_shuffle(shuffle);
            }

            let plan = planned.prepare_replacement(QueueReplacementDraft::new(
                replacement,
                start,
                shuffle_override,
            ));
            assert_eq!(plan.current().unwrap().video_id, "5");
            if plan.shuffle() {
                assert_eq!(plan.cursor_pos(), 0);
                assert_eq!(plan.ordered_iter().next().unwrap().video_id, "5");
            } else {
                assert_eq!(plan.cursor_pos(), start);
            }
            let rev_before_commit = planned.rev();
            planned.commit_mutation(plan);

            assert_ne!(planned.rev(), rev_before_commit);
            assert_queue_state_eq(&planned, &eager);
        }
    }

    #[test]
    fn stale_mutation_guard_rejects_before_mutating_queue_or_rng() {
        // Revision mismatch.
        let mut revision_stale = Queue::default();
        revision_stale.set(songs(3), 0);
        revision_stale.seed_rng(17);
        let plan =
            revision_stale.prepare_replacement(QueueReplacementDraft::new(songs(5), 2, Some(true)));
        revision_stale.extend(vec![song("late")]);
        let before_rejection = fingerprint(&revision_stale);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            revision_stale.commit_mutation(plan);
        }));
        assert!(result.is_err());
        assert_eq!(fingerprint(&revision_stale), before_rejection);

        // Cursor mismatch with an unchanged membership/order revision.
        let mut cursor_stale = Queue::default();
        cursor_stale.set(songs(3), 0);
        let plan = cursor_stale.prepare_replacement(QueueReplacementDraft::new(songs(5), 2, None));
        cursor_stale.goto(1);
        let before_rejection = fingerprint(&cursor_stale);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cursor_stale.commit_mutation(plan);
        }));
        assert!(result.is_err());
        assert_eq!(fingerprint(&cursor_stale), before_rejection);

        // Current identity mismatch even if a future bug changes order without its revision.
        let mut current_stale = Queue::default();
        current_stale.set(songs(3), 0);
        let plan = current_stale.prepare_replacement(QueueReplacementDraft::new(songs(5), 2, None));
        current_stale.order.swap(0, 1);
        let before_rejection = fingerprint(&current_stale);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            current_stale.commit_mutation(plan);
        }));
        assert!(result.is_err());
        assert_eq!(fingerprint(&current_stale), before_rejection);
    }

    #[test]
    fn replacement_plan_caps_clamps_and_handles_empty_input() {
        let mut q = Queue::default();
        let plan = q.prepare_replacement(QueueReplacementDraft::new(
            songs(MAX + 50),
            usize::MAX,
            None,
        ));
        assert_eq!(plan.len(), MAX);
        assert_eq!(plan.cursor_pos(), MAX - 1);
        assert_eq!(plan.current().unwrap().video_id, (MAX - 1).to_string());
        assert_eq!(plan.ordered_iter().count(), MAX);
        q.commit_mutation(plan);
        assert_eq!(q.len(), MAX);
        assert_eq!(q.cursor_pos(), MAX - 1);
        assert_eq!(id(&q), (MAX - 1).to_string());

        let plan = q.prepare_replacement(QueueReplacementDraft::new(
            Vec::new(),
            usize::MAX,
            Some(true),
        ));
        assert!(plan.is_empty());
        assert_eq!(plan.cursor_pos(), 0);
        assert!(plan.current().is_none());
        assert_eq!(plan.ordered_iter().count(), 0);
        q.commit_mutation(plan);
        assert!(q.is_empty());
        assert_eq!(q.cursor_pos(), 0);
        assert!(q.current().is_none());
        assert!(q.shuffle);
    }

    #[test]
    fn play_now_preparation_is_pure_matches_eager_and_commits_one_revision() {
        let make_base = || {
            let mut q = Queue::default();
            q.set(songs(MAX - 1), 17);
            q.seed_rng(0x51de);
            q.set_shuffle(true);
            q.repeat = Repeat::All;
            q.seed_rng(0x51de_0001);
            q
        };
        let mut planned = make_base();
        let mut eager = make_base();
        let before = fingerprint(&planned);
        let revision_bumps = planned.revision_bumps;
        let requested = vec![song("new-0"), song("new-1"), song("new-2")];

        let (plan, outcome) = planned.prepare_play_now_many(requested.clone());

        assert_eq!(fingerprint(&planned), before);
        assert_eq!(planned.revision_bumps, revision_bumps);
        assert_eq!(outcome.requested(), 3);
        assert_eq!(outcome.added(), 1, "the queue had only one free slot");
        assert_eq!(outcome.selected_cursor(), Some(plan.cursor_pos()));
        assert_eq!(plan.current().unwrap().video_id, "new-0");

        assert_eq!(eager.play_now_many(requested), 1);
        planned.commit_mutation(plan);

        assert_queue_state_eq(&planned, &eager);
        assert_eq!(planned.revision_bumps, revision_bumps + 1);
        assert_eq!(rng_probe(&planned), rng_probe(&eager));
    }

    #[test]
    fn idle_enqueue_preparation_preserves_live_rng_and_matches_shuffled_eager_sequence() {
        let make_base = || {
            let mut q = Queue::default();
            q.set(songs(5), 2);
            q.seed_rng(0x1d1e);
            q.set_shuffle(true);
            q.repeat = Repeat::One;
            q.seed_rng(0x1d1e_0001);
            q
        };
        let mut planned = make_base();
        let mut eager = make_base();
        let before = fingerprint(&planned);
        let revision_bumps = planned.revision_bumps;
        let appended = vec![song("new-0"), song("new-1"), song("new-2"), song("new-3")];

        let (plan, outcome) = planned.prepare_idle_enqueue(appended.clone());

        assert_eq!(fingerprint(&planned), before);
        assert_eq!(planned.revision_bumps, revision_bumps);
        assert_eq!(outcome.requested(), appended.len());
        assert_eq!(outcome.added(), appended.len());
        assert_eq!(outcome.selected_cursor(), Some(5));
        assert_eq!(plan.cursor_pos(), 5);

        assert_eq!(eager.extend(appended), 4);
        eager.goto(5);
        planned.commit_mutation(plan);

        assert_queue_state_eq(&planned, &eager);
        assert_eq!(planned.revision_bumps, revision_bumps + 1);
        assert_eq!(
            planned.current().unwrap().video_id,
            eager.current().unwrap().video_id
        );
    }

    #[test]
    fn repeat_off_stops_at_end() {
        let mut q = Queue::default();
        q.set(songs(2), 0);
        assert_eq!(id(&q), "0");
        assert_eq!(q.next(true).unwrap().video_id, "1");
        assert!(q.next(true).is_none()); // end, repeat off -> stop
    }

    #[test]
    fn repeat_all_wraps_around() {
        let mut q = Queue::default();
        q.set(songs(2), 0);
        q.repeat = Repeat::All;
        assert_eq!(q.next(true).unwrap().video_id, "1");
        assert_eq!(q.next(true).unwrap().video_id, "0"); // wrapped
    }

    #[test]
    fn repeat_one_replays_on_auto_but_not_on_manual() {
        let mut q = Queue::default();
        q.set(songs(3), 0);
        q.repeat = Repeat::One;
        assert_eq!(q.next(true).unwrap().video_id, "0"); // auto -> replay
        assert_eq!(q.next(false).unwrap().video_id, "1"); // manual -> advance
    }

    #[test]
    fn prev_wraps_only_with_repeat_all() {
        let mut q = Queue::default();
        q.set(songs(3), 0);
        assert_eq!(id(&q), "0");
        assert_eq!(q.prev().unwrap().video_id, "0"); // off: stays
        q.repeat = Repeat::All;
        assert_eq!(q.prev().unwrap().video_id, "2"); // all: wraps to end
    }

    #[test]
    fn single_track_repeat_all_replays() {
        let mut q = Queue::default();
        q.set(songs(1), 0);
        q.repeat = Repeat::All;
        assert_eq!(q.next(true).unwrap().video_id, "0");
    }

    #[test]
    fn shuffle_keeps_current_and_is_a_permutation() {
        let mut q = Queue::default();
        q.set(songs(10), 4);
        q.rng = fastrand::Rng::with_seed(12345);
        q.toggle_shuffle();
        // Current track is preserved across the shuffle.
        assert_eq!(id(&q), "4");
        // `order` is a valid permutation of 0..10.
        let mut seen = q.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
        // Toggling back restores natural order with the cursor on the same track.
        q.toggle_shuffle();
        assert_eq!(id(&q), "4");
        assert_eq!(q.position(), (5, 10));
    }

    #[test]
    fn peek_next_does_not_move_cursor() {
        let mut q = Queue::default();
        q.set(songs(3), 0);
        assert_eq!(q.peek_next().unwrap().video_id, "1");
        assert_eq!(id(&q), "0"); // cursor unchanged
        // At the end, repeat-off yields nothing; repeat-all wraps.
        q.set(songs(3), 2);
        assert!(q.peek_next().is_none());
        q.repeat = Repeat::All;
        assert_eq!(q.peek_next().unwrap().video_id, "0");
    }

    #[test]
    fn set_truncates_to_cap() {
        let mut q = Queue::default();
        q.set(songs(MAX + 50), 0);
        assert_eq!(q.len(), MAX);
    }

    #[test]
    fn restore_snapshot_enforces_the_cap() {
        // A corrupt/tampered session snapshot with more than MAX songs must be trimmed on
        // restore, matching the cap every other mutation applies (set/extend/insert) — so a
        // hostile session cache can't inject an unbounded queue.
        let over = MAX + 50;
        let snapshot = QueueSnapshot {
            songs: songs(over),
            order: (0..over).collect(),
            cursor: over - 1,
            shuffle: false,
            repeat: Repeat::Off,
        };
        let mut q = Queue::default();
        q.restore_snapshot(snapshot);
        assert_eq!(q.len(), MAX, "restore must enforce the MAX cap");
        // A clean play order was rebuilt for the trimmed songs; the cursor stays in-bounds.
        assert!(q.current().is_some());
    }

    #[test]
    fn remaining_counts_tracks_after_cursor() {
        let mut q = Queue::default();
        assert_eq!(q.remaining(), 0); // empty
        q.set(songs(5), 0);
        assert_eq!(q.remaining(), 4);
        q.next(false);
        assert_eq!(q.remaining(), 3);
        q.set(songs(5), 4); // cursor at the end
        assert_eq!(q.remaining(), 0);
    }

    #[test]
    fn extend_appends_and_reports_actual_count() {
        let mut q = Queue::default();
        q.set(songs(3), 0);
        let added = q.extend(songs(2));
        assert_eq!(added, 2);
        assert_eq!(q.len(), 5);
        // The appended tracks are reachable in play order after the originals.
        assert_eq!(q.remaining(), 4);
    }

    #[test]
    fn extend_respects_the_cap_and_reports_real_count() {
        let mut q = Queue::default();
        q.set(songs(MAX - 2), 0);
        let added = q.extend(songs(10)); // only 2 slots free
        assert_eq!(added, 2);
        assert_eq!(q.len(), MAX);
        // Full queue: further extend adds nothing.
        assert_eq!(q.extend(songs(5)), 0);
        assert_eq!(q.len(), MAX);
    }

    #[test]
    fn extend_into_empty_makes_first_track_current() {
        let mut q = Queue::default();
        assert_eq!(q.extend(songs(3)), 3);
        assert_eq!(id(&q), "0");
        assert_eq!(q.position(), (1, 3));
    }

    #[test]
    fn upcoming_lists_tracks_after_cursor() {
        let mut q = Queue::default();
        assert!(q.upcoming(5).is_empty()); // empty
        q.set(songs(5), 1); // current = id1
        let up: Vec<&str> = q.upcoming(2).iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(up, vec!["2", "3"]);
        assert_eq!(q.upcoming(99).len(), 3); // capped by what's left, not n
    }

    #[test]
    fn ordered_lists_the_play_sequence() {
        let mut q = Queue::default();
        assert!(q.ordered().is_empty());
        q.set(songs(3), 0);
        let ids: Vec<&str> = q.ordered().iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["0", "1", "2"]);
    }

    #[test]
    fn goto_jumps_cursor_and_clamps() {
        let mut q = Queue::default();
        assert!(q.goto(0).is_none()); // empty -> no-op
        q.set(songs(5), 0);
        assert_eq!(q.goto(3).unwrap().video_id, "3");
        assert_eq!(q.position(), (4, 5));
        assert_eq!(q.cursor_pos(), 3);
        // Out of range clamps to the last track.
        assert_eq!(q.goto(99).unwrap().video_id, "4");
        assert_eq!(q.cursor_pos(), 4);
    }

    #[test]
    fn remove_before_cursor_keeps_current() {
        let mut q = Queue::default();
        q.set(songs(5), 2); // current = "2"
        assert_eq!(q.remove_at(0), Some(false)); // removed "0", not the current track
        assert_eq!(id(&q), "2");
        assert_eq!(q.position(), (2, 4)); // now 2nd of 4
    }

    #[test]
    fn move_item_reorders_and_keeps_the_same_track_current() {
        let mut q = Queue::default();
        q.set(songs(5), 2); // current = "2"
        let rev = q.rev();

        // Move a track from after the cursor to before it: cursor shifts right.
        assert_eq!(q.move_item(4, 0), Some(()));
        assert_eq!(id(&q), "2");
        assert_eq!(q.position(), (4, 5)); // order: 4,0,1,2,3 — "2" now 4th
        assert_ne!(q.rev(), rev, "order change bumps the revision");

        // Move the current track itself.
        let rev = q.rev();
        assert_eq!(q.move_item(3, 0), Some(()));
        assert_eq!(id(&q), "2");
        assert_eq!(q.position(), (1, 5));
        assert_ne!(q.rev(), rev);

        // Move from before the cursor to after it: cursor shifts left.
        let rev = q.rev();
        assert_eq!(q.move_item(1, 4), Some(()));
        assert_eq!(id(&q), "2");
        assert_eq!(
            q.position(),
            (1, 5),
            "removing an earlier row and reinserting after keeps '2' current"
        );
        assert_ne!(q.rev(), rev);

        // No-op and out-of-range moves.
        let rev = q.rev();
        assert_eq!(q.move_item(2, 2), Some(()));
        assert_eq!(q.rev(), rev, "no-op move must not bump the revision");
        assert_eq!(q.move_item(9, 0), None);
        assert_eq!(q.move_item(0, 9), None);
        assert_eq!(q.rev(), rev);
    }

    #[test]
    fn clear_upcoming_drops_everything_after_the_current_track() {
        let mut q = Queue::default();
        q.set(songs(5), 2); // current = "2"
        let rev = q.rev();
        assert_eq!(q.clear_upcoming(), 2); // "3", "4"
        assert_eq!(id(&q), "2");
        assert_eq!(q.position(), (3, 3));
        assert_ne!(q.rev(), rev);

        // Nothing upcoming: no removals, no revision bump.
        let rev = q.rev();
        assert_eq!(q.clear_upcoming(), 0);
        assert_eq!(q.rev(), rev);

        let mut empty = Queue::default();
        assert_eq!(empty.clear_upcoming(), 0);
    }

    #[test]
    fn remove_current_makes_the_next_track_current() {
        let mut q = Queue::default();
        q.set(songs(5), 2); // current = "2"
        assert_eq!(q.remove_at(2), Some(true)); // removed the current track
        assert_eq!(id(&q), "3"); // the track that shifted into its slot
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn remove_after_cursor_keeps_current() {
        let mut q = Queue::default();
        q.set(songs(5), 2);
        assert_eq!(q.remove_at(4), Some(false));
        assert_eq!(id(&q), "2");
        assert_eq!(q.len(), 4);
    }

    #[test]
    fn remove_last_remaining_track_empties_the_queue() {
        let mut q = Queue::default();
        q.set(songs(1), 0);
        assert_eq!(q.remove_at(0), Some(true));
        assert!(q.is_empty());
        assert!(q.current().is_none());
    }

    #[test]
    fn remove_out_of_range_is_none() {
        let mut q = Queue::default();
        q.set(songs(2), 0);
        assert_eq!(q.remove_at(5), None);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn remove_under_shuffle_stays_a_permutation() {
        let mut q = Queue::default();
        q.set(songs(6), 0);
        q.rng = fastrand::Rng::with_seed(7);
        q.toggle_shuffle();
        q.remove_at(3);
        assert_eq!(q.len(), 5);
        let mut seen = q.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..5).collect::<Vec<_>>());
        // Every order entry still indexes a real song.
        assert!(q.order.iter().all(|&i| i < q.songs.len()));
    }

    #[test]
    fn extend_under_shuffle_stays_a_permutation() {
        let mut q = Queue::default();
        q.set(songs(4), 0);
        q.rng = fastrand::Rng::with_seed(42);
        q.toggle_shuffle();
        q.extend(songs(3));
        assert_eq!(q.len(), 7);
        let mut seen = q.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..7).collect::<Vec<_>>());
    }

    #[test]
    fn insert_next_many_places_tracks_after_current_without_jumping() {
        let mut q = Queue::default();
        q.set(songs(4), 1); // queue 0,1,2,3 — playing "1"
        let added = q.insert_next_many(vec![song("x"), song("y")]);
        assert_eq!(added, 2);
        assert_eq!(id(&q), "1");
        let ids: Vec<&str> = q.ordered().iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["0", "1", "x", "y", "2", "3"]);
        assert_eq!(q.next(false).unwrap().video_id, "x");
    }

    #[test]
    fn insert_next_many_into_empty_makes_first_track_current() {
        let mut q = Queue::default();
        let added = q.insert_next_many(vec![song("solo")]);
        assert_eq!(added, 1);
        assert_eq!(id(&q), "solo");
        assert_eq!(q.position(), (1, 1));
    }

    #[test]
    fn insert_next_many_under_shuffle_stays_a_permutation_and_next() {
        let mut q = Queue::default();
        q.set(songs(5), 2);
        q.rng = fastrand::Rng::with_seed(99);
        q.toggle_shuffle();
        let current = id(&q).to_owned();
        let at = q.cursor_pos();
        q.insert_next_many(vec![song("z")]);
        assert_eq!(id(&q), current);
        assert_eq!(q.ordered()[at + 1].video_id, "z");
        let mut seen = q.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..6).collect::<Vec<_>>());
        assert!(q.order.iter().all(|&i| i < q.songs.len()));
    }

    #[test]
    fn play_now_into_empty_queue_makes_the_track_current() {
        let mut q = Queue::default();
        assert!(q.play_now(song("solo")));
        assert_eq!(id(&q), "solo");
        assert_eq!(q.position(), (1, 1));
    }

    #[test]
    fn play_now_inserts_after_current_and_jumps_to_it() {
        let mut q = Queue::default();
        q.set(songs(4), 1); // queue 0,1,2,3 — playing "1"
        assert!(q.play_now(song("new")));
        // The inserted track is current…
        assert_eq!(id(&q), "new");
        assert_eq!(q.len(), 5);
        // …and the queue resumes with what *was* after the old current ("2", "3").
        assert_eq!(q.next(false).unwrap().video_id, "2");
        assert_eq!(q.next(false).unwrap().video_id, "3");
    }

    #[test]
    fn play_now_preserves_the_existing_queue() {
        let mut q = Queue::default();
        q.set(songs(3), 0);
        q.play_now(song("x"));
        // Every original track is still present; only one was added.
        assert_eq!(q.len(), 4);
        for orig in ["0", "1", "2"] {
            assert!(q.video_ids().any(|v| v == orig), "kept {orig}");
        }
    }

    #[test]
    fn play_now_respects_the_cap() {
        let mut q = Queue::default();
        q.set(songs(MAX), 0);
        assert!(!q.play_now(song("overflow"))); // full → rejected
        assert_eq!(q.len(), MAX);
    }

    #[test]
    fn play_now_under_shuffle_stays_a_permutation_and_is_current() {
        let mut q = Queue::default();
        q.set(songs(5), 2);
        q.rng = fastrand::Rng::with_seed(99);
        q.toggle_shuffle();
        assert!(q.play_now(song("z")));
        assert_eq!(id(&q), "z");
        let mut seen = q.order.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..6).collect::<Vec<_>>());
        assert!(q.order.iter().all(|&i| i < q.songs.len()));
    }

    #[test]
    fn rev_bumps_on_membership_and_order_changes_only() {
        let mut q = Queue::default();
        let mut last = q.rev();

        let mut expect_bump = |q: &Queue, what: &str| {
            assert!(q.rev() > last, "{what} must bump rev");
            last = q.rev();
        };

        q.set(songs(5), 0);
        expect_bump(&q, "set");
        q.extend(songs(2));
        expect_bump(&q, "extend");
        q.insert_next_many(vec![song("n")]);
        expect_bump(&q, "insert_next_many");
        q.play_now(song("p"));
        expect_bump(&q, "play_now");
        q.remove_at(0);
        expect_bump(&q, "remove_at");
        q.toggle_shuffle();
        expect_bump(&q, "toggle_shuffle");

        // Cursor moves and mode flags are NOT membership/order changes: the cursor rides
        // PlayerModel.queue_pos on the wire, so these must be invisible to rev.
        let frozen = q.rev();
        q.next(false);
        q.next(true);
        q.prev();
        q.goto(2);
        q.cycle_repeat();
        q.set_shuffle(q.shuffle); // no-op set
        assert_eq!(q.rev(), frozen, "cursor/mode changes must not bump rev");

        // No-op mutations don't bump either.
        let mut full = Queue::default();
        full.set(songs(MAX), 0);
        let at_cap = full.rev();
        assert_eq!(full.extend(songs(3)), 0);
        assert_eq!(full.rev(), at_cap, "capped extend added nothing");
        assert_eq!(full.remove_at(MAX + 5), None);
        assert_eq!(full.rev(), at_cap, "out-of-range remove changed nothing");
    }

    #[test]
    fn rev_is_owner_global_so_queue_swaps_never_collide() {
        // The radio-mode scenario (docs/gui/02 §14): stash queue A, live on queue B,
        // mutate both the same number of times, swap back. A per-queue counter would
        // repeat an already-seen rev; the process-global source cannot.
        let mut seen = std::collections::HashSet::new();

        let mut q = Queue::default();
        q.set(songs(3), 0);
        assert!(seen.insert(q.rev()), "fresh rev per mutation");
        let stash_a = q.snapshot();

        // Swap to queue B and mutate it.
        q.restore_snapshot(QueueSnapshot::default());
        assert!(seen.insert(q.rev()), "swap to B is observable");
        q.set(songs(2), 0);
        assert!(seen.insert(q.rev()));

        // Swap back to A: contents equal the stash, but the rev must be brand new —
        // an observer comparing revs sees the swap even though A itself never changed.
        q.restore_snapshot(stash_a);
        assert!(seen.insert(q.rev()), "swap back to A must mint a fresh rev");

        // And two live queues never share a rev.
        let other = Queue::default();
        assert!(seen.insert(other.rev()), "revs are process-global");
    }

    #[test]
    fn restore_snapshot_rebuilds_a_corrupt_non_permutation_order() {
        // `order` has the right length and in-range indices but is NOT a permutation
        // (`[0, 0, 2]` — 0 duplicated, 1 missing). Length + upper-bound alone would accept it,
        // silently repeating one track and dropping another; restore must rebuild a clean order.
        let mut q = Queue::default();
        q.restore_snapshot(QueueSnapshot {
            songs: songs(3),
            order: vec![0, 0, 2],
            cursor: 0,
            shuffle: false,
            repeat: Repeat::Off,
        });
        let mut order = q.order.clone();
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2], "rebuilt to a valid permutation");

        // A wrong-length order also rebuilds (the pre-existing guard, still intact).
        let mut q2 = Queue::default();
        q2.restore_snapshot(QueueSnapshot {
            songs: songs(3),
            order: vec![2, 1],
            cursor: 0,
            shuffle: false,
            repeat: Repeat::Off,
        });
        let mut order2 = q2.order.clone();
        order2.sort_unstable();
        assert_eq!(order2, vec![0, 1, 2]);
    }
}
