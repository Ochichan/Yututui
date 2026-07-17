//! Recovery from an unexpected loss of the main mpv IPC transport.

use super::*;

impl App {
    /// A replacement cache reset failed after the owner committed the new destination. Recover
    /// from that owner projection, not from the actor's old-file snapshot, so an old position can
    /// never be resumed into a newly selected item.
    pub(in crate::app) fn recover_cache_replacement_emergency(
        &mut self,
        reason: crate::player::long_form_seek::CacheReason,
    ) -> Vec<Cmd> {
        let admitted_media_is_active = self
            .prefetch
            .loaded_video_id
            .as_deref()
            .zip(self.queue.current())
            .is_some_and(|(loaded, song)| loaded == song.video_id);
        if !admitted_media_is_active {
            self.supersede_source_recovery();
            self.status.kind = StatusKind::Error;
            self.status.text = format!(
                "{} ({})",
                t!(
                    "Disk cache safety limit reached; player stopped safely",
                    "디스크 캐시 안전 한도에 도달해 플레이어를 안전하게 중지했습니다",
                    "ディスクキャッシュの安全上限に達したためプレイヤーを安全に停止しました"
                ),
                reason.id()
            );
            self.dirty = true;
            return vec![Cmd::PlayerControl(PlayerControl::Restart {
                restore: Vec::new(),
            })];
        }
        self.recover_cache_emergency(
            self.playback.time_pos.unwrap_or(0.0),
            self.playback.paused,
            reason,
        )
    }

    /// Retire a cache-unsafe mpv and resume the same item only after the replacement actor's
    /// correlated file-loaded boundary. The persisted/requested policy is unchanged; the player
    /// command marks just this replacement media as RAM-only.
    pub(in crate::app) fn recover_cache_emergency(
        &mut self,
        position_secs: f64,
        _paused: bool,
        reason: crate::player::long_form_seek::CacheReason,
    ) -> Vec<Cmd> {
        self.supersede_source_recovery();
        self.recorder_player_transport_recovery_started();
        // The actor snapshot can be older than an already-admitted owner intent: cache safety
        // actions have priority over the actor command backlog, and their terminal event can
        // also wait behind a UI/remote message. This reducer is reached only after the runtime
        // proved that the emergency still belongs to the current file generation, so the
        // owner's admitted transport projection wins. The actor position remains a fallback for
        // the narrow startup window in which the owner has not observed any position yet.
        let position_secs = self
            .playback
            .time_pos
            .map(crate::playback_policy::norm_position)
            .unwrap_or_else(|| crate::playback_policy::norm_position(position_secs));
        let paused = self.playback.paused;

        self.playback.time_pos = Some(position_secs);
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::TransportRecovery);
        self.playback.duration = None;
        self.playback.paused = paused;
        self.playback.stream_now_playing = None;
        self.playback.cache_time = None;
        self.playback.cache_time_at = None;
        self.playback.audio_codec = None;
        self.playback.file_format = None;
        self.anim.last_shown_sec = -1;
        self.anim.last_shown_cache_sec = -1;
        self.radio_resync_at = None;
        self.dirty = true;
        self.status.kind = StatusKind::Error;
        self.status.text = format!(
            "{} ({})",
            t!(
                "Disk cache safety limit reached; resuming in memory",
                "디스크 캐시 안전 한도에 도달해 메모리 모드로 재개합니다",
                "ディスクキャッシュの安全上限に達したためメモリモードで再開します"
            ),
            reason.id()
        );

        let mut cmds = self.recorder_teardown();
        let restore = Vec::new();
        let Some(song) = self.queue.current().cloned() else {
            cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
            return cmds;
        };
        let target = match song.playback_target_checked() {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(
                    video_id = %song.video_id,
                    %error,
                    "could not resume current track after cache safety recycle"
                );
                cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
                return cmds;
            }
        };

        let af = match self.settings.as_deref() {
            Some(st) => eq::build_af_string(&st.draft.eq_bands, st.draft.normalize),
            None => self.current_af(),
        };
        let restore = crate::player::recovery::TransportRestorePlan::resume_ram_only_if_loaded(
            self.prefetch.loaded_video_id.as_deref(),
            &song.video_id,
            target,
            position_secs,
            paused,
            crate::player::MediaSourceContext::from_live(song.is_radio_station()),
        )
        .into_commands(af);
        self.prefetch.last_load_prefetched = false;
        cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
        cmds
    }

    /// Reset transport-derived state and ask the runtime for one fresh mpv actor. The current
    /// queue item is loaded directly, deliberately bypassing `load_song`: reconnecting must not
    /// duplicate library history, session activity, signals, artwork, lyrics, or prefetch work.
    pub(in crate::app) fn recover_player_transport(&mut self, reason: String) -> Vec<Cmd> {
        self.supersede_source_recovery();
        self.recorder_player_transport_recovery_started();
        let reason = crate::util::sanitize::sanitize_error_text(reason);
        let was_paused = self.playback.paused;

        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::TransportRecovery);
        self.playback.duration = None;
        self.playback.paused = was_paused;
        self.playback.stream_now_playing = None;
        self.playback.cache_time = None;
        self.playback.cache_time_at = None;
        self.playback.audio_codec = None;
        self.playback.file_format = None;
        self.anim.last_shown_sec = -1;
        self.anim.last_shown_cache_sec = -1;
        self.radio_resync_at = None;
        self.dirty = true;
        self.status.kind = StatusKind::Error;
        self.status.text = format!(
            "{}: {reason}",
            t!(
                "Player disconnected; restarting",
                "플레이어 연결이 끊어져 다시 시작합니다",
                "プレイヤーの接続が切れたため再起動します"
            )
        );

        let mut cmds = self.recorder_teardown();
        let restore = Vec::new();
        let Some(song) = self.queue.current().cloned() else {
            cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
            return cmds;
        };
        let target = match song.playback_target_checked() {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(
                    video_id = %song.video_id,
                    %error,
                    "could not reload current track after player transport failure"
                );
                self.status.text = format!(
                    "{}: {}",
                    t!(
                        "Player restarted, but the current track cannot be reloaded",
                        "플레이어를 다시 시작했지만 현재 곡을 불러올 수 없습니다",
                        "プレイヤーを再起動しましたが現在の曲を読み込めません"
                    ),
                    crate::util::sanitize::sanitize_error_text(error.to_string())
                );
                cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
                return cmds;
            }
        };

        let af = match self.settings.as_deref() {
            Some(st) => eq::build_af_string(&st.draft.eq_bands, st.draft.normalize),
            None => self.current_af(),
        };
        let restore = crate::player::recovery::TransportRestorePlan::reload_if_loaded(
            self.prefetch.loaded_video_id.as_deref(),
            &song.video_id,
            target,
            was_paused,
            crate::player::MediaSourceContext::from_live(song.is_radio_station()),
        )
        .into_commands(af);
        self.prefetch.last_load_prefetched = false;
        cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
        cmds
    }
}
