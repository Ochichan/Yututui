//! Remote-control command application.
//!
//! Maps a [`RemoteCommand`] onto the **same** reducer paths a keypress uses
//! ([`App::on_player_action`], [`App::maybe_autoplay_extend`], [`App::quit_app`]), so
//! `ytt -r <cmd>` is mode-independent: `ytt -r next` skips a track even while the TUI is in
//! Search text entry or Settings. Each command also produces a [`RemoteResponse`] computed
//! from the resulting state, which the control socket writes back to the client.

use super::*;
use crate::remote::proto::{
    InstanceMode, QueueItemSnapshot, RemoteCommand, RemoteResponse, RemoteSettingChange,
    SettingsSnapshot, StatusSnapshot, ToggleState,
};

impl App {
    /// Apply one remote command and return `(response, side-effect commands)`. The commands
    /// flow through the normal run-loop dispatch exactly as a keypress's would.
    pub(in crate::app) fn apply_remote(
        &mut self,
        cmd: RemoteCommand,
    ) -> (RemoteResponse, Vec<Cmd>) {
        match cmd {
            RemoteCommand::Next => {
                let cmds = self.on_player_action(Action::NextTrack);
                (self.transport_resp(), cmds)
            }
            RemoteCommand::Prev => {
                let cmds = self.on_player_action(Action::PrevTrack);
                (self.transport_resp(), cmds)
            }
            RemoteCommand::TogglePause => {
                let cmds = self.on_player_action(Action::TogglePause);
                (RemoteResponse::ok(self.pause_line()), cmds)
            }
            RemoteCommand::Play { .. } | RemoteCommand::Enqueue { .. } => {
                (RemoteResponse::err("daemon_required"), Vec::new())
            }
            RemoteCommand::VolumeUp => {
                let cmds = self.on_player_action(Action::VolUp);
                (RemoteResponse::ok(self.vol_line()), cmds)
            }
            RemoteCommand::VolumeDown => {
                let cmds = self.on_player_action(Action::VolDown);
                (RemoteResponse::ok(self.vol_line()), cmds)
            }
            RemoteCommand::SetVolume { percent } => {
                let volume = percent.clamp(0, 100);
                self.playback.volume = volume;
                self.dirty = true;
                (
                    RemoteResponse::ok(self.vol_line()),
                    vec![Cmd::Player(PlayerCmd::SetVolume(volume))],
                )
            }
            RemoteCommand::SeekTo { ms } => {
                if self.queue.current().is_none() {
                    return (RemoteResponse::err("queue_empty"), Vec::new());
                }
                // Same guarded path an OS scrubber drag takes (`MediaCommand::SeekTo`),
                // so range checks and the epoch bump live in one place.
                let cmds = self.apply_media(crate::media::MediaCommand::SeekTo(ms as f64 / 1000.0));
                (RemoteResponse::status(self.status_snapshot()), cmds)
            }
            RemoteCommand::SeekBack => {
                let cmds = self.on_player_action(Action::SeekBack);
                (RemoteResponse::ok(self.now_playing_line()), cmds)
            }
            RemoteCommand::SeekForward => {
                let cmds = self.on_player_action(Action::SeekForward);
                (RemoteResponse::ok(self.now_playing_line()), cmds)
            }
            RemoteCommand::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![self.save_playback_modes_cmd()],
                )
            }
            RemoteCommand::CycleRepeat => {
                self.queue.cycle_repeat();
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![self.save_playback_modes_cmd()],
                )
            }
            RemoteCommand::QueuePlay { position } => {
                if position >= self.queue.len() {
                    (RemoteResponse::err("queue_index"), Vec::new())
                } else {
                    let cmds = self.queue_popup_play(position);
                    (RemoteResponse::status(self.status_snapshot()), cmds)
                }
            }
            RemoteCommand::QueueRemove { position } => {
                if position >= self.queue.len() {
                    (RemoteResponse::err("queue_index"), Vec::new())
                } else {
                    let cmds = self.remove_queue_range(position, position);
                    (RemoteResponse::status(self.status_snapshot()), cmds)
                }
            }
            RemoteCommand::Streaming { state } => self.remote_set_streaming(state),
            RemoteCommand::SetSetting { change } => self.remote_set_setting(change),
            RemoteCommand::ResumeSession => self.remote_resume_session(),
            RemoteCommand::Status => (RemoteResponse::status(self.status_snapshot()), Vec::new()),
            RemoteCommand::Quit => {
                let cmds = self.quit_app();
                (RemoteResponse::ok("quitting ytt".to_string()), cmds)
            }
        }
    }

    fn remote_resume_session(&mut self) -> (RemoteResponse, Vec<Cmd>) {
        if self.queue.is_empty() {
            self.restore_last_played_from_library();
        }
        if self.queue.current().is_none() {
            return (RemoteResponse::err("session_empty"), Vec::new());
        }
        let song = self.queue.current().cloned();
        let mut cmds = self.load_song(song);
        if self.autoplay_streaming {
            cmds.extend(self.force_autoplay_extend());
        }
        (RemoteResponse::status(self.status_snapshot()), cmds)
    }

    /// Set/toggle autoplay streaming, mirroring the `ToggleStreaming` key handler (status toast +
    /// an immediate top-up when enabling, so a low queue doesn't gap before the next track).
    fn remote_set_streaming(&mut self, state: ToggleState) -> (RemoteResponse, Vec<Cmd>) {
        let on = state.resolve(self.autoplay_streaming);
        self.autoplay_streaming = on;
        self.status.text = format!(
            "{}: {}",
            t!("Autoplay streaming", "자동 스트리밍"),
            if on { "✓" } else { "✗" }
        );
        self.dirty = true;
        let mut cmds = vec![self.save_playback_modes_cmd()];
        if on {
            cmds.extend(self.force_autoplay_extend());
        }
        (
            RemoteResponse::ok(format!("streaming {}", if on { "on" } else { "off" })),
            cmds,
        )
    }

    fn remote_set_setting(&mut self, change: RemoteSettingChange) -> (RemoteResponse, Vec<Cmd>) {
        match change {
            RemoteSettingChange::AutoplayStreaming { value } => {
                self.remote_set_streaming(if value {
                    ToggleState::On
                } else {
                    ToggleState::Off
                })
            }
            RemoteSettingChange::StreamingMode { value } => {
                self.config.streaming.mode = value;
                self.status.text = format!("Streaming mode: {}", value.label());
                self.dirty = true;
                let mut cmds = vec![Cmd::SaveConfig(Box::new(self.config.clone()))];
                if self.autoplay_streaming {
                    cmds.extend(self.force_autoplay_extend());
                }
                (RemoteResponse::status(self.status_snapshot()), cmds)
            }
            RemoteSettingChange::StreamingSource { value } => {
                let search = self.config.effective_search();
                let source = search.normalized_streaming_source(value);
                self.config.search.streaming_source = source;
                self.status.text = format!("Streaming source: {}", source.label());
                self.dirty = true;
                let mut cmds = vec![Cmd::SaveConfig(Box::new(self.config.clone()))];
                if self.autoplay_streaming {
                    cmds.extend(self.force_autoplay_extend());
                }
                (RemoteResponse::status(self.status_snapshot()), cmds)
            }
            RemoteSettingChange::Speed { tenths } => {
                let speed = settings::clamp_speed(f64::from(tenths) / 10.0);
                self.playback.speed = speed;
                self.config.speed = Some(speed);
                self.status.text = format!("{}: {:.1}x", t!("Speed", "재생 속도"), speed);
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![
                        Cmd::SaveConfig(Box::new(self.config.clone())),
                        Cmd::Player(PlayerCmd::SetProperty {
                            name: "speed".to_owned(),
                            value: serde_json::Value::from(speed),
                        }),
                    ],
                )
            }
            RemoteSettingChange::SeekSeconds { seconds } => {
                let seek_seconds = settings::clamp_seek_seconds(f64::from(seconds));
                self.audio.seek_seconds = seek_seconds;
                self.config.seek_seconds = Some(seek_seconds);
                self.status.text = format!("Seek step: {:.0}s", seek_seconds);
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![Cmd::SaveConfig(Box::new(self.config.clone()))],
                )
            }
            RemoteSettingChange::Normalize { value } => {
                self.audio.normalize = value;
                self.config.normalize = Some(value);
                self.status.text = format!(
                    "{}: {}",
                    t!("Normalize", "노멀라이즈"),
                    if value { "✓" } else { "✗" }
                );
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![
                        Cmd::SaveConfig(Box::new(self.config.clone())),
                        Cmd::Player(PlayerCmd::SetAudioFilter(
                            self.current_af().unwrap_or_default(),
                        )),
                    ],
                )
            }
            RemoteSettingChange::Gapless { value } => {
                self.config.gapless = Some(value);
                self.status.text = format!("Gapless: {}", if value { "on" } else { "off" });
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![Cmd::SaveConfig(Box::new(self.config.clone()))],
                )
            }
            RemoteSettingChange::AiEnabled { value } => {
                let old_ai_enabled = self.config.effective_ai_enabled();
                self.config.ai_enabled = Some(value);
                if !value {
                    self.ai.available = false;
                    self.ai.thinking = false;
                    self.streaming.pending_rerank = None;
                }
                self.status.text = format!("DJ Gem: {}", if value { "on" } else { "off" });
                self.dirty = true;
                let mut cmds = vec![Cmd::SaveConfig(Box::new(self.config.clone()))];
                if self.config.effective_ai_enabled() != old_ai_enabled {
                    cmds.push(Cmd::ReloadAi {
                        key: self.config.effective_ai_service_key(),
                        model: self.ai.model,
                        assistant_enabled: self.config.effective_ai_enabled(),
                    });
                }
                (RemoteResponse::status(self.status_snapshot()), cmds)
            }
            RemoteSettingChange::RadioMode { state } => {
                let next = state.resolve(self.radio_dedicated_mode);
                let cmds = if next == self.radio_dedicated_mode {
                    Vec::new()
                } else if next {
                    self.apply_radio_mode_confirm(RadioModeConfirm::Enter)
                } else {
                    self.apply_radio_mode_confirm(RadioModeConfirm::Exit)
                };
                (RemoteResponse::status(self.status_snapshot()), cmds)
            }
        }
    }

    /// A transport response: the now-playing line on success, or `queue_empty` when nothing
    /// is loaded (so `ytt -r next` on an empty queue is a clean rejection, not a fake OK).
    fn transport_resp(&self) -> RemoteResponse {
        if self.queue.current().is_some() {
            RemoteResponse::ok(self.now_playing_line())
        } else {
            RemoteResponse::err("queue_empty")
        }
    }

    fn now_playing_line(&self) -> String {
        match self.queue.current() {
            Some(s) => self.display_song_label(s),
            None => "nothing playing".to_string(),
        }
    }

    fn pause_line(&self) -> String {
        let state = if self.playback.paused {
            "paused"
        } else {
            "playing"
        };
        match self.queue.current() {
            Some(s) => format!("{state}: {}", self.display_song_label(s)),
            None => state.to_string(),
        }
    }

    fn vol_line(&self) -> String {
        format!("volume {}%", self.playback.volume)
    }

    fn status_snapshot(&self) -> StatusSnapshot {
        let cur = self.queue.current();
        let (position, total) = self.queue.position();
        let mut settings = SettingsSnapshot::from_config(&self.config, self.radio_dedicated_mode);
        settings.autoplay_streaming = self.autoplay_streaming;
        settings.speed_tenths = (self.playback.speed * 10.0).round() as u16;
        settings.seek_seconds = self.audio.seek_seconds.round() as u16;
        settings.normalize = self.audio.normalize;
        StatusSnapshot {
            title: cur.map(|s| self.display_title(s).into_owned()),
            artist: cur.map(|s| self.display_artist(s).into_owned()),
            paused: self.playback.paused,
            volume: self.playback.volume,
            position: if total == 0 { 0 } else { position },
            total,
            streaming: self.autoplay_streaming,
            owner_mode: InstanceMode::StandaloneTui,
            settings,
            queue: self
                .queue
                .ordered_iter()
                .enumerate()
                .map(|(index, song)| QueueItemSnapshot {
                    title: self.display_title(song).into_owned(),
                    artist: self.display_artist(song).into_owned(),
                    duration: song.duration.clone(),
                    current: index == self.queue.cursor_pos(),
                })
                .collect(),
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
            elapsed_ms: cur.and(self.playback.time_pos).map(|pos| {
                // Interpolate to "now" like the OS media session does, so the mini
                // player's progress bar starts from a fresh value between polls.
                let mut pos = pos;
                if !self.playback.paused
                    && let Some(at) = self.playback.time_pos_at
                {
                    pos += at.elapsed().as_secs_f64() * self.playback.speed;
                }
                if let Some(duration) = self.playback.duration {
                    pos = pos.min(duration);
                }
                (pos.max(0.0) * 1000.0) as u64
            }),
            duration_ms: cur
                .and(self.playback.duration)
                .map(|duration| (duration.max(0.0) * 1000.0) as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;

    fn two_track_app() -> App {
        let mut app = App::new(50);
        app.queue.set(
            vec![
                Song::remote("id0", "Zero", "A", "3:00"),
                Song::remote("id1", "One", "B", "3:00"),
            ],
            0,
        );
        app
    }

    fn six_track_app() -> App {
        let mut app = App::new(50);
        app.queue.set(
            (0..6)
                .map(|i| Song::remote(format!("id{i}"), format!("Track {i}"), "A", "3:00"))
                .collect(),
            0,
        );
        app
    }

    #[test]
    fn next_advances_even_in_search_mode() {
        let mut app = two_track_app();
        // The whole point of routing through the reducer (not key replay): a non-player
        // input mode must not swallow the command as text.
        app.mode = Mode::Search;
        let (resp, _cmds) = app.apply_remote(RemoteCommand::Next);
        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
    }

    #[test]
    fn next_on_empty_queue_is_rejected() {
        let mut app = App::new(50);
        let (resp, _cmds) = app.apply_remote(RemoteCommand::Next);
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("queue_empty"));
    }

    #[test]
    fn streaming_on_off_toggle_set_autoplay_streaming() {
        let mut app = App::new(50);
        app.mode = Mode::Settings; // mode-independent
        assert!(!app.autoplay_streaming);

        let (resp, _) = app.apply_remote(RemoteCommand::Streaming {
            state: ToggleState::On,
        });
        assert!(resp.ok);
        assert!(app.autoplay_streaming);

        app.apply_remote(RemoteCommand::Streaming {
            state: ToggleState::Off,
        });
        assert!(!app.autoplay_streaming);

        app.apply_remote(RemoteCommand::Streaming {
            state: ToggleState::Toggle,
        });
        assert!(app.autoplay_streaming);
    }

    #[test]
    fn streaming_on_forces_refill_even_when_queue_is_not_low() {
        let mut app = six_track_app();
        assert!(app.queue.remaining() > AUTOPLAY_THRESHOLD);

        let (resp, cmds) = app.apply_remote(RemoteCommand::Streaming {
            state: ToggleState::On,
        });

        assert!(resp.ok);
        assert!(app.autoplay_streaming);
        assert!(app.streaming.pending);
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::StreamingFallback {
                seed_video_id,
                ..
            } if seed_video_id == "id0"
        )));
    }

    #[test]
    fn streaming_source_change_forces_refill_while_streaming_is_on() {
        let mut app = six_track_app();
        app.autoplay_streaming = true;
        app.streaming.last_extend = Some(Instant::now());

        let (resp, cmds) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::StreamingSource {
                value: SearchSource::Jamendo,
            },
        });

        assert!(resp.ok);
        assert!(app.streaming.pending);
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::StreamingFallback { .. }))
        );
    }

    #[test]
    fn disabling_dj_gem_forces_streaming_results_to_local_path() {
        let mut app = two_track_app();
        app.autoplay_streaming = true;
        app.ai.available = true;

        let (resp, _) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::AiEnabled { value: false },
        });
        assert!(resp.ok);
        assert!(!app.ai.available);

        let before = app.queue.len();
        let cmds = app.update(Msg::StreamingResults {
            seed_video_id: "id0".to_owned(),
            candidates: vec![(
                Song::remote("cand1", "Candidate", "B", "3:00"),
                CandidateSource::YtdlpStreaming,
            )],
        });

        assert!(cmds.iter().all(|cmd| !matches!(cmd, Cmd::AiRerank { .. })));
        assert!(app.queue.len() > before);
    }

    #[test]
    fn setting_command_updates_streaming_mode_and_source() {
        let mut app = App::new(50);

        let (resp, cmds) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::StreamingMode {
                value: StreamingMode::Discovery,
            },
        });
        assert!(resp.ok);
        assert_eq!(app.config.streaming.mode, StreamingMode::Discovery);
        assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::SaveConfig(_))));

        app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::StreamingSource {
                value: SearchSource::Jamendo,
            },
        });
        assert_eq!(app.config.search.streaming_source, SearchSource::Jamendo);
    }

    #[test]
    fn setting_command_updates_playback_speed_live() {
        let mut app = App::new(50);
        let (resp, cmds) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::Speed { tenths: 13 },
        });

        assert!(resp.ok);
        assert_eq!(app.playback.speed, 1.3);
        assert_eq!(app.config.speed, Some(1.3));
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::Player(PlayerCmd::SetProperty { name, value })
                    if name == "speed" && value == &serde_json::Value::from(1.3)
            )
        }));
    }

    #[test]
    fn setting_command_can_toggle_radio_mode_for_tui_owner() {
        let mut app = App::new(50);

        let (resp, _) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::RadioMode {
                state: ToggleState::On,
            },
        });
        assert!(resp.ok);
        assert!(app.radio_dedicated_mode);

        app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::RadioMode {
                state: ToggleState::Off,
            },
        });
        assert!(!app.radio_dedicated_mode);
    }

    #[test]
    fn resume_session_loads_last_history_track() {
        let mut app = App::new(50);
        app.library
            .record_play(&Song::remote("id0", "Zero", "A", "3:00"));

        let (resp, cmds) = app.apply_remote(RemoteCommand::ResumeSession);

        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "id0");
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::Player(crate::player::PlayerCmd::Load(url)) if url.contains("id0")
            )
        }));
    }

    #[test]
    fn resume_session_without_history_is_rejected() {
        let mut app = App::new(50);
        let (resp, cmds) = app.apply_remote(RemoteCommand::ResumeSession);

        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("session_empty"));
        assert!(cmds.is_empty());
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut app = App::new(50);
        assert!(!app.should_quit);
        let (resp, _) = app.apply_remote(RemoteCommand::Quit);
        assert!(resp.ok);
        assert!(app.should_quit);
    }

    #[test]
    fn volume_up_raises_volume_and_reports_it() {
        let mut app = App::new(50);
        let before = app.playback.volume;
        let (resp, _) = app.apply_remote(RemoteCommand::VolumeUp);
        assert!(resp.ok);
        assert!(app.playback.volume > before);
        assert!(resp.message.unwrap().contains("volume"));
    }

    #[test]
    fn set_volume_clamps_and_applies() {
        let mut app = App::new(50);
        let (resp, cmds) = app.apply_remote(RemoteCommand::SetVolume { percent: 250 });
        assert!(resp.ok);
        assert_eq!(app.playback.volume, 100);
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::Player(PlayerCmd::SetVolume(100))))
        );

        let (resp, _) = app.apply_remote(RemoteCommand::SetVolume { percent: 35 });
        assert!(resp.ok);
        assert_eq!(app.playback.volume, 35);
    }

    #[test]
    fn seek_to_requires_a_track_and_seeks_absolutely() {
        let mut app = App::new(50);
        let (resp, cmds) = app.apply_remote(RemoteCommand::SeekTo { ms: 5_000 });
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("queue_empty"));
        assert!(cmds.is_empty());

        let mut app = two_track_app();
        app.prefetch.loaded_video_id = Some("id0".to_string());
        app.playback.time_pos = Some(1.0);
        app.playback.duration = Some(180.0);
        let (resp, cmds) = app.apply_remote(RemoteCommand::SeekTo { ms: 90_000 });
        assert!(resp.ok);
        assert!(cmds.iter().any(
            |cmd| matches!(cmd, Cmd::Player(PlayerCmd::SeekAbsolute(pos)) if (*pos - 90.0).abs() < 1e-9)
        ));
        // (The position-epoch bump happens centrally in `App::update`, which wraps
        // this reducer in production — not in `apply_remote` itself.)
        let snapshot = resp.status.expect("status snapshot present");
        assert_eq!(snapshot.duration_ms, Some(180_000));
        assert!(snapshot.elapsed_ms.is_some());
    }

    #[test]
    fn status_reports_queue_and_streaming() {
        let mut app = two_track_app();
        app.autoplay_streaming = true;
        let (resp, cmds) = app.apply_remote(RemoteCommand::Status);
        assert!(cmds.is_empty());
        let snap = resp.status.expect("status snapshot present");
        assert_eq!(snap.total, 2);
        assert_eq!(snap.position, 1);
        assert!(snap.streaming);
        assert_eq!(snap.title.as_deref(), Some("Zero"));
    }

    #[test]
    fn status_reports_queue_rows_and_play_modes() {
        let mut app = two_track_app();
        app.queue.shuffle = true;
        app.queue.repeat = crate::queue::Repeat::One;

        let (resp, cmds) = app.apply_remote(RemoteCommand::Status);

        assert!(cmds.is_empty());
        let snap = resp.status.expect("status snapshot present");
        assert!(snap.shuffle);
        assert_eq!(snap.repeat, crate::queue::Repeat::One);
        assert_eq!(snap.queue.len(), 2);
        assert_eq!(snap.queue[0].title, "Zero");
        assert_eq!(snap.queue[0].artist, "A");
        assert!(snap.queue[0].current);
        assert!(!snap.queue[1].current);
    }

    #[test]
    fn queue_play_jumps_and_loads_selected_track() {
        let mut app = two_track_app();

        let (resp, cmds) = app.apply_remote(RemoteCommand::QueuePlay { position: 1 });

        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::Player(crate::player::PlayerCmd::Load(url)) if url.contains("id1")
            )
        }));
    }

    #[test]
    fn queue_remove_current_loads_next_track() {
        let mut app = two_track_app();

        let (resp, cmds) = app.apply_remote(RemoteCommand::QueueRemove { position: 0 });

        assert!(resp.ok);
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::Player(crate::player::PlayerCmd::Load(url)) if url.contains("id1")
            )
        }));
    }

    #[test]
    fn remote_shuffle_and_repeat_persist_modes() {
        let mut app = two_track_app();

        let (shuffle_resp, shuffle_cmds) = app.apply_remote(RemoteCommand::ToggleShuffle);
        assert!(shuffle_resp.ok);
        assert!(app.queue.shuffle);
        assert!(
            shuffle_cmds.iter().any(|cmd| {
                matches!(cmd, Cmd::SaveConfig(config) if config.shuffle == Some(true))
            })
        );

        let (repeat_resp, repeat_cmds) = app.apply_remote(RemoteCommand::CycleRepeat);
        assert!(repeat_resp.ok);
        assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
        assert!(repeat_cmds.iter().any(|cmd| {
            matches!(cmd, Cmd::SaveConfig(config) if config.repeat == crate::queue::Repeat::All)
        }));
    }
}
