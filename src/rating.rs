//! Shared tri-state rating reducer used by both interactive and daemon owners.

use crate::api::Song;
use crate::library::Library;
use crate::personal_state::Rating;
use crate::signals::Signals;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatingChange {
    pub before: Rating,
    pub after: Rating,
    pub library_changed: bool,
    pub signals_changed: bool,
}

impl RatingChange {
    pub fn changed(self) -> bool {
        self.before != self.after || self.library_changed || self.signals_changed
    }
}

/// Read the one canonical rating. A contradictory legacy projection resolves to Disliked.
pub fn current(library: &Library, signals: &Signals, video_id: &str) -> Rating {
    if signals.is_disliked(video_id) {
        Rating::Disliked
    } else if library.is_favorite(video_id) {
        Rating::Liked
    } else {
        Rating::Neutral
    }
}

pub fn cycled(rating: Rating) -> Rating {
    match rating {
        Rating::Neutral => Rating::Liked,
        Rating::Liked => Rating::Disliked,
        Rating::Disliked => Rating::Neutral,
    }
}

/// Apply one explicit rating and repair any legacy liked+disliked contradiction.
pub fn set(
    library: &mut Library,
    signals: &mut Signals,
    song: &Song,
    target: Rating,
    now: i64,
) -> RatingChange {
    let before = current(library, signals, &song.video_id);
    let was_liked = library.is_favorite(&song.video_id);
    let was_disliked = signals.is_disliked(&song.video_id);
    let artist_key = crate::signals::normalize_artist(&song.artist);
    let mut library_changed = false;
    let mut signals_changed = false;

    if was_liked && target != Rating::Liked {
        library.toggle_favorite(song);
        signals.record_like(&song.video_id, &artist_key, false, now);
        library_changed = true;
        signals_changed = true;
    }
    if was_disliked && target != Rating::Disliked {
        signals.toggle_dislike(&song.video_id, &artist_key, now);
        signals_changed = true;
    }

    match target {
        Rating::Neutral => {}
        Rating::Liked => {
            if was_disliked {
                // Clearing the hard block above restores its old affinity. The explicit like then
                // adds the ordinary like lift.
            }
            if !library.is_favorite(&song.video_id) {
                let liked = library.toggle_favorite(song);
                debug_assert!(liked);
                signals.record_like(&song.video_id, &artist_key, true, now);
                library_changed = true;
                signals_changed = true;
            }
        }
        Rating::Disliked => {
            if !signals.is_disliked(&song.video_id) {
                let disliked = signals.toggle_dislike(&song.video_id, &artist_key, now);
                debug_assert!(disliked);
                signals_changed = true;
            }
            // `was_liked && was_disliked` enters with `before == Disliked`. The favorite was
            // removed above even though the semantic rating did not change.
        }
    }

    RatingChange {
        before,
        after: current(library, signals, &song.video_id),
        library_changed,
        signals_changed,
    }
}

pub fn cycle(library: &mut Library, signals: &mut Signals, song: &Song, now: i64) -> RatingChange {
    set(
        library,
        signals,
        song,
        cycled(current(library, signals, &song.video_id)),
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song() -> Song {
        Song::remote("track", "Track", "Artist", "3:00")
    }

    #[test]
    fn full_cycle_returns_to_neutral_and_affinity_baseline() {
        let song = song();
        let artist = crate::signals::normalize_artist(&song.artist);
        let mut library = Library::default();
        let mut signals = Signals::default();

        assert_eq!(
            cycle(&mut library, &mut signals, &song, 1).after,
            Rating::Liked
        );
        assert_eq!(
            cycle(&mut library, &mut signals, &song, 2).after,
            Rating::Disliked
        );
        assert_eq!(
            cycle(&mut library, &mut signals, &song, 3).after,
            Rating::Neutral
        );
        assert!(!library.is_favorite(&song.video_id));
        assert!(!signals.is_disliked(&song.video_id));
        assert!(signals.artist_weight(&artist).abs() < f32::EPSILON);
    }

    #[test]
    fn contradictory_legacy_projection_resolves_to_disliked_and_is_repaired() {
        let song = song();
        let mut library = Library::default();
        let mut signals = Signals::default();
        library.toggle_favorite(&song);
        signals.toggle_dislike(&song.video_id, "artist", 1);

        assert_eq!(
            current(&library, &signals, &song.video_id),
            Rating::Disliked
        );
        let change = set(&mut library, &mut signals, &song, Rating::Disliked, 2);
        assert!(change.library_changed);
        assert!(!library.is_favorite(&song.video_id));
        assert!(signals.is_disliked(&song.video_id));
    }
}
