use std::collections::HashSet;

use super::{DaemonEngine, RequesterKey};
use crate::api::Song;
use crate::playlists::AddResult;
use crate::remote::proto::{
    LibraryPageModel, PlaylistDetailModel, PlaylistSummaryModel, RemoteResponse, ResponseData,
};

impl DaemonEngine {
    pub fn playlists_rev(&self) -> u64 {
        self.playlists_rev
    }

    pub fn library_invalidations(&self) -> u64 {
        self.library_invalidations
    }

    /// Test-only: seed a local playlist without touching persistence gates.
    #[cfg(test)]
    pub(in crate::daemon) fn test_add_playlist(&mut self, name: &str) -> String {
        self.playlists
            .create(name)
            .expect("test playlist name is valid")
    }

    /// Resolve a local playlist id to its display name (transfer destinations).
    pub(in crate::daemon) fn playlist_name(&self, playlist_id: &str) -> Option<String> {
        self.playlists
            .find(playlist_id)
            .map(|playlist| playlist.name.clone())
    }

    pub(in crate::daemon) fn transfer_playlists_snapshot(&self) -> crate::playlists::Playlists {
        self.playlists.clone()
    }

    /// Persist a transfer candidate before swapping it into the live daemon projection.
    pub(in crate::daemon) fn commit_transfer_playlists_candidate(
        &mut self,
        candidate: crate::playlists::Playlists,
    ) -> Result<(), crate::transfer::local_playlist::LocalPlaylistStoreError> {
        self.persist_transfer_playlists_candidate(&candidate)?;
        self.playlists = candidate;
        self.bump_playlists_rev();
        Ok(())
    }

    pub fn playlists_models(&self) -> Vec<PlaylistSummaryModel> {
        self.playlists
            .list()
            .iter()
            .map(|playlist| PlaylistSummaryModel {
                id: playlist.id.clone(),
                name: playlist.name.clone(),
                count: playlist.songs.len() as u64,
                description: None,
            })
            .collect()
    }

    fn library_tracks(&self, scope: &str, filter: &str) -> Result<Vec<Song>, &'static str> {
        let mut tracks = match scope {
            "favorites" => self.library.favorites.clone(),
            "history" => self.library.history.iter().cloned().collect(),
            "radio_likes" => self.library.radio_favorites.clone(),
            "radio_history" => self.library.radios.iter().cloned().collect(),
            "all" => {
                let mut seen = HashSet::new();
                self.library
                    .favorites
                    .iter()
                    .chain(self.library.history.iter())
                    .filter(|song| seen.insert(song.video_id.clone()))
                    .cloned()
                    .collect()
            }
            _ => return Err("bad_request"),
        };
        let filter = filter.trim().to_lowercase();
        if !filter.is_empty() {
            tracks.retain(|song| {
                song.title.to_lowercase().contains(&filter)
                    || song.artist.to_lowercase().contains(&filter)
            });
        }
        Ok(tracks)
    }

    pub(super) fn gui_fetch_library_page(
        &self,
        scope: &str,
        filter: &str,
        offset: usize,
        limit: usize,
    ) -> RemoteResponse {
        let tracks = match self.library_tracks(scope, filter) {
            Ok(tracks) => tracks,
            Err(reason) => return RemoteResponse::err(reason),
        };
        let total = tracks.len() as u64;
        let tracks = tracks
            .iter()
            .skip(offset)
            .take(limit)
            .map(|song| crate::remote::publish::track_model(song, &self.library, &self.signals))
            .collect();
        response_with_data(
            "library page",
            ResponseData::LibraryPage(LibraryPageModel {
                scope: scope.to_owned(),
                filter: filter.trim().to_owned(),
                offset: offset as u64,
                total,
                tracks,
            }),
        )
    }

    pub(super) async fn gui_library_play(&mut self, scope: &str, filter: &str) -> RemoteResponse {
        let tracks = match self.library_tracks(scope, filter) {
            Ok(tracks) => tracks,
            Err(reason) => return RemoteResponse::err(reason),
        };
        self.gui_replace_queue(tracks.into_iter().take(999).collect())
            .await
    }

    pub(super) async fn gui_library_enqueue(
        &mut self,
        scope: &str,
        filter: &str,
    ) -> RemoteResponse {
        let tracks = match self.library_tracks(scope, filter) {
            Ok(tracks) => tracks,
            Err(reason) => return RemoteResponse::err(reason),
        };
        self.gui_enqueue_songs(tracks).await
    }

    pub(super) fn gui_library_remove(&mut self, scope: &str, video_id: &str) -> RemoteResponse {
        let removed = match scope {
            "favorites" => {
                let mut removed = false;
                while let Some(index) = self
                    .library
                    .favorites
                    .iter()
                    .position(|song| song.video_id == video_id)
                {
                    removed |= self.library.remove_favorite_at(index);
                }
                removed
            }
            "history" => {
                let mut removed = false;
                while let Some(index) = self
                    .library
                    .history
                    .iter()
                    .position(|song| song.video_id == video_id)
                {
                    removed |= self.library.remove_history_at(index);
                }
                removed
            }
            "radio_likes" => {
                self.library
                    .radio_favorites
                    .iter()
                    .any(|song| song.video_id == video_id)
                    && self.library.remove_radio_favorite_by_id(video_id)
            }
            "radio_history" => {
                self.library
                    .radios
                    .iter()
                    .any(|song| song.video_id == video_id)
                    && self.library.remove_radio_recent_by_id(video_id)
            }
            _ => return RemoteResponse::err("bad_request"),
        };
        if !removed {
            return RemoteResponse::err("unknown_track");
        }
        self.save_library("daemon GUI library removal");
        self.library_invalidations = self.library_invalidations.wrapping_add(1);
        RemoteResponse::ok("library track removed".to_owned())
    }

    pub(super) fn gui_playlist_create(&mut self, name: &str) -> RemoteResponse {
        let Some(id) = self.playlists.create(name) else {
            return RemoteResponse::err("bad_request");
        };
        self.save_playlists("daemon GUI playlist create");
        self.bump_playlists_rev();
        RemoteResponse::ok(id)
    }

    pub(super) fn gui_playlist_delete(&mut self, playlist_id: &str) -> RemoteResponse {
        if self.playlists.delete(playlist_id).is_none() {
            return RemoteResponse::err("unknown_playlist");
        }
        self.save_playlists("daemon GUI playlist delete");
        self.bump_playlists_rev();
        RemoteResponse::ok("playlist deleted".to_owned())
    }

    pub(super) fn gui_playlist_add_tracks(
        &mut self,
        requester: Option<&RequesterKey>,
        playlist_id: &str,
        video_ids: &[String],
    ) -> RemoteResponse {
        if self.playlists.find(playlist_id).is_none() {
            return RemoteResponse::err("unknown_playlist");
        }
        let songs = match video_ids
            .iter()
            .map(|id| self.resolve_video_id(requester, id).ok_or("unknown_track"))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(songs) => songs,
            Err(reason) => return RemoteResponse::err(reason),
        };
        let results = self.playlists.add_many(playlist_id, songs);
        let changed = results.contains(&AddResult::Added);
        if changed {
            self.save_playlists("daemon GUI playlist add tracks");
            self.bump_playlists_rev();
        }
        if results.contains(&AddResult::Full) {
            RemoteResponse::err("queue_full")
        } else {
            RemoteResponse::ok("playlist tracks added".to_owned())
        }
    }

    pub(super) fn gui_playlist_remove_track(
        &mut self,
        playlist_id: &str,
        video_id: &str,
    ) -> RemoteResponse {
        if self.playlists.find(playlist_id).is_none() {
            return RemoteResponse::err("unknown_playlist");
        }
        if !self.playlists.remove_song(playlist_id, video_id) {
            return RemoteResponse::err("unknown_track");
        }
        self.save_playlists("daemon GUI playlist remove track");
        self.bump_playlists_rev();
        RemoteResponse::ok("playlist track removed".to_owned())
    }

    pub(super) async fn gui_playlist_play(&mut self, playlist_id: &str) -> RemoteResponse {
        let Some(playlist) = self.playlists.find(playlist_id) else {
            return RemoteResponse::err("unknown_playlist");
        };
        self.gui_replace_queue(playlist.songs.clone()).await
    }

    pub(super) fn gui_fetch_playlist_detail(&self, playlist_id: &str) -> RemoteResponse {
        let Some(playlist) = self.playlists.find(playlist_id) else {
            return RemoteResponse::ok("playlist not found".to_owned());
        };
        response_with_data(
            "playlist detail",
            ResponseData::PlaylistDetail(PlaylistDetailModel {
                id: playlist.id.clone(),
                name: playlist.name.clone(),
                description: None,
                tracks: playlist
                    .songs
                    .iter()
                    .map(|song| {
                        crate::remote::publish::track_model(song, &self.library, &self.signals)
                    })
                    .collect(),
            }),
        )
    }

    /// The GUI rating chip's `rate` command. Only `cycle` on the CURRENT track is
    /// accepted (its sole sender binds the player model's current track); the cycle
    /// transitions mirror the TUI's `Action::CycleRating` (src/app/player.rs) lockstep —
    /// neutral → like → dislike → neutral over library favorites + dislike signals.
    /// The daemon skips the App-only session-event/affinity recorder, matching its
    /// OS-media rating path (`media_set_rating`).
    pub(super) fn gui_rate(
        &mut self,
        video_id: &str,
        rating: crate::remote::proto::RateChange,
    ) -> RemoteResponse {
        if rating != crate::remote::proto::RateChange::Cycle {
            return RemoteResponse::err("not_supported");
        }
        let Some(song) = self.queue.current().cloned() else {
            return RemoteResponse::err("unknown_track");
        };
        if song.video_id != video_id {
            return RemoteResponse::err("unknown_track");
        }
        if song.is_radio_station() {
            self.library.toggle_favorite(&song);
            self.save_library("daemon GUI radio rating");
            self.library_invalidations = self.library_invalidations.wrapping_add(1);
            return RemoteResponse::ok("rating cycled".to_string());
        }
        let artist_key = crate::signals::normalize_artist(&song.artist);
        let now = crate::signals::unix_now();
        let liked = self.library.is_favorite(&song.video_id);
        let disliked = self.signals.is_disliked(&song.video_id);
        match (liked, disliked) {
            // neutral → like
            (false, false) => {
                let now_fav = self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, now_fav, now);
            }
            // like → dislike
            (true, _) => {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
                self.signals
                    .toggle_dislike(&song.video_id, &artist_key, now);
            }
            // dislike → neutral: signals-only, like the App leg — no library write and
            // no invalidation push for a mutation the library never saw.
            (false, true) => {
                self.signals
                    .toggle_dislike(&song.video_id, &artist_key, now);
                self.save_signals("daemon GUI rating signals");
                return RemoteResponse::ok("rating cycled".to_string());
            }
        }
        self.save_library("daemon GUI rating library");
        self.save_signals("daemon GUI rating signals");
        self.library_invalidations = self.library_invalidations.wrapping_add(1);
        RemoteResponse::ok("rating cycled".to_string())
    }

    pub(super) async fn gui_replace_queue(&mut self, songs: Vec<Song>) -> RemoteResponse {
        if songs.is_empty() {
            return RemoteResponse::err("queue_empty");
        }
        let previous = self.queue.snapshot();
        self.queue.set(songs, 0);
        self.load_current_or_restore_queue(previous)
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|error| RemoteResponse::err(error.reason()))
    }

    async fn gui_enqueue_songs(&mut self, songs: Vec<Song>) -> RemoteResponse {
        if songs.is_empty() {
            return RemoteResponse::err("queue_empty");
        }
        if !self.queue.has_capacity_for(songs.len()) {
            return RemoteResponse::err("queue_full");
        }
        let previous = self.queue.snapshot();
        let old_len = self.queue.len();
        let was_idle = self.loaded_video_id.is_none();
        let expected = songs.len();
        let added = if self.config.effective_enqueue_next() && !was_idle {
            self.queue.insert_next_many(songs)
        } else {
            self.queue.extend(songs)
        };
        debug_assert_eq!(added, expected, "queue capacity was preflighted");
        if was_idle {
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            return self
                .load_current_or_restore_queue(previous)
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|error| RemoteResponse::err(error.reason()));
        }
        self.save_session();
        RemoteResponse::status(self.status())
    }

    pub(super) fn bump_playlists_rev(&mut self) {
        self.playlists_rev = self.playlists_rev.wrapping_add(1);
    }
}

fn response_with_data(message: &str, data: ResponseData) -> RemoteResponse {
    let mut response = RemoteResponse::ok(message.to_owned());
    response.data = Some(data);
    response
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::library::Library;

    fn song(id: &str, title: &str, artist: &str) -> Song {
        Song::remote(id, title, artist, "3:00")
    }

    fn engine() -> DaemonEngine {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true;
        engine
    }

    fn reason(response: &RemoteResponse) -> Option<&str> {
        response.reason.as_deref()
    }

    #[test]
    fn library_scopes_filter_and_all_dedup_preserve_order() {
        let mut engine = engine();
        engine.library = Library {
            favorites: vec![
                song("fav", "Favorite Song", "Alpha"),
                song("dup", "Favorite Copy", "Beta"),
            ],
            history: VecDeque::from(vec![
                song("new", "Newest History", "Gamma"),
                song("dup", "History Copy", "Delta"),
                song("old", "Oldest History", "Echo"),
            ]),
            radio_favorites: vec![song("radio-like", "Liked Radio", "Station")],
            radios: VecDeque::from(vec![song("radio-new", "Newest Radio", "Broadcaster")]),
            ..Library::default()
        };

        let ids = |tracks: Vec<Song>| {
            tracks
                .into_iter()
                .map(|track| track.video_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(engine.library_tracks("all", "").unwrap()),
            ["fav", "dup", "new", "old"]
        );
        assert_eq!(
            ids(engine.library_tracks("history", "").unwrap()),
            ["new", "dup", "old"]
        );
        assert_eq!(
            ids(engine.library_tracks("radio_likes", "").unwrap()),
            ["radio-like"]
        );
        assert_eq!(
            ids(engine.library_tracks("radio_history", "").unwrap()),
            ["radio-new"]
        );
        assert_eq!(
            ids(engine.library_tracks("all", "  gAmMa ").unwrap()),
            ["new"]
        );
        assert!(matches!(
            engine.library_tracks("missing", ""),
            Err("bad_request")
        ));
    }

    #[test]
    fn library_page_reports_filtered_total_and_window() {
        let mut engine = engine();
        engine.library.favorites = vec![
            song("a", "Match One", "Artist"),
            song("b", "Match Two", "Artist"),
            song("c", "No", "Other"),
        ];
        let response = engine.gui_fetch_library_page("favorites", " match ", 1, 1);
        let Some(ResponseData::LibraryPage(page)) = response.data else {
            panic!("expected library page data");
        };
        assert_eq!(page.scope, "favorites");
        assert_eq!(page.filter, "match");
        assert_eq!(page.offset, 1);
        assert_eq!(page.total, 2);
        assert_eq!(page.tracks.len(), 1);
        assert_eq!(page.tracks[0].video_id, "b");
    }

    #[test]
    fn library_remove_covers_scopes_reasons_and_the_invalidation_counter() {
        let mut engine = engine();
        engine.library = Library {
            favorites: vec![song("fav", "Favorite Song", "Alpha")],
            history: VecDeque::from(vec![
                song("dup", "History Copy", "Beta"),
                song("dup", "History Copy Again", "Beta"),
            ]),
            radio_favorites: vec![song("radio-like", "Liked Radio", "Station")],
            ..Library::default()
        };
        let before = engine.library_invalidations();

        // Concrete scopes remove and bump the invalidation counter…
        assert!(engine.gui_library_remove("favorites", "fav").ok);
        assert!(engine.library.favorites.is_empty());
        assert_eq!(engine.library_invalidations(), before + 1);

        // …including every duplicate history row for the id.
        assert!(engine.gui_library_remove("history", "dup").ok);
        assert!(engine.library.history.is_empty());
        assert_eq!(engine.library_invalidations(), before + 2);

        assert!(engine.gui_library_remove("radio_likes", "radio-like").ok);
        assert_eq!(engine.library_invalidations(), before + 3);

        // Misses and the synthetic/unknown scopes reject without a counter bump.
        let miss = engine.gui_library_remove("favorites", "gone");
        assert_eq!(reason(&miss), Some("unknown_track"));
        let synthetic = engine.gui_library_remove("all", "fav");
        assert_eq!(reason(&synthetic), Some("bad_request"));
        let unknown = engine.gui_library_remove("nonsense", "fav");
        assert_eq!(reason(&unknown), Some("bad_request"));
        assert_eq!(engine.library_invalidations(), before + 3);
    }

    #[test]
    fn why_gem_provenance_records_dedups_and_answers_fetch() {
        use crate::remote::proto::{ResponseData, WhyGemModel};
        let mut engine = engine();
        let rev = engine.why_gem_rev();
        engine.record_why_gem_picks("DJ Gem", &[song("a", "A", "Alpha"), song("b", "B", "Beta")]);
        assert_eq!(engine.why_gem_ids(), vec!["a".to_owned(), "b".to_owned()]);
        assert_ne!(engine.why_gem_rev(), rev);

        // Identical re-record is a no-op (no rev churn, no provenance re-push).
        let rev = engine.why_gem_rev();
        engine.record_why_gem_picks("DJ Gem", &[song("a", "A", "Alpha")]);
        assert_eq!(engine.why_gem_rev(), rev);

        // A different slot updates in place.
        engine.record_why_gem(
            "a".to_owned(),
            WhyGemModel {
                slot: "balanced".to_owned(),
                reasons: Vec::new(),
                confidence: None,
            },
        );
        assert_ne!(engine.why_gem_rev(), rev);
        assert_eq!(engine.why_gem_ids().len(), 2);

        let hit = engine.gui_fetch_why_gem("a");
        assert!(hit.ok);
        match hit.data {
            Some(ResponseData::WhyGem(model)) => assert_eq!(model.slot, "balanced"),
            other => panic!("expected why-gem data, got {other:?}"),
        }
        let miss = engine.gui_fetch_why_gem("zzz");
        assert!(miss.ok);
        assert!(miss.data.is_none(), "unknown track answers with no data");
    }

    #[tokio::test]
    async fn playlist_crud_returns_stable_reason_codes() {
        let mut engine = engine();

        assert_eq!(
            reason(&engine.gui_playlist_create("   ")),
            Some("bad_request")
        );
        assert_eq!(
            reason(&engine.gui_playlist_delete("missing")),
            Some("unknown_playlist")
        );

        let created = engine.gui_playlist_create("Road Mix");
        assert!(created.ok);
        assert_eq!(engine.playlists_rev(), 1);
        assert_eq!(created.message.as_deref(), Some("road-mix"));

        assert_eq!(
            reason(&engine.gui_playlist_add_tracks(None, "missing", &["dQw4w9WgXcQ".to_owned()])),
            Some("unknown_playlist")
        );
        assert_eq!(
            reason(&engine.gui_playlist_add_tracks(
                None,
                "road-mix",
                &["bad/not/video".to_owned()]
            )),
            Some("unknown_track")
        );
        assert!(
            engine
                .gui_playlist_add_tracks(None, "road-mix", &["dQw4w9WgXcQ".to_owned()])
                .ok
        );
        assert_eq!(engine.playlists_rev(), 2);
        assert!(
            engine
                .gui_playlist_add_tracks(None, "road-mix", &["dQw4w9WgXcQ".to_owned()])
                .ok
        );
        assert_eq!(engine.playlists_rev(), 2, "duplicate does not mutate");

        assert_eq!(
            reason(&engine.gui_playlist_remove_track("missing", "dQw4w9WgXcQ")),
            Some("unknown_playlist")
        );
        assert_eq!(
            reason(&engine.gui_playlist_remove_track("road-mix", "M7lc1UVf-VE")),
            Some("unknown_track")
        );
        assert!(
            engine
                .gui_playlist_remove_track("road-mix", "dQw4w9WgXcQ")
                .ok
        );
        assert_eq!(engine.playlists_rev(), 3);
        assert_eq!(
            reason(&engine.gui_playlist_play("road-mix").await),
            Some("queue_empty")
        );

        let missing = engine.gui_fetch_playlist_detail("missing");
        assert!(missing.ok);
        assert!(missing.data.is_none());
        assert!(engine.gui_playlist_delete("road-mix").ok);
        assert_eq!(engine.playlists_rev(), 4);
    }
}
