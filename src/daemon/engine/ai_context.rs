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
        self.record_why_gem_picks("DJ Gem", &songs);
        let _ = self.gui_replace_queue(songs).await;
    }

    pub(in crate::daemon) async fn ai_enqueue(&mut self, songs: Vec<Song>) {
        let _ = self.extend_queue_from_picks(songs, "DJ Gem").await;
    }

    /// v1 pick provenance (user-scoped decision): record WHERE a row came from — the
    /// chat ("DJ Gem") or the autoplay streaming mode — with whatever the pick context
    /// carried. Reasons/confidence generation is deliberately out of scope; unknown
    /// tracks answer `fetch_why_gem` with no data (the GUI's null path).
    pub(in crate::daemon) fn record_why_gem_picks(&mut self, slot: &str, songs: &[Song]) {
        for song in songs {
            self.record_why_gem(
                song.video_id.clone(),
                crate::remote::proto::WhyGemModel {
                    slot: slot.to_owned(),
                    reasons: Vec::new(),
                    confidence: None,
                },
            );
        }
    }

    pub(in crate::daemon) fn record_why_gem(
        &mut self,
        video_id: String,
        model: crate::remote::proto::WhyGemModel,
    ) {
        const WHY_GEM_MAX: usize = 999;
        if let Some(entry) = self
            .why_gem
            .iter_mut()
            .find(|(existing, _)| *existing == video_id)
        {
            if entry.1 == model {
                return;
            }
            entry.1 = model;
        } else {
            if self.why_gem.len() >= WHY_GEM_MAX {
                self.why_gem.remove(0);
            }
            self.why_gem.push((video_id, model));
        }
        self.why_gem_rev = self.why_gem_rev.wrapping_add(1);
    }

    pub fn why_gem_rev(&self) -> u64 {
        self.why_gem_rev
    }

    pub fn why_gem_ids(&self) -> Vec<String> {
        self.why_gem.iter().map(|(id, _)| id.clone()).collect()
    }

    pub(super) fn gui_fetch_why_gem(&self, video_id: &str) -> crate::remote::proto::RemoteResponse {
        use crate::remote::proto::{RemoteResponse, ResponseData};
        let mut response = RemoteResponse::ok("why gem".to_owned());
        if let Some((_, model)) = self.why_gem.iter().find(|(id, _)| id == video_id) {
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
