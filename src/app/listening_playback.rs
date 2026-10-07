use std::sync::OnceLock;
use std::time::Instant;

use super::*;
use crate::listening::{
    BookmarkId, BookmarkRecord, DjPreset, ListeningLoadReason, ListeningOperation,
    ListeningProjection, PendingSeekOutcome, PlaybackMemoryAction, ResumeClear, ResumePoint,
    ResumeProvenance, format_listening_projection, portable_track, radio_station_target,
    resolve_portable_track,
};
use crate::remote::proto::{ListeningRemoteAction, RemoteResponse};

fn monotonic_millis() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl App {
    pub(in crate::app) fn remote_listening(
        &mut self,
        action: ListeningRemoteAction,
    ) -> (RemoteResponse, Vec<Cmd>) {
        if matches!(action, ListeningRemoteAction::Enable) {
            let commands = self.enable_listening_records();
            return if self.listening_records_enabled() {
                (
                    RemoteResponse::ok("listening records enabled".to_owned()),
                    commands,
                )
            } else {
                (RemoteResponse::err("persistence_read_only"), commands)
            };
        }
        let projection = match ListeningProjection::from_ledger(&self.personal_state.ledger) {
            Ok(projection) => projection,
            Err(_) => return (RemoteResponse::err("listening_state_invalid"), Vec::new()),
        };
        if matches!(action, ListeningRemoteAction::List) {
            return (
                RemoteResponse::ok(format_listening_projection(&projection)),
                Vec::new(),
            );
        }
        if !self.listening_records_enabled() {
            return (
                RemoteResponse::err("listening_records_disabled"),
                Vec::new(),
            );
        }
        match action {
            ListeningRemoteAction::Enable | ListeningRemoteAction::List => unreachable!(),
            ListeningRemoteAction::BookmarkAdd { label } => {
                if self.personal_state.listening.current_track().is_none() {
                    return (RemoteResponse::err("nothing_playing"), Vec::new());
                }
                if self
                    .personal_state
                    .listening
                    .current_position_ms()
                    .is_none()
                {
                    return (RemoteResponse::err("position_unknown"), Vec::new());
                }
                if !self.personal_state.listening.current_can_seek() {
                    return (RemoteResponse::err("track_not_seekable"), Vec::new());
                }
                let commands = self.save_current_bookmark(label);
                if commands.is_empty() {
                    (RemoteResponse::err("listening_write_failed"), commands)
                } else {
                    (RemoteResponse::ok("bookmark queued".to_owned()), commands)
                }
            }
            ListeningRemoteAction::BookmarkDelete { bookmark_id } => {
                let Ok(bookmark_id) = BookmarkId::new(bookmark_id) else {
                    return (RemoteResponse::err("bad_bookmark_id"), Vec::new());
                };
                let commands = self
                    .commit_listening_change(ListeningOperation::DeleteBookmark { bookmark_id });
                if commands.is_empty() {
                    (RemoteResponse::err("listening_write_failed"), commands)
                } else {
                    (
                        RemoteResponse::ok("bookmark delete queued".to_owned()),
                        commands,
                    )
                }
            }
            ListeningRemoteAction::BookmarkJump { bookmark_id } => {
                let Ok(bookmark_id) = BookmarkId::new(bookmark_id) else {
                    return (RemoteResponse::err("bad_bookmark_id"), Vec::new());
                };
                let Some(bookmarks) = projection.bookmarks.get(&bookmark_id) else {
                    return (RemoteResponse::err("bookmark_not_found"), Vec::new());
                };
                let [bookmark] = bookmarks.as_slice() else {
                    return (RemoteResponse::err("bookmark_conflict"), Vec::new());
                };
                let current = self
                    .personal_state
                    .listening
                    .current_track()
                    .is_some_and(|track| track.key == bookmark.track.key);
                if current && !self.personal_state.listening.current_can_seek() {
                    return (RemoteResponse::err("track_not_seekable"), Vec::new());
                }
                if !current && self.resolve_listening_track(&bookmark.track).is_none() {
                    return (RemoteResponse::err("track_unavailable"), Vec::new());
                }
                let commands = self.jump_listening(bookmark.track.clone(), bookmark.position_ms);
                (
                    RemoteResponse::ok("bookmark jump requested".to_owned()),
                    commands,
                )
            }
            ListeningRemoteAction::Restart => {
                if self.personal_state.listening.current_track().is_none() {
                    return (RemoteResponse::err("nothing_playing"), Vec::new());
                }
                if !self.personal_state.listening.current_can_seek() {
                    return (RemoteResponse::err("track_not_seekable"), Vec::new());
                }
                (
                    RemoteResponse::ok("track restart requested".to_owned()),
                    self.restart_listening(),
                )
            }
            ListeningRemoteAction::PresetSave { name } => {
                let preset_id = crate::listening::DjPresetId::new(format!(
                    "preset-{:032x}",
                    fastrand::u128(..)
                ))
                .expect("generated preset id is valid");
                let snapshot = self.streaming.taste.snapshot();
                let commands = self.commit_listening_change(ListeningOperation::UpsertDjPreset {
                    preset: DjPreset {
                        preset_id,
                        name,
                        snapshot,
                    },
                });
                if commands.is_empty() {
                    (RemoteResponse::err("listening_write_failed"), commands)
                } else {
                    (RemoteResponse::ok("DJ preset queued".to_owned()), commands)
                }
            }
            ListeningRemoteAction::PresetLoad { preset_id } => {
                let Ok(preset_id) = crate::listening::DjPresetId::new(preset_id) else {
                    return (RemoteResponse::err("bad_preset_id"), Vec::new());
                };
                let Some(presets) = projection.dj_presets.get(&preset_id) else {
                    return (RemoteResponse::err("preset_not_found"), Vec::new());
                };
                let [preset] = presets.as_slice() else {
                    return (RemoteResponse::err("preset_conflict"), Vec::new());
                };
                let name = preset.name.clone();
                let commands = self.apply_dj_preset(preset.clone());
                (
                    RemoteResponse::ok(format!("DJ preset loaded: {name}")),
                    commands,
                )
            }
            ListeningRemoteAction::PresetDelete { preset_id } => {
                let Ok(preset_id) = crate::listening::DjPresetId::new(preset_id) else {
                    return (RemoteResponse::err("bad_preset_id"), Vec::new());
                };
                let commands =
                    self.commit_listening_change(ListeningOperation::DeleteDjPreset { preset_id });
                if commands.is_empty() {
                    (RemoteResponse::err("listening_write_failed"), commands)
                } else {
                    (
                        RemoteResponse::ok("DJ preset delete queued".to_owned()),
                        commands,
                    )
                }
            }
        }
    }

    pub fn listening_records_enabled(&self) -> bool {
        self.config.effective_listening_records_enabled()
    }

    pub(in crate::app) fn enable_listening_records(&mut self) -> Vec<Cmd> {
        if self.listening_records_enabled() {
            return Vec::new();
        }
        if let Err(error) = crate::persist::ensure_persistence_writes_allowed() {
            self.listening_error(error.to_string());
            return Vec::new();
        }
        if self.config.listening_local_scope.is_none() {
            self.config.listening_local_scope =
                Some(format!("local-scope-{:032x}", fastrand::u128(..)));
        }
        if let Some(song) = self.queue.current() {
            self.personal_state
                .listening
                .rebind_current_track(portable_track(
                    song,
                    self.config.listening_local_scope.as_deref(),
                ));
        }
        self.config.listening_records_enabled = Some(true);
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    pub fn listening_resume_enabled(&self) -> bool {
        self.config.effective_listening_resume()
    }

    pub(in crate::app) fn toggle_listening_resume(&mut self) -> Vec<Cmd> {
        if let Err(error) = crate::persist::ensure_persistence_writes_allowed() {
            self.listening_error(error.to_string());
            return Vec::new();
        }
        let enabled = !self.listening_resume_enabled();
        self.config.listening_resume = Some(enabled);
        self.set_status_info(if enabled {
            t!(
                "Automatic resume enabled",
                "자동 이어듣기를 켰어요",
                "自動再開をオンにしました"
            )
        } else {
            t!(
                "Automatic resume disabled",
                "자동 이어듣기를 껐어요",
                "自動再開をオフにしました"
            )
        });
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    pub(in crate::app) fn save_current_bookmark(&mut self, label: String) -> Vec<Cmd> {
        if !self.listening_records_enabled() {
            self.listening_error(
                t!(
                    "Enable listening records first",
                    "먼저 청취 기록을 켜 주세요",
                    "先にリスニング記録を有効にしてください"
                )
                .to_owned(),
            );
            return Vec::new();
        }
        let Some(track) = self.personal_state.listening.current_track().cloned() else {
            self.listening_error(
                t!(
                    "Nothing is playing",
                    "재생 중인 곡이 없어요",
                    "再生中の曲がありません"
                )
                .to_owned(),
            );
            return Vec::new();
        };
        let Some(position_ms) = self.personal_state.listening.current_position_ms() else {
            self.listening_error(
                t!(
                    "The current position is not confirmed yet",
                    "현재 위치가 아직 확인되지 않았어요",
                    "現在位置がまだ確認されていません"
                )
                .to_owned(),
            );
            return Vec::new();
        };
        if !self.personal_state.listening.current_can_seek() {
            self.listening_error(
                t!(
                    "Bookmarks require a seekable track",
                    "북마크는 탐색 가능한 곡에서만 저장할 수 있어요",
                    "ブックマークはシーク可能な曲で保存できます"
                )
                .to_owned(),
            );
            return Vec::new();
        }
        let bookmark_id = match BookmarkId::new(format!("bookmark-{:032x}", fastrand::u128(..))) {
            Ok(id) => id,
            Err(error) => {
                self.listening_error(error.to_string());
                return Vec::new();
            }
        };
        self.commit_listening_change(ListeningOperation::UpsertBookmark {
            bookmark: BookmarkRecord {
                bookmark_id,
                track,
                position_ms,
                label,
            },
        })
    }

    pub(in crate::app) fn jump_listening(
        &mut self,
        track: crate::personal_state::PortableTrack,
        position_ms: u64,
    ) -> Vec<Cmd> {
        let key = track.key.clone();
        self.personal_state.listening.request_seek(
            key.clone(),
            position_ms,
            ListeningLoadReason::Bookmark,
        );
        if self
            .personal_state
            .listening
            .current_track()
            .is_some_and(|current| current.key == key)
        {
            return self.maybe_admit_listening_seek();
        }
        let Some(song) = self.resolve_listening_track(&track) else {
            self.personal_state.listening.cancel_pending_seek();
            self.listening_error(
                t!(
                    "This track is not available on this device",
                    "이 기기에서 이 곡을 찾을 수 없어요",
                    "このデバイスでは曲を利用できません"
                )
                .to_owned(),
            );
            return Vec::new();
        };
        self.overlays.listening = None;
        let commands = self.play_now(song);
        if commands.is_empty() {
            self.personal_state.listening.cancel_pending_seek();
        }
        commands
    }

    pub(in crate::app) fn restart_listening(&mut self) -> Vec<Cmd> {
        let Some(track) = self.personal_state.listening.current_track().cloned() else {
            return Vec::new();
        };
        self.personal_state
            .listening
            .request_seek(track.key, 0, ListeningLoadReason::Restart);
        self.maybe_admit_listening_seek()
    }

    pub(in crate::app) fn clear_listening_resume(
        &mut self,
        track: crate::personal_state::PortableTrack,
    ) -> Vec<Cmd> {
        if !self.listening_records_enabled() {
            return Vec::new();
        }
        let Some(provenance) = self.listening_provenance() else {
            return Vec::new();
        };
        self.commit_listening_change(ListeningOperation::ClearResume {
            clear: ResumeClear { track, provenance },
        })
    }

    pub(in crate::app) fn apply_dj_preset(&mut self, preset: DjPreset) -> Vec<Cmd> {
        if let Err(error) = self.streaming.taste.replace_snapshot(preset.snapshot) {
            self.listening_error(format!("{error:?}"));
            return Vec::new();
        }
        self.cancel_pending_streaming_recommendation();
        self.personal_state
            .listening
            .set_active_preset_name(Some(preset.name.clone()));
        self.overlays.listening = None;
        self.set_status_info(format!(
            "{}: {}",
            t!(
                "DJ preset loaded",
                "DJ 프리셋 불러옴",
                "DJプリセットを読み込みました"
            ),
            preset.name
        ));
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn begin_listening_track(
        &mut self,
        song: &Song,
        reason: ListeningLoadReason,
    ) {
        let track = portable_track(song, self.config.listening_local_scope.as_deref());
        self.personal_state.listening.begin_track(
            track.clone(),
            song.is_radio_station(),
            radio_station_target(song),
        );
        let projection = (self.listening_records_enabled()
            && self.config.effective_listening_resume()
            && reason.permits_automatic_resume()
            && !self.personal_state.listening.has_pending_seek())
        .then(|| ListeningProjection::from_ledger(&self.personal_state.ledger).ok())
        .flatten();
        let conflicted = projection
            .as_ref()
            .and_then(|projection| projection.resumes.get(&track.key))
            .is_some_and(crate::listening::ResumeState::is_conflicted);
        let resume = projection
            .as_ref()
            .and_then(|projection| projection.automatic_resume(&track.key).cloned());
        if conflicted && reason == ListeningLoadReason::Deliberate {
            self.set_status_info(t!(
                "Multiple resume points: open Bookmarks to choose",
                "여러 재생 지점이 있습니다: 북마크에서 선택하세요",
                "複数の再生位置があります：ブックマークから選択してください"
            ));
        }
        if let Some(point) = resume {
            self.personal_state
                .listening
                .request_seek(track.key, point.position_ms, reason);
        }
    }

    pub(in crate::app) fn observe_listening_position(&mut self, seconds: f64) -> Vec<Cmd> {
        if !self.listening_records_enabled() {
            self.personal_state
                .listening
                .observe_position_only(seconds, monotonic_millis());
            return Vec::new();
        }
        let actions = self.personal_state.listening.observe_position_secs(
            seconds,
            monotonic_millis(),
            self.config.effective_listening_resume(),
        );
        self.commit_playback_memory_actions(actions)
    }

    pub(in crate::app) fn observe_listening_duration(&mut self, duration: Option<f64>) -> Vec<Cmd> {
        self.personal_state
            .listening
            .observe_duration_secs(duration);
        self.maybe_admit_listening_seek()
    }

    pub(in crate::app) fn observe_listening_seekable(
        &mut self,
        seekable: Option<bool>,
    ) -> Vec<Cmd> {
        self.personal_state.listening.observe_seekable(seekable);
        self.maybe_admit_listening_seek()
    }

    pub(in crate::app) fn observe_listening_buffering(&mut self, buffering: bool) {
        self.personal_state.listening.observe_buffering(buffering);
    }

    pub(in crate::app) fn observe_listening_paused(&mut self, paused: bool) -> Vec<Cmd> {
        if !self.listening_records_enabled() {
            self.personal_state.listening.observe_paused(paused, false);
            return Vec::new();
        }
        let action = self
            .personal_state
            .listening
            .observe_paused(paused, self.config.effective_listening_resume());
        self.commit_playback_memory_actions(action)
    }

    pub(in crate::app) fn complete_listening_track(&mut self) -> Vec<Cmd> {
        let action = self.personal_state.listening.complete_track();
        if !self.listening_records_enabled() {
            return Vec::new();
        }
        self.commit_playback_memory_actions(action)
    }

    pub(in crate::app) fn snapshot_listening_progress(&mut self) -> Vec<Cmd> {
        if !self.listening_records_enabled() {
            return Vec::new();
        }
        let action = self
            .personal_state
            .listening
            .snapshot_resume(self.config.effective_listening_resume());
        self.commit_playback_memory_actions(action)
    }

    pub(in crate::app) fn clear_listening_track(&mut self) {
        self.personal_state.listening.clear_track();
    }

    fn maybe_admit_listening_seek(&mut self) -> Vec<Cmd> {
        let Some(outcome) = self.personal_state.listening.pending_seek_outcome() else {
            return Vec::new();
        };
        let pending = match outcome {
            PendingSeekOutcome::Ready(pending) => pending,
            PendingSeekOutcome::Invalid(pending) => {
                self.personal_state.listening.cancel_pending_seek();
                self.set_status_error(t!(
                    "Saved position is outside this track",
                    "저장된 위치가 이 음원의 범위를 벗어났어요",
                    "保存位置がこの音源の範囲外です"
                ));
                if pending.reason.permits_automatic_resume()
                    && let Some(track) = self.personal_state.listening.current_track().cloned()
                {
                    return self.clear_listening_resume(track);
                }
                return Vec::new();
            }
        };
        let track = self
            .personal_state
            .listening
            .current_track()
            .cloned()
            .expect("ready listening seek has a current track");
        let change = if !self.listening_records_enabled() {
            None
        } else if pending.reason == ListeningLoadReason::Restart || pending.position_ms == 0 {
            self.listening_provenance()
                .map(|provenance| ListeningOperation::ClearResume {
                    clear: ResumeClear { track, provenance },
                })
        } else if self.config.effective_listening_resume()
            && self
                .personal_state
                .listening
                .resume_position_eligible(pending.position_ms)
        {
            self.listening_provenance()
                .map(|provenance| ListeningOperation::SetResume {
                    point: ResumePoint {
                        track,
                        position_ms: pending.position_ms,
                        provenance,
                    },
                })
        } else {
            None
        };
        self.player_intent(
            "listening_seek",
            PlayerCmd::exact_seek(pending.position_ms as f64 / 1_000.0),
            PlayerCommit::ListeningSeek {
                position_ms: pending.position_ms,
                change: change.map(Box::new),
            },
        )
    }

    fn listening_provenance(&mut self) -> Option<ResumeProvenance> {
        match crate::personal_state::listening_device_id(
            &self.personal_state.ledger,
            self.personal_state.device_id.as_ref(),
        ) {
            Ok(device_id) => Some(ResumeProvenance {
                playback_session_id: self
                    .personal_state
                    .listening
                    .playback_session_id()
                    .to_owned(),
                device_id,
            }),
            Err(error) => {
                self.listening_error(error.to_string());
                None
            }
        }
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
            .or_else(|| {
                self.download_store
                    .tracks()
                    .iter()
                    .find(|candidate| {
                        portable_track(candidate, self.config.listening_local_scope.as_deref()).key
                            == track.key
                    })
                    .cloned()
            })
            .or_else(|| {
                self.local_mode
                    .index
                    .index
                    .tracks
                    .iter()
                    .find_map(|candidate| {
                        let song = candidate.to_song();
                        (portable_track(&song, self.config.listening_local_scope.as_deref()).key
                            == track.key)
                            .then_some(song)
                    })
            })
            .or_else(|| resolve_portable_track(track, self.config.listening_local_scope.as_deref()))
    }

    fn commit_playback_memory_actions(
        &mut self,
        actions: impl IntoIterator<Item = PlaybackMemoryAction>,
    ) -> Vec<Cmd> {
        let mut commands = Vec::new();
        for action in actions {
            let change = match action {
                PlaybackMemoryAction::SaveResume { track, position_ms } => {
                    let Some(provenance) = self.listening_provenance() else {
                        continue;
                    };
                    ListeningOperation::SetResume {
                        point: ResumePoint {
                            track,
                            position_ms,
                            provenance,
                        },
                    }
                }
                PlaybackMemoryAction::ClearResume { track } => {
                    let Some(provenance) = self.listening_provenance() else {
                        continue;
                    };
                    ListeningOperation::ClearResume {
                        clear: ResumeClear { track, provenance },
                    }
                }
                PlaybackMemoryAction::RecordPassportVisit(station) => {
                    let now = crate::signals::unix_now();
                    let first = ListeningProjection::from_ledger(&self.personal_state.ledger)
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
            };
            commands.extend(self.commit_listening_change(change));
        }
        commands
    }

    #[cfg(test)]
    pub(crate) fn seed_listening_for_test(
        &mut self,
        song: &Song,
        position_secs: f64,
        duration_secs: f64,
    ) {
        self.begin_listening_track(song, ListeningLoadReason::Deliberate);
        self.personal_state
            .listening
            .observe_duration_secs(Some(duration_secs));
        self.personal_state.listening.observe_seekable(Some(true));
        let _ = self.observe_listening_position(position_secs);
    }

    #[cfg(test)]
    pub(crate) fn reopen_listening_for_test(&mut self, song: &Song, duration_secs: f64) {
        self.begin_listening_track(song, ListeningLoadReason::Deliberate);
        let _ = self.observe_listening_duration(Some(duration_secs));
        let _ = self.observe_listening_seekable(Some(true));
    }

    #[cfg(test)]
    pub(crate) fn stale_pending_allows_resume_for_test(
        &mut self,
        stale_song: &Song,
        target_song: &Song,
    ) -> bool {
        let target = portable_track(target_song, self.config.listening_local_scope.as_deref());
        let Some(provenance) = self.listening_provenance() else {
            return false;
        };
        let Ok(state) = crate::personal_state::append_listening(
            &self.personal_state.ledger,
            self.personal_state.device_id.as_ref(),
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
        self.personal_state.replace_ledger(state);

        let stale = portable_track(stale_song, self.config.listening_local_scope.as_deref());
        self.personal_state
            .listening
            .begin_track(stale.clone(), false, None);
        self.personal_state.listening.request_seek(
            stale.key,
            45_000,
            ListeningLoadReason::Bookmark,
        );
        self.begin_listening_track(target_song, ListeningLoadReason::Deliberate);
        self.personal_state
            .listening
            .observe_duration_secs(Some(1_800.0));
        self.personal_state.listening.observe_seekable(Some(true));
        matches!(
            self.personal_state.listening.pending_seek_outcome(),
            Some(PendingSeekOutcome::Ready(pending))
                if pending.track_key == target.key && pending.position_ms == 120_000
        )
    }
}
