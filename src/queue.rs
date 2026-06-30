//! The play queue: ordering, shuffle, repeat, and next/previous navigation.
//!
//! The model separates *what's in the queue* from *the order it plays in*:
//! `songs` holds tracks in the order they were added, while `order` is a permutation of
//! indices into `songs` describing the actual play sequence. `cursor` points at the
//! current track within `order`. Shuffle just reshuffles `order` (keeping the current
//! track current); turning it off restores natural order. This keeps every operation a
//! pure index manipulation — easy to reason about and unit-test.

use crate::api::Song;
use serde::{Deserialize, Serialize};

/// Hard cap on queued tracks (priority #1: bounded memory).
const MAX: usize = 999;

/// Repeat mode, cycled by the `r` key.
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

    /// A language-neutral Unicode glyph for the `R:` status-line button: the media
    /// "repeat-all" / "repeat-one" symbols, and a cross when off. Identical in every UI language.
    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "✗",
            Repeat::All => "🔁",
            Repeat::One => "🔂",
        }
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
    rng: fastrand::Rng,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            songs: Vec::new(),
            order: Vec::new(),
            cursor: 0,
            shuffle: false,
            repeat: Repeat::Off,
            rng: fastrand::Rng::new(),
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

    pub fn contains_video_id(&self, video_id: &str) -> bool {
        self.songs.iter().any(|s| s.video_id == video_id)
    }

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

    /// How many tracks remain *after* the current one in the play order. Drives the
    /// autoplay/radio hook (extend when this runs low). Zero when empty or at the end.
    pub fn remaining(&self) -> usize {
        self.order.len().saturating_sub(self.cursor + 1)
    }

    /// Append `more` tracks to the end of the queue, respecting the [`MAX`] cap. Returns
    /// the number actually added — fewer than requested (or zero) when near the cap, so
    /// the caller can report the *real* count rather than what was asked for. The new
    /// tracks are made reachable from the current cursor; with shuffle on they're
    /// randomized among themselves so they don't clump in insertion order.
    pub fn extend(&mut self, more: Vec<Song>) -> usize {
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
    pub fn set(&mut self, mut songs: Vec<Song>, start: usize) {
        songs.truncate(MAX);
        let start = start.min(songs.len().saturating_sub(1));
        self.songs = songs;
        self.rebuild_order(start);
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
    /// AI assistant's `get_queue` context snapshot.
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

    /// Toggle shuffle, keeping the current track current.
    pub fn toggle_shuffle(&mut self) {
        self.set_shuffle(!self.shuffle);
    }

    /// Set shuffle explicitly, keeping the current track current.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        if self.shuffle == shuffle {
            return;
        }
        self.shuffle = shuffle;
        if let Some(&current_idx) = self.order.get(self.cursor) {
            self.rebuild_order(current_idx);
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
}
