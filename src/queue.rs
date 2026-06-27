//! The play queue: ordering, shuffle, repeat, and next/previous navigation.
//!
//! The model separates *what's in the queue* from *the order it plays in*:
//! `songs` holds tracks in the order they were added, while `order` is a permutation of
//! indices into `songs` describing the actual play sequence. `cursor` points at the
//! current track within `order`. Shuffle just reshuffles `order` (keeping the current
//! track current); turning it off restores natural order. This keeps every operation a
//! pure index manipulation — easy to reason about and unit-test.

use crate::api::Song;

/// Hard cap on queued tracks (priority #1: bounded memory).
const MAX: usize = 200;

/// Repeat mode, cycled by the `r` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
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

    /// The track currently selected to play, if any.
    pub fn current(&self) -> Option<&Song> {
        let idx = *self.order.get(self.cursor)?;
        self.songs.get(idx)
    }

    /// 1-based `(position, total)` of the current track, for display.
    pub fn position(&self) -> (usize, usize) {
        (self.cursor + 1, self.songs.len())
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

    /// Toggle shuffle, keeping the current track current.
    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
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
        Song {
            video_id: id.to_owned(),
            title: format!("title-{id}"),
            artist: "a".to_owned(),
            duration: "0:10".to_owned(),
        }
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
}
