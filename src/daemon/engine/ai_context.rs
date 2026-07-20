//! Read snapshot and owner-lane mutations used by the daemon DJ Gem host.

use crate::ai::{AiContext, GeminiModel, PlaylistInfo};
use crate::api::Song;
use crate::playlists::AddResult;
use crate::remote::proto::ToggleState;

use super::{DaemonEngine, EngineEffect};

impl DaemonEngine {
    pub(crate) fn build_ai_context(&self) -> AiContext {
        let fmt = |song: &Song| format!("{} — {}", song.title, song.artist);
        let current_radio_station = self
            .queue
            .current()
            .filter(|song| song.is_radio_station())
            .map(fmt);
        AiContext {
            current_track: self.queue.current().map(fmt),
            current_radio_station,
            // The daemon does not yet retain mpv's ICY display labels.
            current_radio_now_playing: None,
            queue_upcoming: self.queue.upcoming(10).into_iter().map(fmt).collect(),
            queue_len: self.queue.len(),
            queue_remaining: self.queue.remaining(),
            recent_history: self.library.history.iter().take(5).map(fmt).collect(),
            favorites: self.library.favorites.iter().take(20).map(fmt).collect(),
            playlists: self
                .playlists
                .list()
                .iter()
                .map(|playlist| PlaylistInfo {
                    id: playlist.id.clone(),
                    name: playlist.name.clone(),
                    count: playlist.songs.len(),
                })
                .collect(),
            search: self.config.effective_search(),
            authenticated: self.config.effective_cookie().is_some(),
            autoplay_streaming: self.streaming,
            repeat_on: self.queue.repeat.is_on(),
        }
    }

    pub(in crate::daemon) fn ai_runtime_config(&self) -> (Option<String>, GeminiModel) {
        (self.config.effective_ai_key(), self.config.gemini_model)
    }

    pub(in crate::daemon) async fn ai_play_tracks(&mut self, songs: Vec<Song>) {
        let video_ids: Vec<String> = songs.iter().map(|song| song.video_id.clone()).collect();
        let response = self.gui_replace_queue(songs).await;
        if response.ok {
            self.record_why_gem_ids(
                "DJ Gem",
                video_ids.into_iter().take(crate::why_gem::WHY_GEM_MAX),
            );
        }
    }

    pub(in crate::daemon) async fn ai_enqueue(&mut self, songs: Vec<Song>) {
        let _ = self.extend_queue_from_picks(songs, "DJ Gem").await;
    }

    /// v1 pick provenance (user-scoped decision): record WHERE a row came from — the
    /// chat ("DJ Gem") or the autoplay streaming mode — with whatever the pick context
    /// carried. Reasons/confidence generation is deliberately out of scope; unknown
    /// tracks answer `fetch_why_gem` with no data (the GUI's null path).
    #[cfg(test)]
    pub(in crate::daemon) fn record_why_gem_picks(&mut self, slot: &str, songs: &[Song]) {
        self.record_why_gem_ids(slot, songs.iter().map(|song| song.video_id.clone()));
    }

    pub(in crate::daemon) fn record_why_gem_ids(
        &mut self,
        slot: &str,
        video_ids: impl IntoIterator<Item = String>,
    ) {
        self.why_gem
            .upsert_many(video_ids.into_iter().map(|video_id| {
                (
                    video_id,
                    crate::remote::proto::WhyGemModel {
                        slot: slot.to_owned(),
                        reasons: Vec::new(),
                        confidence: None,
                    },
                )
            }));
    }

    #[cfg(test)]
    pub(in crate::daemon) fn record_why_gem(
        &mut self,
        video_id: String,
        model: crate::remote::proto::WhyGemModel,
    ) {
        self.why_gem.upsert(video_id, model);
    }

    pub fn why_gem_rev(&self) -> u64 {
        self.why_gem.revision()
    }

    pub fn why_gem_ids(&self) -> Vec<String> {
        self.why_gem.ids()
    }

    /// Remove rows that no longer exist in the live queue before the retained provenance
    /// snapshot is published. Duplicate queue occurrences intentionally keep one shared answer.
    pub(in crate::daemon) fn reconcile_why_gem(&mut self) {
        self.why_gem.retain_video_ids(
            self.queue.rev(),
            self.queue.ordered_iter().map(|song| song.video_id.as_str()),
        );
    }

    pub(in crate::daemon) fn forget_why_gem_picks<'a>(
        &mut self,
        video_ids: impl IntoIterator<Item = &'a str>,
    ) {
        self.why_gem.forget_many(video_ids);
    }

    pub(in crate::daemon) fn clear_why_gem(&mut self) {
        self.why_gem.clear();
    }

    pub(super) fn gui_fetch_why_gem(&self, video_id: &str) -> crate::remote::proto::RemoteResponse {
        use crate::remote::proto::{RemoteResponse, ResponseData};
        let mut response = RemoteResponse::ok("why gem".to_owned());
        if let Some(model) = self.why_gem.get(video_id) {
            response.data = Some(ResponseData::WhyGem(model.clone()));
        }
        response
    }

    /// Revalidate an AI `start_streaming`/`stop_streaming` event on the daemon owner lane.
    /// The response lets [`crate::daemon::ai_host`] surface a structured rejection through its
    /// retained assistant-message projection; effects remain owner-owned.
    pub(in crate::daemon) fn ai_set_autoplay(
        &mut self,
        on: bool,
    ) -> (crate::remote::proto::RemoteResponse, Vec<EngineEffect>) {
        self.set_streaming(if on {
            ToggleState::On
        } else {
            ToggleState::Off
        })
    }

    pub(in crate::daemon) fn ai_create_playlist(&mut self, name: &str) {
        let _ = self.gui_playlist_create(name);
    }

    pub(in crate::daemon) fn ai_add_to_playlist(&mut self, key: &str, songs: Vec<Song>) {
        let Some(playlist_id) = self.playlists.find(key).map(|playlist| playlist.id.clone()) else {
            return;
        };
        let changed = self
            .playlists
            .add_many(&playlist_id, songs)
            .contains(&AddResult::Added);
        if changed {
            self.save_playlists("daemon AI playlist add tracks");
            self.bump_playlists_rev();
        }
    }

    pub(in crate::daemon) async fn ai_play_playlist(&mut self, key: &str) {
        let Some(playlist_id) = self.playlists.find(key).map(|playlist| playlist.id.clone()) else {
            return;
        };
        let _ = self.gui_playlist_play(&playlist_id).await;
    }
}
