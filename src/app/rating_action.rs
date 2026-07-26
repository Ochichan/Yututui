//! Shared App entry point for explicit favorite/unfavorite actions.

use std::sync::Arc;

use super::{App, Cmd, PersistCmd};
use crate::api::Song;
use crate::personal_state::Rating;

impl App {
    /// Treat the legacy "favorite" gesture as an explicit two-state choice inside the canonical
    /// tri-state register. One personal-state transaction persists both projections atomically.
    pub(in crate::app) fn toggle_song_favorite_rating(&mut self, song: &Song) -> Vec<Cmd> {
        let change = crate::rating::toggle_liked(
            Arc::make_mut(&mut self.library),
            Arc::make_mut(&mut self.signals),
            song,
            crate::signals::unix_now(),
        );
        self.persist_song_rating_change(change)
    }

    pub(in crate::app) fn ensure_song_liked_rating(&mut self, song: &Song) -> Vec<Cmd> {
        let change = crate::rating::set(
            Arc::make_mut(&mut self.library),
            Arc::make_mut(&mut self.signals),
            song,
            Rating::Liked,
            crate::signals::unix_now(),
        );
        self.persist_song_rating_change(change)
    }

    /// Remove selected music favorites as explicit `Liked -> Neutral` operations.
    ///
    /// The collection view may delete several rows at once, so all rating mutations share one
    /// timestamp and one personal-state persistence marker.
    pub(in crate::app) fn neutralize_song_ratings(&mut self, songs: &[Song]) -> Vec<Cmd> {
        let now = crate::signals::unix_now();
        let mut changed = false;
        for song in songs {
            changed |= crate::rating::set(
                Arc::make_mut(&mut self.library),
                Arc::make_mut(&mut self.signals),
                song,
                Rating::Neutral,
                now,
            )
            .changed();
        }
        self.persist_any_song_rating_change(changed)
    }

    fn persist_song_rating_change(&mut self, change: crate::rating::RatingChange) -> Vec<Cmd> {
        self.persist_any_song_rating_change(change.changed())
    }

    fn persist_any_song_rating_change(&mut self, changed: bool) -> Vec<Cmd> {
        if changed {
            self.dirty = true;
            // Every personal-state marker snapshots the ledger and all projections together.
            vec![Cmd::Persist(PersistCmd::Library)]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorite_action_repairs_dislike_then_clears_to_neutral_in_daemon_parity() {
        let song = Song::remote("server-song", "Song", "Artist", "3:00");
        let mut app = App::new(50);
        let mut daemon_library = crate::library::Library::default();
        let mut daemon_signals = crate::signals::Signals::default();

        crate::rating::set(
            Arc::make_mut(&mut app.library),
            Arc::make_mut(&mut app.signals),
            &song,
            Rating::Disliked,
            1,
        );
        crate::rating::set(
            &mut daemon_library,
            &mut daemon_signals,
            &song,
            Rating::Disliked,
            1,
        );

        assert_eq!(app.toggle_song_favorite_rating(&song).len(), 1);
        crate::rating::toggle_liked(&mut daemon_library, &mut daemon_signals, &song, 2);
        assert_eq!(
            crate::rating::current(&app.library, &app.signals, &song.video_id),
            Rating::Liked
        );
        assert_eq!(
            crate::rating::current(&daemon_library, &daemon_signals, &song.video_id),
            Rating::Liked
        );
        assert_eq!(
            app.signals
                .artist_weight(&crate::signals::normalize_artist(&song.artist)),
            daemon_signals.artist_weight(&crate::signals::normalize_artist(&song.artist))
        );

        assert_eq!(app.toggle_song_favorite_rating(&song).len(), 1);
        crate::rating::toggle_liked(&mut daemon_library, &mut daemon_signals, &song, 3);
        assert_eq!(
            crate::rating::current(&app.library, &app.signals, &song.video_id),
            Rating::Neutral
        );
        assert_eq!(
            crate::rating::current(&daemon_library, &daemon_signals, &song.video_id),
            Rating::Neutral
        );
    }
}
