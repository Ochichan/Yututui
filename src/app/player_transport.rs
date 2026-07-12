//! Recovery from an unexpected loss of the main mpv IPC transport.

use super::*;

impl App {
    /// Reset transport-derived state and ask the runtime for one fresh mpv actor. The current
    /// queue item is loaded directly, deliberately bypassing `load_song`: reconnecting must not
    /// duplicate library history, session activity, signals, artwork, lyrics, or prefetch work.
    pub(in crate::app) fn recover_player_transport(&mut self, reason: String) -> Vec<Cmd> {
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
                "플레이어 연결이 끊어져 다시 시작합니다"
            )
        );

        let mut cmds = self.recorder_teardown();
        let mut restore = Vec::new();
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
                        "플레이어를 다시 시작했지만 현재 곡을 불러올 수 없습니다"
                    ),
                    crate::util::sanitize::sanitize_error_text(error.to_string())
                );
                cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
                return cmds;
            }
        };

        self.prefetch.last_load_prefetched = false;
        restore.push(PlayerCmd::Load(target));
        let af = match self.settings.as_deref() {
            Some(st) => eq::build_af_string(&st.draft.eq_bands, st.draft.normalize),
            None => self.current_af(),
        };
        if let Some(af) = af {
            restore.push(PlayerCmd::SetAudioFilter(af));
        }
        if was_paused {
            restore.push(PlayerCmd::CyclePause);
        }
        cmds.push(Cmd::PlayerControl(PlayerControl::Restart { restore }));
        cmds
    }
}
