//! Remote-control command application.
//!
//! Maps a [`RemoteCommand`] onto the **same** reducer paths a keypress uses
//! ([`App::on_player_action`], [`App::maybe_autoplay_extend`], [`App::quit_app`]), so
//! `ytt -r <cmd>` is mode-independent: `ytt -r next` skips a track even while the TUI is in
//! Search text entry or Settings. Each command also produces a [`RemoteResponse`] computed
//! from the resulting state, which the control socket writes back to the client.

use super::*;
use crate::remote::proto::{
    ArtworkRef, InstanceMode, QueueItemSnapshot, RemoteCommand, RemoteResponse,
    RemoteSettingChange, SettingsSnapshot, StatusSnapshot, ToggleState,
};

impl App {
    pub(in crate::app) fn remote_reply_plan(cmd: &RemoteCommand) -> Option<RemoteReplyPlan> {
        match cmd {
            RemoteCommand::Next | RemoteCommand::Prev => Some(RemoteReplyPlan::Transport),
            RemoteCommand::TogglePause => Some(RemoteReplyPlan::Pause),
            RemoteCommand::VolumeUp
            | RemoteCommand::VolumeDown
            | RemoteCommand::SetVolume { .. } => Some(RemoteReplyPlan::Volume),
            RemoteCommand::SeekBack | RemoteCommand::SeekForward => {
                Some(RemoteReplyPlan::NowPlaying)
            }
            RemoteCommand::SeekTo { .. }
            | RemoteCommand::QueuePlay { .. }
            | RemoteCommand::QueuePlayIfRevision { .. }
            | RemoteCommand::QueueRemove { .. }
            | RemoteCommand::QueueRemoveIfRevision { .. }
            | RemoteCommand::ResumeSession
            | RemoteCommand::SetSetting { .. } => Some(RemoteReplyPlan::Status),
            _ => None,
        }
    }

    pub(crate) fn resolve_remote_reply(&self, plan: RemoteReplyPlan) -> RemoteResponse {
        match plan {
            RemoteReplyPlan::Fixed(response) => *response,
            RemoteReplyPlan::Pause => RemoteResponse::ok(self.pause_line()),
            RemoteReplyPlan::Volume => RemoteResponse::ok(self.vol_line()),
            RemoteReplyPlan::Status => RemoteResponse::status(self.status_snapshot()),
            RemoteReplyPlan::Transport => self.transport_resp(),
            RemoteReplyPlan::NowPlaying => RemoteResponse::ok(self.now_playing_line()),
        }
    }

    /// Apply one remote command and return `(response, side-effect commands)`. The commands
    /// flow through the normal run-loop dispatch exactly as a keypress's would.
    pub(in crate::app) fn apply_remote(
        &mut self,
        cmd: RemoteCommand,
    ) -> (RemoteResponse, Vec<Cmd>) {
        if cmd
            .expected_queue_rev()
            .is_some_and(|rev| rev != self.queue.rev())
        {
            return (RemoteResponse::err("stale_rev"), Vec::new());
        }
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
            // GUI-session verbs (docs/gui/02): like Play/Enqueue, searches need the api
            // actor lane the standalone TUI reserves for its own Search screen — the GUI
            // is expected to talk to a daemon owner. Settings WRITES are also daemon-only
            // for now: the TUI's Settings screen state derives from config at draw time,
            // so a remote mutation would need the same reducer plumbing its keypresses
            // use (follow-up); settings READS already work (the snapshot projects
            // `self.config` via core_view).
            RemoteCommand::RunSearch { .. }
            | RemoteCommand::PlayTracks { .. }
            | RemoteCommand::EnqueueTracks { .. }
            | RemoteCommand::Apply { .. }
            | RemoteCommand::SetGeminiKey { .. }
            | RemoteCommand::ResetAllSettings => {
                (RemoteResponse::err("daemon_required"), Vec::new())
            }
            // Intercepted by the top-level reducer before this playback/settings dispatcher so
            // the reply can stay open until the blocking writer finishes.
            RemoteCommand::ExportPersonalData { .. } => {
                (RemoteResponse::err("invalid_export_dispatch"), Vec::new())
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
                let volume = percent.clamp(0, crate::playback_policy::VOLUME_MAX);
                (
                    RemoteResponse::ok(format!("volume: {volume}%")),
                    self.player_intent(
                        "set_volume",
                        PlayerCmd::SetVolume(volume),
                        PlayerCommit::Volume {
                            volume,
                            pre_mute_volume: None,
                        },
                    ),
                )
            }
            RemoteCommand::SeekTo { ms } => {
                if self.queue.current().is_none() {
                    return (RemoteResponse::err("queue_empty"), Vec::new());
                }
                // Clamp a remote seek: unlike the MPRIS OS surface (which ignores out-of-range
                // per spec), the remote API clamps, so an over-long `ms` lands at the end
                // instead of being dropped. The shared clamp also caps at MAX_SEEK_SECONDS so
                // an absurd value can't reach mpv when the duration is unknown (live/unprobed).
                let secs = crate::playback_policy::clamp_seek_target(
                    ms as f64 / 1000.0,
                    self.playback.duration,
                );
                // Then through the same guarded path an OS scrubber drag takes, so the epoch
                // bump and radio guard live in one place.
                let cmds = self.apply_media(crate::media::MediaCommand::SeekTo(secs));
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
                // Music-mode invariant: reject turning repeat on while autoplay is on (mirrors
                // the daemon engine so parity holds). Off→All is the only enabling transition.
                if self
                    .queue
                    .repeat
                    .cycle_blocked_by_streaming(self.autoplay_streaming)
                {
                    self.status.text = t!(
                        "Can't use repeat while autoplay is on",
                        "자동재생 중에는 반복을 켤 수 없어요"
                    )
                    .to_owned();
                    self.dirty = true;
                    return (
                        RemoteResponse::err("incompatible_playback_modes"),
                        Vec::new(),
                    );
                }
                self.queue.cycle_repeat();
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![self.save_playback_modes_cmd()],
                )
            }
            RemoteCommand::QueuePlay { position }
            | RemoteCommand::QueuePlayIfRevision { position, .. } => {
                self.remote_queue_play(position)
            }
            RemoteCommand::QueueRemove { position }
            | RemoteCommand::QueueRemoveIfRevision { position, .. } => {
                self.remote_queue_remove(position)
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

    fn remote_queue_play(&mut self, position: usize) -> (RemoteResponse, Vec<Cmd>) {
        if position >= self.queue.len() {
            return (RemoteResponse::err("queue_index"), Vec::new());
        }
        // The shared transition commits the cursor, popup state, load bookkeeping, and
        // low-queue streaming top-up only after the player accepts the batch.
        let cmds = self.queue_popup_play(position);
        (RemoteResponse::status(self.status_snapshot()), cmds)
    }

    fn remote_queue_remove(&mut self, position: usize) -> (RemoteResponse, Vec<Cmd>) {
        if position >= self.queue.len() {
            return (RemoteResponse::err("queue_index"), Vec::new());
        }
        let mut cmds = self.remove_queue_range(position, position);
        // Current-inclusive removals already run the refill check from their accepted Track
        // commit. A non-current removal commits synchronously and owns its refill here.
        let waits_for_player = cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::PlayerControl(PlayerControl::Intent(_))));
        if !waits_for_player {
            cmds.extend(self.maybe_autoplay_extend());
        }
        (RemoteResponse::status(self.status_snapshot()), cmds)
    }

    fn remote_resume_session(&mut self) -> (RemoteResponse, Vec<Cmd>) {
        if self.queue.current().is_none() {
            let Some(song) = self.library.history.front().cloned() else {
                return (RemoteResponse::err("session_empty"), Vec::new());
            };
            let cmds = self.replace_queue_and_load(
                vec![song],
                0,
                None,
                QueueReplacementOptions {
                    force_autoplay_extend: self.autoplay_streaming,
                    ..QueueReplacementOptions::default()
                },
            );
            return (RemoteResponse::status(self.status_snapshot()), cmds);
        }
        let cmds = self.resume_current_track();
        (RemoteResponse::status(self.status_snapshot()), cmds)
    }

    /// Set/toggle autoplay streaming, mirroring the `ToggleStreaming` key handler (status toast +
    /// an immediate top-up when enabling, so a low queue doesn't gap before the next track).
    fn remote_set_streaming(&mut self, state: ToggleState) -> (RemoteResponse, Vec<Cmd>) {
        let on = state.resolve(self.autoplay_streaming);
        // Music-mode invariant: reject the enable without changing playback/config state or
        // emitting effects. The TUI still surfaces the same localized notice as its key path.
        if on && self.queue.repeat.is_on() {
            self.status.text = t!(
                "Can't use autoplay while repeat is on",
                "반복 재생 중에는 자동재생을 켤 수 없어요"
            )
            .to_owned();
            self.dirty = true;
            return (
                RemoteResponse::err("incompatible_playback_modes"),
                Vec::new(),
            );
        }
        self.set_autoplay_streaming(on);
        self.status.text = format!(
            "{}: {}",
            t!("Autoplay", "자동재생"),
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
                self.status.text = format!("Curating style: {}", value.label());
                self.dirty = true;
                let mut cmds = vec![Cmd::Persist(PersistCmd::Config(Box::new(
                    self.config.clone(),
                )))];
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
                let mut cmds = vec![Cmd::Persist(PersistCmd::Config(Box::new(
                    self.config.clone(),
                )))];
                if self.autoplay_streaming {
                    cmds.extend(self.force_autoplay_extend());
                }
                (RemoteResponse::status(self.status_snapshot()), cmds)
            }
            RemoteSettingChange::Speed { tenths } => {
                let speed = settings::clamp_speed(f64::from(tenths) / 10.0);
                (
                    RemoteResponse::status(self.status_snapshot()),
                    self.player_intent(
                        "set_speed",
                        PlayerCmd::SetProperty {
                            name: "speed".to_owned(),
                            value: serde_json::Value::from(speed),
                        },
                        PlayerCommit::Speed {
                            speed,
                            announce: true,
                            persist: true,
                        },
                    ),
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
                    vec![Cmd::Persist(PersistCmd::Config(Box::new(
                        self.config.clone(),
                    )))],
                )
            }
            RemoteSettingChange::Normalize { value } => (
                RemoteResponse::status(self.status_snapshot()),
                self.normalize_intent(value, true),
            ),
            RemoteSettingChange::Gapless { value } => {
                self.config.gapless = Some(value);
                self.status.text = format!("Gapless: {}", if value { "on" } else { "off" });
                self.dirty = true;
                (
                    RemoteResponse::status(self.status_snapshot()),
                    vec![Cmd::Persist(PersistCmd::Config(Box::new(
                        self.config.clone(),
                    )))],
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
                let mut cmds = vec![Cmd::Persist(PersistCmd::Config(Box::new(
                    self.config.clone(),
                )))];
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

    /// A transport response derived from what the player actually accepted, not merely the
    /// queue cursor (which intentionally remains on the last item after repeat-off ends).
    fn transport_resp(&self) -> RemoteResponse {
        if self.prefetch.loaded_video_id.is_some() && self.queue.current().is_some() {
            RemoteResponse::ok(self.now_playing_line())
        } else if self.queue.is_empty() {
            RemoteResponse::err("queue_empty")
        } else {
            RemoteResponse::err("queue_end")
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
            title: cur.map(|s| crate::api::sanitize_title(self.display_title(s).as_ref())),
            artist: cur.map(|s| crate::api::sanitize_artist(self.display_artist(s).as_ref())),
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
                    title: crate::api::sanitize_title(self.display_title(song).as_ref()),
                    artist: crate::api::sanitize_artist(self.display_artist(song).as_ref()),
                    duration: crate::api::sanitize_duration(&song.duration),
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
            is_live: cur.is_some_and(|song| song.is_radio_station()),
            queue_rev: Some(self.queue.rev()),
            track_id: cur.map(|song| crate::api::sanitize_provider_id(&song.video_id)),
            position_epoch: self.playback.position_epoch,
            // Same current-track gate as the OS media snapshot (media_reducer): stale
            // art from the previous track never rides a status reply.
            artwork: cur.and_then(|song| {
                self.media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| ArtworkRef {
                        key: art.key.clone(),
                        path: Some(art.path.to_string_lossy().into_owned()),
                        mime: None,
                    })
            }),
        }
    }

    /// The v8 publisher's read view of this owner (docs/gui/02 §14). Same interpolation
    /// math as [`status_snapshot`](Self::status_snapshot) / the OS media session, so a
    /// pushed snapshot's position is fresh at emit time.
    pub fn core_view(&self) -> crate::remote::publish::CoreView<'_> {
        let cur = self.queue.current();
        crate::remote::publish::CoreView {
            queue: &self.queue,
            paused: self.playback.paused,
            volume: self.playback.volume,
            speed_tenths: (self.playback.speed * 10.0).round() as u16,
            elapsed_ms: cur.and(self.playback.time_pos).map(|mut pos| {
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
            position_epoch: self.playback.position_epoch,
            streaming: self.autoplay_streaming,
            radio_mode: self.radio_dedicated_mode,
            stream_now_playing: self
                .playback
                .stream_now_playing
                .as_ref()
                .map(|now| std::borrow::Cow::Owned(now.label())),
            owner_mode: InstanceMode::StandaloneTui,
            eq_preset: self.audio.preset.label(),
            eq_bands: self.audio.bands,
            eq_normalize: self.audio.normalize,
            config: &self.config,
            // Same current-track gate as status_snapshot above.
            artwork: cur.and_then(|song| {
                self.media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| crate::remote::publish::CoreArtwork {
                        key: &art.key,
                        path: Some(art.path.as_path()),
                        mime: None,
                    })
            }),
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

    fn live_radio(id: &str) -> Song {
        let mut song = Song::remote(id, "Live station", "Station", "");
        song.playable = Some(crate::api::PlayableRef::RadioStream {
            url: format!("https://radio.example/{id}.mp3"),
        });
        song
    }

    #[test]
    fn status_distinguishes_unknown_duration_from_genuine_live_stream() {
        let mut app = App::new(50);
        app.queue
            .set(vec![Song::remote("loading", "Loading", "Artist", "")], 0);

        let (unknown_response, _) = app.apply_remote(RemoteCommand::Status);
        let unknown = unknown_response.status.expect("status snapshot");
        assert_eq!(unknown.duration_ms, None);
        assert!(!unknown.is_live);
        assert_eq!(unknown.queue_rev, Some(app.queue.rev()));
        assert_eq!(unknown.track_id.as_deref(), Some("loading"));
        assert_eq!(unknown.position_epoch, app.playback.position_epoch);

        app.queue.set(vec![live_radio("station")], 0);
        let (live_response, _) = app.apply_remote(RemoteCommand::Status);
        let live = live_response.status.expect("status snapshot");
        assert_eq!(live.duration_ms, None);
        assert!(live.is_live);
        assert_eq!(live.queue_rev, Some(app.queue.rev()));
        assert_eq!(live.track_id.as_deref(), Some("station"));
        assert_eq!(live.position_epoch, app.playback.position_epoch);
    }

    #[test]
    fn next_advances_even_in_search_mode() {
        let mut app = two_track_app();
        // The whole point of routing through the reducer (not key replay): a non-player
        // input mode must not swallow the command as text.
        app.mode = Mode::Search;
        let (_pre_admission_resp, cmds) = app.apply_remote(RemoteCommand::Next);
        assert_eq!(app.queue.current().unwrap().video_id, "id0");
        let _follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Transport);
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
    fn remote_streaming_enables_reject_without_state_or_effects() {
        for command in [
            RemoteCommand::Streaming {
                state: ToggleState::On,
            },
            RemoteCommand::Streaming {
                state: ToggleState::Toggle,
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::AutoplayStreaming { value: true },
            },
        ] {
            let mut app = App::new(50);
            app.queue.repeat = crate::queue::Repeat::One;
            app.config.repeat = crate::queue::Repeat::One;
            app.config.autoplay_streaming = Some(false);
            app.streaming.consecutive_failures = 2;
            app.status.text = "before".to_owned();
            app.dirty = false;

            let (resp, cmds) = app.apply_remote(command);

            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("incompatible_playback_modes"));
            assert!(cmds.is_empty(), "a rejection must emit no effects");
            assert!(!app.autoplay_streaming);
            assert_eq!(app.queue.repeat, crate::queue::Repeat::One);
            assert_eq!(app.config.autoplay_streaming, Some(false));
            assert_eq!(app.config.repeat, crate::queue::Repeat::One);
            assert_eq!(app.streaming.consecutive_failures, 2);
            assert!(matches!(
                app.status.text.as_str(),
                "Can't use autoplay while repeat is on"
                    | "반복 재생 중에는 자동재생을 켤 수 없어요"
            ));
            assert!(app.dirty, "the localized rejection notice must redraw");
        }
    }

    #[test]
    fn remote_cycle_repeat_rejects_without_state_or_effects() {
        let mut app = App::new(50);
        app.autoplay_streaming = true;
        app.config.autoplay_streaming = Some(true);
        app.streaming.consecutive_failures = 2;
        app.status.text = "before".to_owned();
        app.dirty = false;

        let (resp, cmds) = app.apply_remote(RemoteCommand::CycleRepeat);

        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("incompatible_playback_modes"));
        assert!(cmds.is_empty(), "a rejection must emit no effects");
        assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
        assert_eq!(app.config.repeat, crate::queue::Repeat::Off);
        assert_eq!(app.config.autoplay_streaming, Some(true));
        assert_eq!(app.streaming.consecutive_failures, 2);
        assert!(matches!(
            app.status.text.as_str(),
            "Can't use repeat while autoplay is on" | "자동재생 중에는 반복을 켤 수 없어요"
        ));
        assert!(app.dirty, "the localized rejection notice must redraw");
    }

    #[test]
    fn repeat_streaming_disables_remain_allowed() {
        for command in [
            RemoteCommand::Streaming {
                state: ToggleState::Off,
            },
            RemoteCommand::Streaming {
                state: ToggleState::Toggle,
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::AutoplayStreaming { value: false },
            },
        ] {
            let mut app = App::new(50);
            app.autoplay_streaming = true;
            app.queue.repeat = crate::queue::Repeat::One;

            let (resp, cmds) = app.apply_remote(command);

            assert!(resp.ok);
            assert!(!app.autoplay_streaming);
            assert_eq!(app.queue.repeat, crate::queue::Repeat::One);
            assert!(cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::Persist(PersistCmd::Config(config))
                    if config.autoplay_streaming == Some(false)
            )));
        }

        let mut app = App::new(50);
        app.autoplay_streaming = true;
        app.queue.repeat = crate::queue::Repeat::One;
        let (resp, cmds) = app.apply_remote(RemoteCommand::CycleRepeat);
        assert!(resp.ok);
        assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
        assert!(app.autoplay_streaming);
        assert!(!cmds.is_empty(), "the allowed repeat change is persisted");
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
        let cmds = app.update(StreamingMsg::Results {
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
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
        );

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
        assert_eq!(app.playback.speed, 1.0);
        assert_eq!(app.config.speed, None);
        assert!(cmds.iter().all(|cmd| !matches!(cmd, Cmd::Persist(_))));
        assert!(matches!(
            cmds.as_slice(),
            [cmd] if matches!(
                cmd.player_command(),
                Some(PlayerCmd::SetProperty { name, value })
                    if name == "speed" && value == &serde_json::Value::from(1.3)
            )
        ));

        let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
        assert_eq!(app.playback.speed, 1.3);
        assert_eq!(app.config.speed, Some(1.3));
        assert!(matches!(
            follow_ups.as_slice(),
            [Cmd::Persist(PersistCmd::Config(config))] if config.speed == Some(1.3)
        ));
    }

    #[test]
    fn setting_command_persists_normalize_only_after_filter_admission() {
        let mut app = App::new(50);
        let (resp, cmds) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::Normalize { value: true },
        });

        assert!(resp.ok);
        assert!(!app.audio.normalize);
        assert_eq!(app.config.normalize, None);
        assert!(cmds.iter().all(|cmd| !matches!(cmd, Cmd::Persist(_))));
        assert!(matches!(
            cmds.as_slice(),
            [cmd] if matches!(
                cmd.player_command(),
                Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("dynaudnorm")
            )
        ));

        let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
        assert!(app.audio.normalize);
        assert_eq!(app.config.normalize, Some(true));
        assert!(matches!(
            follow_ups.as_slice(),
            [Cmd::Persist(PersistCmd::Config(config))] if config.normalize == Some(true)
        ));
    }

    #[test]
    fn setting_command_can_toggle_radio_mode_for_tui_owner() {
        let mut app = App::new(50);

        let (resp, cmds) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::RadioMode {
                state: ToggleState::On,
            },
        });
        assert!(resp.ok);
        assert!(!app.radio_dedicated_mode);
        app.admit_player_intents_with_followups_for_test(&cmds);
        assert!(app.radio_dedicated_mode);

        let (_, cmds) = app.apply_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::RadioMode {
                state: ToggleState::Off,
            },
        });
        assert!(app.radio_dedicated_mode);
        app.admit_player_intents_with_followups_for_test(&cmds);
        assert!(!app.radio_dedicated_mode);
    }

    #[test]
    fn resume_session_commits_last_history_track_only_after_admission() {
        let mut app = App::new(50);
        app.library
            .record_play(&Song::remote("id0", "Zero", "A", "3:00"));
        let rev_before = app.queue.rev();

        let (resp, cmds) = app.apply_remote(RemoteCommand::ResumeSession);

        assert!(resp.ok);
        assert!(app.queue.is_empty());
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd.player_command(),
                Some(crate::player::PlayerCmd::Load(url)) if url.contains("id0")
            )
        }));

        let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
        assert_eq!(
            app.queue.current().map(|song| song.video_id.as_str()),
            Some("id0")
        );
        assert_ne!(app.queue.rev(), rev_before);
        assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
        assert!(!follow_ups.is_empty());
    }

    #[test]
    fn rejected_resume_history_load_keeps_live_queue_and_playback_state() {
        let mut app = App::new(50);
        app.library
            .record_play(&Song::remote("id0", "Zero", "A", "3:00"));
        let rev_before = app.queue.rev();
        let epoch_before = app.playback.position_epoch;
        let history_before = app.library.history.len();

        let (_resp, cmds) = app.apply_remote(RemoteCommand::ResumeSession);
        let intent = cmds
            .into_iter()
            .find_map(|cmd| match cmd {
                Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(*intent),
                _ => None,
            })
            .expect("resume must produce a player intent");
        let effects = crate::runtime::player_delivery::settle_player_intent(
            &mut app,
            intent,
            Err(crate::util::delivery::DeliveryError::Busy),
        );

        assert!(effects.is_empty());
        assert!(app.queue.is_empty());
        assert_eq!(app.queue.rev(), rev_before);
        assert_eq!(app.playback.position_epoch, epoch_before);
        assert_eq!(app.prefetch.loaded_video_id, None);
        assert_eq!(app.library.history.len(), history_before);
    }

    #[test]
    fn resume_session_uses_already_restored_queue_before_history() {
        let mut app = App::new(50);
        app.library
            .record_play(&Song::remote("history", "Old History", "A", "3:00"));
        app.queue.set(
            vec![
                Song::remote("restored0", "Restored Zero", "B", "3:00"),
                Song::remote("restored1", "Restored One", "C", "3:00"),
            ],
            1,
        );

        let (resp, cmds) = app.apply_remote(RemoteCommand::ResumeSession);

        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "restored1");
        assert!(app.prefetch.loaded_video_id.is_none());
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd.player_command(),
                Some(crate::player::PlayerCmd::Load(url))
                    if url.contains("restored1") && !url.contains("history")
            )
        }));
        app.admit_player_intents_with_followups_for_test(&cmds);
        assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("restored1"));
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
        let (_pre_admission_resp, cmds) = app.apply_remote(RemoteCommand::VolumeUp);
        assert_eq!(app.playback.volume, before, "volume waits for admission");
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetVolume(volume)) if *volume > before
        )));
        app.admit_player_intents_for_test(&cmds);
        assert!(app.playback.volume > before);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Volume);
        assert!(resp.ok);
        assert!(resp.message.unwrap().contains("volume"));
    }

    #[test]
    fn toggle_pause_defers_state_and_reply_until_admission() {
        let mut app = two_track_app();
        app.prefetch.loaded_video_id = Some("id0".to_owned());
        app.playback.paused = false;

        let (_pre_admission_resp, cmds) = app.apply_remote(RemoteCommand::TogglePause);

        assert!(!app.playback.paused, "pause waits for player admission");
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(true)
        )));
        app.admit_player_intents_for_test(&cmds);
        assert!(app.playback.paused);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Pause);
        assert!(resp.ok);
        assert!(
            resp.message
                .as_deref()
                .is_some_and(|message| message.starts_with("paused:"))
        );
    }

    #[test]
    fn set_volume_clamps_and_applies() {
        let mut app = App::new(50);
        let (_pre_admission_resp, cmds) =
            app.apply_remote(RemoteCommand::SetVolume { percent: 250 });
        assert_eq!(app.playback.volume, 50);
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(100))))
        );
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.volume, 100);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Volume);
        assert!(resp.ok);
        assert_eq!(resp.message.as_deref(), Some("volume 100%"));

        let (_pre_admission_resp, cmds) =
            app.apply_remote(RemoteCommand::SetVolume { percent: 35 });
        assert_eq!(app.playback.volume, 100);
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.volume, 35);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Volume);
        assert_eq!(resp.message.as_deref(), Some("volume 35%"));
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
        let epoch = app.playback.position_epoch;
        let (_pre_admission_resp, cmds) = app.apply_remote(RemoteCommand::SeekTo { ms: 90_000 });
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SeekAbsolute(pos)) if (*pos - 90.0).abs() < 1e-9
        )));
        assert_eq!(app.playback.time_pos, Some(1.0));
        assert_eq!(app.playback.position_epoch, epoch);
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.time_pos, Some(90.0));
        assert_eq!(app.playback.position_epoch, epoch + 1);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Status);
        let snapshot = resp.status.expect("post-admission status snapshot");
        assert_eq!(snapshot.duration_ms, Some(180_000));
        assert!(snapshot.elapsed_ms.is_some_and(|elapsed| elapsed >= 90_000));

        // A seek past the end clamps to the track duration (remote clamps, unlike MPRIS which
        // ignores out-of-range) rather than being dropped.
        let (_pre_admission_resp, cmds) = app.apply_remote(RemoteCommand::SeekTo { ms: 999_000 });
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SeekAbsolute(pos)) if (*pos - 180.0).abs() < 1e-9
        )));
        assert_eq!(app.playback.time_pos, Some(90.0));
        assert_eq!(app.playback.position_epoch, epoch + 1);
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.time_pos, Some(180.0));
        assert_eq!(app.playback.position_epoch, epoch + 2);
        let resp = app.resolve_remote_reply(RemoteReplyPlan::Status);
        assert_eq!(
            resp.status.and_then(|snapshot| snapshot.elapsed_ms),
            Some(180_000)
        );
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
    fn status_artwork_only_matches_current_track() {
        let mut app = two_track_app();
        // Art for a *different* track is not surfaced (mirrors the media snapshot gate).
        app.media_art = Some(crate::media::artwork::MediaArtworkReady {
            key: "id1".to_owned(),
            path: std::path::PathBuf::from("/tmp/id1.jpg"),
        });
        let (resp, _) = app.apply_remote(RemoteCommand::Status);
        assert!(resp.status.expect("status").artwork.is_none());

        app.media_art = Some(crate::media::artwork::MediaArtworkReady {
            key: "id0".to_owned(),
            path: std::path::PathBuf::from("/tmp/id0.jpg"),
        });
        let (resp, _) = app.apply_remote(RemoteCommand::Status);
        let art = resp.status.expect("status").artwork.expect("artwork");
        assert_eq!(art.key, "id0");
        assert_eq!(art.path.as_deref(), Some("/tmp/id0.jpg"));
        assert_eq!(art.mime, None);
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
    fn status_snapshot_sanitizes_persisted_metadata() {
        let mut app = App::new(50);
        let mut song = Song::remote("id0", "Zero", "A", "3:00");
        song.title = format!(
            "{}{}",
            "x".repeat(crate::api::MAX_TITLE_CHARS + 20),
            '\u{202e}'
        );
        song.artist = "A\nB".to_owned();
        song.duration = "9".repeat(crate::api::MAX_DURATION_CHARS + 20);
        app.queue.set(vec![song], 0);

        let (resp, _) = app.apply_remote(RemoteCommand::Status);
        let snap = resp.status.expect("status snapshot present");

        assert_eq!(
            snap.title.as_ref().unwrap().chars().count(),
            crate::api::MAX_TITLE_CHARS
        );
        assert!(!snap.title.as_ref().unwrap().contains('\u{202e}'));
        assert_eq!(snap.artist.as_deref(), Some("AB"));
        assert_eq!(
            snap.queue[0].duration.chars().count(),
            crate::api::MAX_DURATION_CHARS
        );
    }

    #[test]
    fn queue_play_jumps_and_loads_selected_track() {
        let mut app = two_track_app();

        let (resp, cmds) = app.apply_remote(RemoteCommand::QueuePlay { position: 1 });

        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "id0");
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd.player_command(),
                Some(crate::player::PlayerCmd::Load(url)) if url.contains("id1")
            )
        }));
        app.admit_player_intents_with_followups_for_test(&cmds);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
    }

    #[test]
    fn revision_checked_queue_play_accepts_the_rendered_snapshot() {
        let mut app = two_track_app();
        let expected_rev = app.queue.rev();

        let (resp, cmds) = app.apply_remote(RemoteCommand::QueuePlayIfRevision {
            position: 1,
            expected_rev,
        });

        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "id0");
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd.player_command(),
                Some(crate::player::PlayerCmd::Load(url)) if url.contains("id1")
            )
        }));
        app.admit_player_intents_with_followups_for_test(&cmds);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
    }

    #[test]
    fn revision_checked_queue_remove_rejects_a_stale_snapshot_without_mutating() {
        let mut app = two_track_app();
        let rev_before = app.queue.rev();

        let (resp, cmds) = app.apply_remote(RemoteCommand::QueueRemoveIfRevision {
            position: 0,
            expected_rev: u64::MAX,
        });

        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("stale_rev"));
        assert!(cmds.is_empty());
        assert_eq!(app.queue.rev(), rev_before);
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.queue.current().unwrap().video_id, "id0");
    }

    #[test]
    fn queue_remove_current_loads_next_track() {
        let mut app = two_track_app();

        let (resp, mut cmds) = app.apply_remote(RemoteCommand::QueueRemove { position: 0 });

        assert!(resp.ok);
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.queue.current().unwrap().video_id, "id0");
        assert!(cmds.iter().any(|cmd| {
            matches!(
                cmd.player_command(),
                Some(crate::player::PlayerCmd::Load(url)) if url.contains("id1")
            )
        }));
        let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
        cmds.extend(follow_ups);
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
    }

    #[test]
    fn remote_shuffle_and_repeat_persist_modes() {
        let mut app = two_track_app();

        let (shuffle_resp, shuffle_cmds) = app.apply_remote(RemoteCommand::ToggleShuffle);
        assert!(shuffle_resp.ok);
        assert!(app.queue.shuffle);
        assert!(shuffle_cmds.iter().any(|cmd| {
            matches!(cmd, Cmd::Persist(PersistCmd::Config(config)) if config.shuffle == Some(true))
        }));

        let (repeat_resp, repeat_cmds) = app.apply_remote(RemoteCommand::CycleRepeat);
        assert!(repeat_resp.ok);
        assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
        assert!(repeat_cmds.iter().any(|cmd| {
            matches!(cmd, Cmd::Persist(PersistCmd::Config(config)) if config.repeat == crate::queue::Repeat::All)
        }));
    }
}
