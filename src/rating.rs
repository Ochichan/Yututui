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

/// Session-only recommendation feedback emitted by an explicit track-rating change.
///
/// Persistent affinity still lives in [`Signals`]. This short-lived signal lets both playback
/// owners apply the same immediate rerank nudge without teaching radio favorites as track taste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRatingSignal {
    Like,
    Dislike,
}

impl SessionRatingSignal {
    pub const fn affinity_delta(self) -> f32 {
        match self {
            Self::Like => 0.15,
            Self::Dislike => -0.40,
        }
    }
}

/// Translate one semantic track-rating transition into its session recommendation signal.
///
/// Projection-only repairs and transitions to neutral are deliberately silent. Radio stations
/// retain their independent favorite collection and never enter track recommendation feedback.
pub fn session_signal(song: &Song, change: RatingChange) -> Option<SessionRatingSignal> {
    if song.is_radio_station() || change.before == change.after {
        return None;
    }
    match change.after {
        Rating::Liked => Some(SessionRatingSignal::Like),
        Rating::Disliked => Some(SessionRatingSignal::Dislike),
        Rating::Neutral => None,
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

/// Toggle only the liked side of the tri-state register.
///
/// Radio stations retain their separate binary favorite collection and never create track
/// affinity signals.
pub fn toggle_liked(
    library: &mut Library,
    signals: &mut Signals,
    song: &Song,
    now: i64,
) -> RatingChange {
    if song.is_radio_station() {
        let before = if library.is_favorite(&song.video_id) {
            Rating::Liked
        } else {
            Rating::Neutral
        };
        let after = if library.toggle_favorite(song) {
            Rating::Liked
        } else {
            Rating::Neutral
        };
        return RatingChange {
            before,
            after,
            library_changed: true,
            signals_changed: false,
        };
    }
    let target = if current(library, signals, &song.video_id) == Rating::Liked {
        Rating::Neutral
    } else {
        Rating::Liked
    };
    set(library, signals, song, target, now)
}

/// Toggle only the disliked side of the tri-state register.
///
/// A radio station has no disliked state, so that request is an explicit no-op.
pub fn toggle_disliked(
    library: &mut Library,
    signals: &mut Signals,
    song: &Song,
    now: i64,
) -> RatingChange {
    let before = current(library, signals, &song.video_id);
    if song.is_radio_station() {
        return RatingChange {
            before,
            after: before,
            library_changed: false,
            signals_changed: false,
        };
    }
    let target = if before == Rating::Disliked {
        Rating::Neutral
    } else {
        Rating::Disliked
    };
    set(library, signals, song, target, now)
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
    if song.is_radio_station() {
        return toggle_liked(library, signals, song, now);
    }
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

    #[test]
    fn liked_toggle_repairs_dislike_and_radio_stays_out_of_signals() {
        let song = song();
        let mut library = Library::default();
        let mut signals = Signals::default();
        set(&mut library, &mut signals, &song, Rating::Disliked, 1);
        assert_eq!(
            toggle_liked(&mut library, &mut signals, &song, 2).after,
            Rating::Liked
        );
        assert!(!signals.is_disliked(&song.video_id));

        let mut station = Song::remote("station", "Station", "Radio", "");
        station.playable = Some(crate::api::PlayableRef::RadioStream {
            url: "https://radio.example/station.mp3".to_owned(),
        });
        assert_eq!(
            cycle(&mut library, &mut signals, &station, 3).after,
            Rating::Liked
        );
        assert_eq!(
            cycle(&mut library, &mut signals, &station, 4).after,
            Rating::Neutral
        );
        assert!(!signals.is_disliked(&station.video_id));
    }

    #[test]
    fn session_signal_only_tracks_semantic_non_radio_feedback() {
        let track = song();
        let liked = RatingChange {
            before: Rating::Neutral,
            after: Rating::Liked,
            library_changed: true,
            signals_changed: true,
        };
        assert_eq!(
            session_signal(&track, liked),
            Some(SessionRatingSignal::Like)
        );
        assert_eq!(SessionRatingSignal::Like.affinity_delta(), 0.15);
        assert_eq!(SessionRatingSignal::Dislike.affinity_delta(), -0.40);

        let projection_repair = RatingChange {
            before: Rating::Disliked,
            after: Rating::Disliked,
            library_changed: true,
            signals_changed: false,
        };
        assert_eq!(session_signal(&track, projection_repair), None);

        let mut station = Song::remote("station", "Station", "Radio", "");
        station.playable = Some(crate::api::PlayableRef::RadioStream {
            url: "https://radio.example/station.mp3".to_owned(),
        });
        assert_eq!(session_signal(&station, liked), None);
    }
}
