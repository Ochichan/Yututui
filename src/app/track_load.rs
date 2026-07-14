//! Single-track load validation and post-admission playback projections.
//!
//! Queue/cursor transaction planning lives in `track_transition`; this module owns the
//! mechanics of selecting a playable URL and committing one accepted load (or cleared
//! playback) into the reducer's dependent caches and side effects.

use super::track_transition::{PreparedTrackLoad, SkippedCandidate, SkippedReason};
use super::*;

impl App {
    pub(in crate::app) fn prepare_track_load(
        &self,
        song: Song,
    ) -> Result<PreparedTrackLoad, SkippedReason> {
        if let Some(reason) = song.unplayable_youtube_ref_reason() {
            return Err(SkippedReason::UnplayableYoutube(reason.to_owned()));
        }
        let playback_target = song
            .playback_target_checked()
            .map_err(|error| SkippedReason::InvalidUrl(error.to_string()))?;
        let mut invalid_prefetch = None;
        let (url, prefetched_url) = match self.prefetch.resolved.peek_fresh_url(&song.video_id) {
            Some(prefetched) => match crate::api::validate_playable_url(song.source, &prefetched) {
                Ok(url) => (url, Some(prefetched)),
                Err(error) => {
                    invalid_prefetch = Some((song.video_id.clone(), error.to_string()));
                    (playback_target, None)
                }
            },
            None => (playback_target, None),
        };
        Ok(PreparedTrackLoad {
            song,
            url,
            prefetched_url,
            invalid_prefetch,
        })
    }

    pub(in crate::app) fn track_audio_filter(&self) -> Option<String> {
        match self.settings.as_deref() {
            Some(st) => eq::build_af_string(&st.draft.eq_bands, st.draft.normalize),
            None => self.current_af(),
        }
    }

    pub(in crate::app) fn commit_prepared_track_load(
        &mut self,
        load: PreparedTrackLoad,
    ) -> Vec<Cmd> {
        let PreparedTrackLoad {
            song,
            url,
            prefetched_url,
            invalid_prefetch,
        } = load;
        let mut effects = Vec::new();

        if let Some((video_id, error)) = invalid_prefetch {
            tracing::warn!(%video_id, %error, "dropping invalid prefetched stream URL");
            self.prefetch.resolved.remove(&video_id);
        }
        if let Some(prefetched) = prefetched_url.as_deref() {
            self.prefetch
                .resolved
                .claim_loaded_url(&song.video_id, prefetched);
        }
        self.prefetch.last_load_prefetched = prefetched_url.is_some();
        tracing::info!(url = %url, prefetched = self.prefetch.last_load_prefetched, "load track");

        self.begin_source_logical_item();
        self.reset_progress();
        if let Some(warning) = self.recorder.health_warning.as_ref() {
            // Startup autoplay and ordinary track changes may refresh transient playback status,
            // but recorder recovery/backpressure remains an actionable persistent condition.
            self.status.kind = StatusKind::Error;
            self.status.text.clone_from(warning);
        } else {
            self.status.text.clear();
        }
        self.library.record_play(&song);
        if !song.is_radio_station() {
            self.note_session_activity();
        }
        self.associate_lyrics_with_track(&song.video_id);
        self.prefetch.loaded_video_id = Some(song.video_id.clone());
        self.clear_artwork();

        effects.push(Cmd::Persist(PersistCmd::Library));
        if self.lyrics.visible {
            self.lyrics.loading = true;
            effects.push(fetch_lyrics_cmd(&song));
        }
        if let Some(source) = self.artwork_source(&song) {
            self.art.loading = true;
            effects.push(Cmd::FetchArtwork {
                video_id: song.video_id.clone(),
                source,
            });
        }
        if self.prefetch.enabled()
            && let Some(next) = self.queue.peek_next()
            && let Some(watch_url) = next.prefetch_target()
        {
            let video_id = next.video_id.clone();
            if !self.prefetch.resolved.contains_fresh(&video_id) {
                effects.push(Cmd::Resolve {
                    video_id,
                    watch_url,
                });
            }
        }
        effects.extend(self.maybe_autoplay_extend());
        effects.extend(self.request_romanization_for_songs(std::slice::from_ref(&song)));
        self.dirty = true;
        effects
    }

    pub(in crate::app) fn commit_playback_cleared(&mut self) -> Vec<Cmd> {
        self.supersede_source_recovery();
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::PlaybackCleared);
        self.playback.duration = None;
        self.playback.paused = true;
        self.playback.stream_now_playing = None;
        self.playback.cache_time = None;
        self.playback.cache_time_at = None;
        self.anim.last_shown_sec = -1;
        self.anim.last_shown_cache_sec = -1;
        self.radio_resync_at = None;
        self.prefetch.loaded_video_id = None;
        self.clear_artwork();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn log_skipped_candidates(&mut self, skipped: &[SkippedCandidate]) {
        for candidate in skipped {
            match &candidate.reason {
                SkippedReason::UnplayableYoutube(reason) => tracing::warn!(
                    video_id = %candidate.song.video_id,
                    title = %candidate.song.title,
                    artist = %candidate.song.artist,
                    %reason,
                    "skipping non-playable YouTube entry"
                ),
                SkippedReason::InvalidUrl(error) => tracing::warn!(
                    video_id = %candidate.song.video_id,
                    title = %candidate.song.title,
                    artist = %candidate.song.artist,
                    %error,
                    "skipping track with invalid playback URL"
                ),
            }
        }
    }

    /// Test fixtures use the same typed Stay transition as production. Keeping this adapter
    /// intent-producing prevents reducer tests from reintroducing an eager raw-player bridge.
    #[cfg(test)]
    pub(in crate::app) fn load_song(&mut self, song: Option<Song>) -> Vec<Cmd> {
        let Some(song) = song else {
            return Vec::new();
        };
        debug_assert_eq!(
            self.queue
                .current()
                .map(|current| current.video_id.as_str()),
            Some(song.video_id.as_str()),
            "load_song test adapter expects the current queue entry"
        );
        self.stay_on_current_track()
    }
}
