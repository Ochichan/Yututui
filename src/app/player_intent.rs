//! Two-phase player admission for user-visible transport state.
//!
//! Reducers describe the mpv command and the state it would project, but do not mutate that
//! state. The runtime admits the complete command batch first, then feeds [`PlayerCommit`] back
//! through `App::update`. A busy or closed lane therefore leaves pause, volume, position, speed,
//! and `position_epoch` unchanged and can return a truthful remote-control error.

use super::*;

/// Runtime-owned player controls which must not be forwarded blindly to the mpv actor.
pub enum PlayerControl {
    Intent(Box<PlayerIntent>),
    /// Tear down the dead audio-player lifetime and start at most one replacement actor.
    /// The ordered restore batch is admitted atomically to the fresh actor's bounded pre-ready
    /// backlog, so no prefix can become visible when the complete restore does not fit.
    Restart {
        restore: Vec<PlayerCmd>,
    },
}

/// A command batch plus the reducer commit that becomes valid once the batch is admitted.
pub struct PlayerIntent {
    pub(crate) commands: Vec<PlayerCmd>,
    pub(crate) commit: PlayerCommit,
    pub(crate) label: &'static str,
    pub(crate) remote_reply: Option<PendingRemoteReply>,
}

impl PlayerIntent {
    pub(in crate::app) fn one(
        label: &'static str,
        command: PlayerCmd,
        commit: PlayerCommit,
    ) -> Self {
        Self {
            commands: vec![command],
            commit,
            label,
            remote_reply: None,
        }
    }

    pub(in crate::app) fn batch(
        label: &'static str,
        commands: Vec<PlayerCmd>,
        commit: PlayerCommit,
    ) -> Self {
        debug_assert!(
            !commands.is_empty(),
            "player intent batch must not be empty"
        );
        Self {
            commands,
            commit,
            label,
            remote_reply: None,
        }
    }

    pub(in crate::app) fn with_remote_reply(
        &mut self,
        sender: crate::remote::RemoteReply,
        response: RemoteReplyPlan,
    ) {
        assert!(
            self.remote_reply.is_none(),
            "a player intent may carry only one correlated remote reply"
        );
        self.remote_reply = Some(PendingRemoteReply { sender, response });
    }
}

/// State changes whose validity depends on player command admission.
#[derive(Clone)]
pub enum PlayerCommit {
    AudioOutputRefresh,
    AudioOutputSelection {
        correlation_id: u64,
        target: Option<String>,
        source: AudioOutputSelectionSource,
    },
    Pause {
        paused: bool,
        clear_video_pause: bool,
    },
    Volume {
        volume: i64,
        pre_mute_volume: Option<i64>,
    },
    Seek {
        optimistic_position: Option<f64>,
    },
    Speed {
        speed: f64,
        announce: bool,
        persist: bool,
    },
    EqPreset {
        preset: EqPreset,
        close_streaming_dropdown: bool,
    },
    Normalize {
        normalize: bool,
        persist: bool,
    },
    SettingsAudioPreview(Box<super::settings_audio::SettingsAudioPreviewPlan>),
    SettingsSave(Box<super::settings_audio::SettingsSavePlan>),
    PrefetchWatchRetry(Box<super::player_recovery::PrefetchWatchRetryPlan>),
    RadioLiveSeek(Box<super::player_recovery::RadioLiveSeekPlan>),
    Recorder(Box<crate::recorder::RecorderTransitionPlan>),
    Stop(Box<super::media_reducer::PlaybackStopPlan>),
    Track(Box<TrackTransitionPlan>),
    VideoOpen(Box<VideoOpenPlan>),
    VideoFinish(Box<VideoFinishPlan>),
}

impl PlayerCommit {
    pub(in crate::app) fn is_seek(&self) -> bool {
        matches!(self, Self::Seek { .. } | Self::RadioLiveSeek(_))
    }

    /// Admission preflight for plans whose commands and reducer projection were prepared from
    /// an exact owner-state snapshot. Simple absolute controls remain current by construction;
    /// every stateful plan validates the same guards as its commit before mpv sees the batch.
    pub(crate) fn is_current_for(&self, app: &App) -> bool {
        match self {
            Self::AudioOutputSelection { correlation_id, .. } => {
                app.audio_output_selection_is_current(*correlation_id)
            }
            Self::SettingsAudioPreview(plan) => app.settings_audio_preview_is_current(plan),
            Self::SettingsSave(plan) => app.settings_save_is_current(plan),
            Self::PrefetchWatchRetry(plan) => app.prefetch_watch_retry_is_current(plan),
            Self::RadioLiveSeek(plan) => app.radio_live_seek_is_current(plan),
            Self::Recorder(plan) => app.recorder_transition_is_current(plan),
            Self::Stop(plan) => app.media_stop_is_current(plan),
            Self::Track(plan) => app.track_transition_is_current(plan),
            Self::VideoOpen(plan) => app.video_open_is_current(plan),
            Self::VideoFinish(plan) => app.video_finish_is_current(plan),
            Self::AudioOutputRefresh
            | Self::Pause { .. }
            | Self::Volume { .. }
            | Self::Seek { .. }
            | Self::Speed { .. }
            | Self::EqPreset { .. }
            | Self::Normalize { .. } => true,
        }
    }
}

/// A remote response evaluated after the corresponding commit, so its snapshot cannot claim
/// state which failed admission. `Fixed` is useful when the payload is playback-independent.
pub enum RemoteReplyPlan {
    Fixed(Box<crate::remote::proto::RemoteResponse>),
    Pause,
    Volume,
    Status,
    Transport,
    NowPlaying,
}

pub struct PendingRemoteReply {
    pub(crate) sender: crate::remote::RemoteReply,
    pub(crate) response: RemoteReplyPlan,
}

impl App {
    /// Change playback speed by `delta`, but project the new value only after admission.
    pub(in crate::app) fn adjust_speed(&mut self, delta: f64) -> Vec<Cmd> {
        let speed =
            (((self.playback.speed + delta) * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX);
        self.player_intent(
            "set_speed",
            PlayerCmd::SetProperty {
                name: "speed".to_owned(),
                value: serde_json::Value::from(speed),
            },
            PlayerCommit::Speed {
                speed,
                announce: true,
                persist: false,
            },
        )
    }

    /// Apply an EQ preset only after the complete filter update enters the player lane.
    pub(in crate::app) fn eq_preset_intent(
        &self,
        preset: EqPreset,
        close_streaming_dropdown: bool,
    ) -> Vec<Cmd> {
        let bands = preset.gains();
        self.player_intent(
            "set_audio_filter",
            PlayerCmd::SetAudioFilter(
                eq::build_af_string(&bands, self.audio.normalize).unwrap_or_default(),
            ),
            PlayerCommit::EqPreset {
                preset,
                close_streaming_dropdown,
            },
        )
    }

    /// Apply normalization only after the prospective filter chain enters the player lane.
    /// Remote setting writes additionally persist the accepted value; ordinary controls remain
    /// session-scoped.
    pub(in crate::app) fn normalize_intent(&self, normalize: bool, persist: bool) -> Vec<Cmd> {
        self.player_intent(
            "set_audio_filter",
            PlayerCmd::SetAudioFilter(
                eq::build_af_string(&self.audio.bands, normalize).unwrap_or_default(),
            ),
            PlayerCommit::Normalize { normalize, persist },
        )
    }

    pub(in crate::app) fn player_intent(
        &self,
        label: &'static str,
        command: PlayerCmd,
        commit: PlayerCommit,
    ) -> Vec<Cmd> {
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::one(label, command, commit),
        )))]
    }

    pub(crate) fn commit_player_intent(&mut self, commit: PlayerCommit) -> Vec<Cmd> {
        match commit {
            PlayerCommit::AudioOutputRefresh => self.commit_audio_output_refresh(),
            PlayerCommit::AudioOutputSelection {
                correlation_id,
                target,
                source,
            } => {
                self.commit_audio_output_selection(correlation_id, target, source);
            }
            PlayerCommit::Pause {
                paused,
                clear_video_pause,
            } => {
                self.playback.paused = paused;
                if clear_video_pause {
                    self.video.paused_audio = false;
                }
            }
            PlayerCommit::Volume {
                volume,
                pre_mute_volume,
            } => {
                self.playback.volume = volume;
                self.playback.pre_mute_volume = pre_mute_volume;
            }
            PlayerCommit::Seek {
                optimistic_position,
            } => {
                if let Some(position) = optimistic_position {
                    self.playback.time_pos = Some(position);
                    self.playback.time_pos_at = Some(Instant::now());
                }
            }
            PlayerCommit::Speed {
                speed,
                announce,
                persist,
            } => {
                self.playback.speed = speed;
                if persist {
                    self.config.speed = Some(speed);
                }
                if announce {
                    self.status.kind = StatusKind::Info;
                    self.status.text = format!("{}: {speed:.1}x", t!("Speed", "재생 속도"));
                }
                if persist {
                    self.dirty = true;
                    return vec![Cmd::Persist(PersistCmd::Config(Box::new(
                        self.config.clone(),
                    )))];
                }
            }
            PlayerCommit::EqPreset {
                preset,
                close_streaming_dropdown,
            } => {
                self.audio.preset = preset;
                self.audio.bands = preset.gains();
                self.dropdowns.eq_open = false;
                if close_streaming_dropdown {
                    self.dropdowns.streaming_open = false;
                }
                self.dropdowns.search_source_open = false;
                self.status.text = format!("EQ: {}", preset.label());
            }
            PlayerCommit::Normalize { normalize, persist } => {
                self.audio.normalize = normalize;
                if persist {
                    self.config.normalize = Some(normalize);
                }
                self.status.text = format!(
                    "{}: {}",
                    if persist {
                        t!("Normalize", "노멀라이즈")
                    } else {
                        t!("Normalize", "음량 평준화")
                    },
                    if normalize { "✓" } else { "✗" }
                );
                if persist {
                    self.dirty = true;
                    return vec![Cmd::Persist(PersistCmd::Config(Box::new(
                        self.config.clone(),
                    )))];
                }
            }
            PlayerCommit::SettingsAudioPreview(plan) => {
                return self.commit_settings_audio_preview(*plan);
            }
            PlayerCommit::SettingsSave(plan) => return self.commit_settings_save_plan(*plan),
            PlayerCommit::PrefetchWatchRetry(plan) => {
                return self.commit_prefetch_watch_retry(*plan);
            }
            PlayerCommit::RadioLiveSeek(plan) => return self.commit_radio_live_seek(*plan),
            PlayerCommit::Recorder(plan) => return self.commit_recorder_transition(*plan),
            PlayerCommit::Stop(plan) => return self.commit_media_stop(*plan),
            PlayerCommit::Track(plan) => return self.commit_track_transition(*plan),
            PlayerCommit::VideoOpen(plan) => return self.commit_video_open(*plan),
            PlayerCommit::VideoFinish(plan) => return self.commit_video_finish(*plan),
        }
        self.dirty = true;
        Vec::new()
    }

    /// Attach a remote reply to the admission-sensitive intent returned by a shared reducer
    /// path. Returns the sender when the command produced no intent, allowing an immediate
    /// reply for read-only/non-player operations.
    pub(in crate::app) fn attach_remote_reply(
        cmds: &mut [Cmd],
        sender: crate::remote::RemoteReply,
        response: RemoteReplyPlan,
    ) -> Result<(), (crate::remote::RemoteReply, RemoteReplyPlan)> {
        let Some(intent) = cmds.iter_mut().find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(intent),
            _ => None,
        }) else {
            return Err((sender, response));
        };
        intent.with_remote_reply(sender, response);
        Ok(())
    }

    /// Reducer tests do not construct `RuntimeHandles`; explicitly simulate successful
    /// admission so their state assertions exercise the same commit message as production.
    #[cfg(test)]
    pub(crate) fn admit_player_intents_for_test(&mut self, cmds: &[Cmd]) {
        let follow_ups = self.admit_player_intents_with_followups_for_test(cmds);
        assert!(
            follow_ups.is_empty(),
            "simple player intent commit unexpectedly emitted follow-up effects"
        );
    }

    #[cfg(test)]
    pub(crate) fn admit_player_intents_with_followups_for_test(
        &mut self,
        cmds: &[Cmd],
    ) -> Vec<Cmd> {
        self.admit_player_intents_with_recorder_replies_for_test(cmds, true, true)
    }

    #[cfg(test)]
    pub(crate) fn admit_player_intents_with_recorder_replies_for_test(
        &mut self,
        cmds: &[Cmd],
        close_ok: bool,
        open_ok: bool,
    ) -> Vec<Cmd> {
        for command in cmds.iter().flat_map(Cmd::player_commands) {
            if let PlayerCmd::TrackedProperty(tracked) = command {
                let succeeds = if tracked.value.as_str() == Some("") {
                    close_ok
                } else {
                    open_ok
                };
                if succeeds {
                    tracked.acknowledgement.succeed();
                } else {
                    tracked.acknowledgement.fail("injected mpv rejection");
                }
            }
        }
        let commits: Vec<PlayerCommit> = cmds
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(intent.commit.clone()),
                _ => None,
            })
            .collect();
        let mut pending_follow_ups = std::collections::VecDeque::new();
        for commit in commits {
            pending_follow_ups.extend(self.update(PlayerMsg::IntentAdmitted(commit)));
        }
        let mut all_follow_ups = Vec::new();
        while let Some(follow_up) = pending_follow_ups.pop_front() {
            match follow_up {
                Cmd::Recorder(job @ crate::recorder::job::RecorderJob::AwaitTransition { .. }) => {
                    let event = crate::recorder::job::run(job)
                        .expect("recorder transition wait produces a correlated event");
                    pending_follow_ups.extend(self.update(Msg::Recorder(event)));
                }
                other => all_follow_ups.push(other),
            }
        }
        all_follow_ups
    }
}

impl Cmd {
    /// Every underlying actor command in dispatch order. Track transitions intentionally carry
    /// recorder-clear → Load/Stop → AF in one batch, so boundary tests must not inspect only
    /// the first element.
    #[cfg(test)]
    pub(crate) fn player_commands(&self) -> impl Iterator<Item = &PlayerCmd> {
        let commands: &[PlayerCmd] = match self {
            Self::PlayerControl(PlayerControl::Intent(intent)) => &intent.commands,
            Self::PlayerControl(PlayerControl::Restart { restore }) => restore,
            _ => &[],
        };
        commands.iter()
    }

    /// First underlying actor command for assertions and boundary instrumentation. Runtime
    /// admission supports ordered batches for both reducer intents and transport restoration.
    #[cfg(test)]
    pub(crate) fn player_command(&self) -> Option<&PlayerCmd> {
        self.player_commands().next()
    }
}
