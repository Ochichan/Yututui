use std::sync::OnceLock;
use std::time::Instant;

use super::*;
use crate::listening::{
    BookmarkId, BookmarkRecord, DjPreset, DjPresetId, ListeningLoadReason, ListeningOperation,
    ListeningProjection, PendingSeekOutcome, PlaybackMemoryAction, ResumeClear, ResumePoint,
    ResumeProvenance, format_listening_projection, portable_track, radio_station_target,
    resolve_portable_track,
};
use crate::remote::proto::ListeningRemoteAction;

fn monotonic_millis() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl DaemonEngine {
    pub(super) fn on_listening_track_loaded(
        &mut self,
        song: &Song,
        ordinary: bool,
        cause: crate::crossfade::AdvanceCause,
    ) {
        if !ordinary {
            return;
        }
        self.snapshot_listening_progress();
        let reason = match cause {
            crate::crossfade::AdvanceCause::Manual => ListeningLoadReason::Deliberate,
            crate::crossfade::AdvanceCause::EndOfTrack => ListeningLoadReason::Automatic,
        };
        self.begin_listening_track(song, reason);
    }

    pub(super) fn begin_listening_track(&mut self, song: &Song, reason: ListeningLoadReason) {
        let track = portable_track(song, self.config.listening_local_scope.as_deref());
        self.listening.begin_track(
            track.clone(),
            song.is_radio_station(),
            radio_station_target(song),
        );
        let projection = (self.config.effective_listening_records_enabled()
            && self.config.effective_listening_resume()
            && reason.permits_automatic_resume()
            && !self.listening.has_pending_seek())
        .then(|| ListeningProjection::from_ledger(&self.personal_state).ok())
        .flatten();
        let conflicted = projection
            .as_ref()
            .and_then(|projection| projection.resumes.get(&track.key))
            .is_some_and(crate::listening::ResumeState::is_conflicted);
        let resume = projection
            .as_ref()
            .and_then(|projection| projection.automatic_resume(&track.key).cloned());
        if conflicted && reason == ListeningLoadReason::Deliberate {
            tracing::info!(
                track_key = ?track.key,
                "multiple resume points require an explicit bookmark choice"
            );
        }
        if let Some(point) = resume {
            self.listening
                .request_seek(track.key, point.position_ms, reason);
        }
    }

    pub(super) fn observe_listening_position(&mut self, seconds: f64) {
        if !self.config.effective_listening_records_enabled() {
            self.listening
                .observe_position_only(seconds, monotonic_millis());
            return;
        }
        let actions = self.listening.observe_position_secs(
            seconds,
            monotonic_millis(),
            self.config.effective_listening_resume(),
        );
        self.commit_listening_actions(actions);
    }

    pub(super) fn observe_listening_duration(&mut self, duration: Option<f64>) {
        self.listening.observe_duration_secs(duration);
        self.try_apply_listening_seek();
    }

    pub(super) fn observe_listening_seekable(&mut self, seekable: Option<bool>) {
        self.listening.observe_seekable(seekable);
        self.try_apply_listening_seek();
    }

    pub(super) fn observe_listening_paused(&mut self, paused: bool) {
        let action = self.listening.observe_paused(
            paused,
            self.config.effective_listening_records_enabled()
                && self.config.effective_listening_resume(),
        );
        self.commit_listening_actions(action);
    }

    pub(super) fn snapshot_listening_progress(&mut self) {
        if !self.config.effective_listening_records_enabled() {
            return;
        }
        let action = self
            .listening
            .snapshot_resume(self.config.effective_listening_resume());
        self.commit_listening_actions(action);
    }

    pub(super) fn complete_listening_track(&mut self) {
        let action = self.listening.complete_track();
        if self.config.effective_listening_records_enabled() {
            self.commit_listening_actions(action);
        }
    }

    pub(super) fn clear_listening_track(&mut self) {
        self.listening.clear_track();
    }

    pub(super) fn enable_listening_records(&mut self) -> RemoteResponse {
        if let Err(error) = super::persistence_gate::current_recovery_status() {
            return self.reject_remote_recovery(error);
        }
        if self.config.listening_local_scope.is_none() {
            self.config.listening_local_scope =
                Some(format!("local-scope-{:032x}", fastrand::u128(..)));
        }
        if let Some(song) = self.queue.current() {
            self.listening.rebind_current_track(portable_track(
                song,
                self.config.listening_local_scope.as_deref(),
            ));
        }
        self.config.listening_records_enabled = Some(true);
        self.save_config("enable listening records");
        RemoteResponse::ok("listening records enabled".to_owned())
    }

    pub(super) fn apply_listening_preset(&mut self, preset: DjPreset) -> RemoteResponse {
        let name = preset.name;
        if self.taste.replace_snapshot(preset.snapshot).is_err() {
            return RemoteResponse::err("invalid_dj_preset");
        }
        self.cancel_pending_streaming_request();
        self.listening.set_active_preset_name(Some(name.clone()));
        RemoteResponse::ok(format!("DJ preset loaded: {name}"))
    }

    pub(super) async fn remote_listening(
        &mut self,
        action: ListeningRemoteAction,
    ) -> RemoteResponse {
        if matches!(action, ListeningRemoteAction::Enable) {
            return self.enable_listening_records();
        }
        let projection = match ListeningProjection::from_ledger(&self.personal_state) {
            Ok(projection) => projection,
            Err(_) => return RemoteResponse::err("listening_state_invalid"),
        };
        if matches!(action, ListeningRemoteAction::List) {
            return RemoteResponse::ok(format_listening_projection(&projection));
        }
        if !self.config.effective_listening_records_enabled() {
            return RemoteResponse::err("listening_records_disabled");
        }
        match action {
            ListeningRemoteAction::Enable | ListeningRemoteAction::List => unreachable!(),
            ListeningRemoteAction::BookmarkAdd { label } => {
                let Some(track) = self.listening.current_track().cloned() else {
                    return RemoteResponse::err("nothing_playing");
                };
                let Some(position_ms) = self.listening.current_position_ms() else {
                    return RemoteResponse::err("position_unknown");
                };
                if !self.listening.current_can_seek() {
                    return RemoteResponse::err("track_not_seekable");
                }
                let bookmark_id = BookmarkId::new(format!("bookmark-{:032x}", fastrand::u128(..)))
                    .expect("generated bookmark id is valid");
                let change = ListeningOperation::UpsertBookmark {
                    bookmark: BookmarkRecord {
                        bookmark_id,
                        track,
                        position_ms,
                        label,
                    },
                };
                match self.commit_listening_change(change) {
                    Ok(()) => RemoteResponse::ok("bookmark queued".to_owned()),
                    Err(_) => RemoteResponse::err("listening_write_failed"),
                }
            }
            ListeningRemoteAction::BookmarkDelete { bookmark_id } => {
                let Ok(bookmark_id) = BookmarkId::new(bookmark_id) else {
                    return RemoteResponse::err("bad_bookmark_id");
                };
                match self
                    .commit_listening_change(ListeningOperation::DeleteBookmark { bookmark_id })
                {
                    Ok(()) => RemoteResponse::ok("bookmark delete queued".to_owned()),
                    Err(_) => RemoteResponse::err("listening_write_failed"),
                }
            }
            ListeningRemoteAction::BookmarkJump { bookmark_id } => {
                let Ok(bookmark_id) = BookmarkId::new(bookmark_id) else {
                    return RemoteResponse::err("bad_bookmark_id");
                };
                let Some(bookmarks) = projection.bookmarks.get(&bookmark_id) else {
                    return RemoteResponse::err("bookmark_not_found");
                };
                let [bookmark] = bookmarks.as_slice() else {
                    return RemoteResponse::err("bookmark_conflict");
                };
                let target = bookmark.track.clone();
                self.listening.request_seek(
                    target.key.clone(),
                    bookmark.position_ms,
                    ListeningLoadReason::Bookmark,
                );
                if self
                    .listening
                    .current_track()
                    .is_some_and(|track| track.key == target.key)
                {
                    if !self.listening.current_can_seek() {
                        self.listening.cancel_pending_seek();
                        return RemoteResponse::err("track_not_seekable");
                    }
                    return match self.maybe_apply_listening_seek() {
                        Ok(true) => RemoteResponse::ok("bookmark jump requested".to_owned()),
                        Ok(false) => RemoteResponse::err("saved_position_invalid"),
                        Err(error) => self.reject_player_command(error),
                    };
                }
                let Some(song) = self.resolve_listening_track(&target) else {
                    self.listening.cancel_pending_seek();
                    return RemoteResponse::err("track_unavailable");
                };
                let previous = self.queue.snapshot();
                if !self.queue.play_now(song) {
                    self.listening.cancel_pending_seek();
                    return RemoteResponse::err("queue_full");
                }
                match self.load_current_or_restore_queue(previous).await {
                    Ok(()) => RemoteResponse::ok("bookmark jump requested".to_owned()),
                    Err(error) => {
                        self.listening.cancel_pending_seek();
                        RemoteResponse::err(error.reason())
                    }
                }
            }
            ListeningRemoteAction::Restart => {
                let Some(track) = self.listening.current_track().cloned() else {
                    return RemoteResponse::err("nothing_playing");
                };
                if !self.listening.current_can_seek() {
                    return RemoteResponse::err("track_not_seekable");
                }
                self.listening
                    .request_seek(track.key, 0, ListeningLoadReason::Restart);
                match self.maybe_apply_listening_seek() {
                    Ok(true) => RemoteResponse::ok("track restart requested".to_owned()),
                    Ok(false) => RemoteResponse::err("saved_position_invalid"),
                    Err(error) => self.reject_player_command(error),
                }
            }
            ListeningRemoteAction::PresetSave { name } => {
                let preset_id = DjPresetId::new(format!("preset-{:032x}", fastrand::u128(..)))
                    .expect("generated preset id is valid");
                let snapshot = self.taste.snapshot();
                match self.commit_listening_change(ListeningOperation::UpsertDjPreset {
                    preset: DjPreset {
                        preset_id,
                        name,
                        snapshot,
                    },
                }) {
                    Ok(()) => RemoteResponse::ok("DJ preset queued".to_owned()),
                    Err(_) => RemoteResponse::err("listening_write_failed"),
                }
            }
            ListeningRemoteAction::PresetLoad { preset_id } => {
                let Ok(preset_id) = DjPresetId::new(preset_id) else {
                    return RemoteResponse::err("bad_preset_id");
                };
                let Some(presets) = projection.dj_presets.get(&preset_id) else {
                    return RemoteResponse::err("preset_not_found");
                };
                let [preset] = presets.as_slice() else {
                    return RemoteResponse::err("preset_conflict");
                };
                self.apply_listening_preset(preset.clone())
            }
            ListeningRemoteAction::PresetDelete { preset_id } => {
                let Ok(preset_id) = DjPresetId::new(preset_id) else {
                    return RemoteResponse::err("bad_preset_id");
                };
                match self.commit_listening_change(ListeningOperation::DeleteDjPreset { preset_id })
                {
                    Ok(()) => RemoteResponse::ok("DJ preset delete queued".to_owned()),
                    Err(_) => RemoteResponse::err("listening_write_failed"),
                }
            }
        }
    }

    pub(super) fn commit_listening_change(
        &mut self,
        change: ListeningOperation,
    ) -> Result<(), crate::personal_state::PersonalStateError> {
        if !self.config.effective_listening_records_enabled() {
            return Err(crate::personal_state::PersonalStateError::InvalidOperation(
                "listening records are not enabled",
            ));
        }
        let state = crate::personal_state::append_listening(
            &self.personal_state,
            self.personal_state_device_id.as_ref(),
            change,
            crate::signals::unix_now(),
        )?;
        self.install_personal_state(state);
        self.save_personal_state(
            "daemon listening records",
            crate::persist::StoreKind::PersonalState,
        );
        Ok(())
    }

    fn try_apply_listening_seek(&mut self) {
        if let Err(error) = self.maybe_apply_listening_seek() {
            self.last_error = Some(error.to_string());
            tracing::warn!(%error, "daemon listening seek was rejected");
        }
    }

    fn maybe_apply_listening_seek(&mut self) -> Result<bool, EngineError> {
        let Some(outcome) = self.listening.pending_seek_outcome() else {
            return Ok(false);
        };
        let pending = match outcome {
            PendingSeekOutcome::Ready(pending) => pending,
            PendingSeekOutcome::Invalid(pending) => {
                self.listening.cancel_pending_seek();
                if pending.reason.permits_automatic_resume()
                    && let Some(track) = self.listening.current_track().cloned()
                    && let Some(change) =
                        self.listening_change(PlaybackMemoryAction::ClearResume { track })
                {
                    let _ = self.commit_listening_change(change);
                }
                return Ok(false);
            }
        };
        let track = self.listening.current_track().cloned();
        self.send_active_player_command(
            "listening_resume",
            PlayerCmd::exact_seek(pending.position_ms as f64 / 1_000.0),
        )?;
        self.listening.cancel_pending_seek();
        self.note_seek(pending.position_ms as f64 / 1_000.0);
        self.listening.note_restored_position(pending.position_ms);
        let action = if pending.reason == ListeningLoadReason::Restart {
            track.map(|track| PlaybackMemoryAction::ClearResume { track })
        } else if pending.reason == ListeningLoadReason::Bookmark
            && self.config.effective_listening_resume()
            && self.listening.resume_position_eligible(pending.position_ms)
        {
            track.map(|track| PlaybackMemoryAction::SaveResume {
                track,
                position_ms: pending.position_ms,
            })
        } else {
            None
        };
        if let Some(change) = action.and_then(|action| self.listening_change(action)) {
            let _ = self.commit_listening_change(change);
        }
        Ok(true)
    }

    fn commit_listening_actions(
        &mut self,
        actions: impl IntoIterator<Item = PlaybackMemoryAction>,
    ) {
        for action in actions {
            let Some(change) = self.listening_change(action) else {
                continue;
            };
            if let Err(error) = self.commit_listening_change(change) {
                tracing::warn!(%error, "daemon listening record was not applied");
            }
        }
    }

    fn listening_change(&self, action: PlaybackMemoryAction) -> Option<ListeningOperation> {
        let device_id = crate::personal_state::listening_device_id(
            &self.personal_state,
            self.personal_state_device_id.as_ref(),
        )
        .ok()?;
        let provenance = ResumeProvenance {
            playback_session_id: self.listening.playback_session_id().to_owned(),
            device_id,
        };
        Some(match action {
            PlaybackMemoryAction::SaveResume { track, position_ms } => {
                ListeningOperation::SetResume {
                    point: ResumePoint {
                        track,
                        position_ms,
                        provenance,
                    },
                }
            }
            PlaybackMemoryAction::ClearResume { track } => ListeningOperation::ClearResume {
                clear: ResumeClear { track, provenance },
            },
            PlaybackMemoryAction::RecordPassportVisit(station) => {
                let now = crate::signals::unix_now();
                let first = ListeningProjection::from_ledger(&self.personal_state)
                    .ok()
                    .and_then(|projection| {
                        projection
                            .passport_visits
                            .get(&station.station_uuid)
                            .cloned()
                    })
                    .map_or(now, |visit| visit.first_listened_at_unix.min(now));
                ListeningOperation::RecordPassportVisit {
                    visit: crate::listening::PassportVisit {
                        station_uuid: station.station_uuid,
                        station_name: station.station_name,
                        country_code: station.country_code,
                        first_listened_at_unix: first,
                        last_listened_at_unix: now,
                    },
                }
            }
        })
    }

    fn resolve_listening_track(
        &self,
        track: &crate::personal_state::PortableTrack,
    ) -> Option<Song> {
        self.queue
            .ordered_iter()
            .chain(self.library.favorites.iter())
            .chain(self.library.history.iter())
            .find(|candidate| {
                portable_track(candidate, self.config.listening_local_scope.as_deref()).key
                    == track.key
            })
            .cloned()
            .or_else(|| resolve_portable_track(track, self.config.listening_local_scope.as_deref()))
    }

    #[cfg(test)]
    pub(in crate::daemon) fn seed_listening_for_test(
        &mut self,
        song: &Song,
        position_secs: f64,
        duration_secs: f64,
    ) {
        self.begin_listening_track(song, ListeningLoadReason::Deliberate);
        self.listening.observe_duration_secs(Some(duration_secs));
        self.listening.observe_seekable(Some(true));
        self.observe_listening_position(position_secs);
    }

    #[cfg(test)]
    pub(in crate::daemon) fn reopen_listening_for_test(&mut self, song: &Song, duration_secs: f64) {
        self.begin_listening_track(song, ListeningLoadReason::Deliberate);
        self.observe_listening_duration(Some(duration_secs));
        self.observe_listening_seekable(Some(true));
    }

    #[cfg(test)]
    pub(in crate::daemon) fn stale_pending_allows_resume_for_test(
        &mut self,
        stale_song: &Song,
        target_song: &Song,
    ) -> bool {
        let target = portable_track(target_song, self.config.listening_local_scope.as_deref());
        let Ok(device_id) = crate::personal_state::listening_device_id(
            &self.personal_state,
            self.personal_state_device_id.as_ref(),
        ) else {
            return false;
        };
        let provenance = ResumeProvenance {
            playback_session_id: self.listening.playback_session_id().to_owned(),
            device_id,
        };
        let Ok(state) = crate::personal_state::append_listening(
            &self.personal_state,
            self.personal_state_device_id.as_ref(),
            ListeningOperation::SetResume {
                point: ResumePoint {
                    track: target.clone(),
                    position_ms: 120_000,
                    provenance,
                },
            },
            crate::signals::unix_now(),
        ) else {
            return false;
        };
        self.install_personal_state(state);

        let stale = portable_track(stale_song, self.config.listening_local_scope.as_deref());
        self.listening.begin_track(stale.clone(), false, None);
        self.listening
            .request_seek(stale.key, 45_000, ListeningLoadReason::Bookmark);
        self.begin_listening_track(target_song, ListeningLoadReason::Deliberate);
        self.listening.observe_duration_secs(Some(1_800.0));
        self.listening.observe_seekable(Some(true));
        matches!(
            self.listening.pending_seek_outcome(),
            Some(PendingSeekOutcome::Ready(pending))
                if pending.track_key == target.key && pending.position_ms == 120_000
        )
    }
}
