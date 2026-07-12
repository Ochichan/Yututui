//! OS media-session command application + snapshot production.
//!
//! Inbound: maps a [`MediaCommand`] (media keys, Now Playing / SMTC / MPRIS surfaces)
//! onto the **same** reducer paths a keypress uses, so OS controls work regardless of
//! the TUI's input mode — exactly like the `ytt -r` remote path. Commands the current
//! state can't honor (Next at the queue end, seek on a live stream) are ignored
//! quietly, since the OS button state was already reported as disabled.
//!
//! Outbound: [`App::media_snapshot`] is the single translation from app state to the
//! platform-independent [`MediaSnapshot`] the adapters publish to the OS.

use std::hash::{Hash, Hasher};
use std::time::Instant;

use super::*;
use crate::media::{
    MediaCaps, MediaCommand, MediaPlaybackStatus, MediaSnapshot, MediaTrack, parse_youtube_video_id,
};
use crate::streaming::candidate::parse_duration_secs;

/// Reducer projection paired with the ordered recorder-clear → Stop player batch. None of the
/// playback, recorder, loaded-track, or video-latch state is changed until the runtime accepts
/// the complete batch.
#[derive(Clone)]
pub struct PlaybackStopPlan {
    expected_queue_rev: u64,
    expected_cursor: usize,
    expected_video_id: Option<String>,
    expected_loaded_video_id: Option<String>,
    recorder: crate::recorder::RecorderTransitionPlan,
}

impl App {
    pub fn media_scrobble_heartbeat_active(&self) -> bool {
        self.queue.current().is_some() && !self.playback.paused
    }

    pub fn media_fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let current = self.queue.current();
        current.map(|song| song.video_id.as_str()).hash(&mut h);
        self.playback.paused.hash(&mut h);
        self.playback.speed.to_bits().hash(&mut h);
        self.playback.volume.hash(&mut h);
        self.playback.position_epoch.hash(&mut h);
        self.queue.shuffle.hash(&mut h);
        match self.queue.repeat {
            crate::queue::Repeat::Off => 0u8,
            crate::queue::Repeat::All => 1,
            crate::queue::Repeat::One => 2,
        }
        .hash(&mut h);
        self.queue.len().hash(&mut h);
        self.queue.position().hash(&mut h);
        self.queue
            .peek_next()
            .map(|song| song.video_id.as_str())
            .hash(&mut h);
        self.media_can_seek().hash(&mut h);
        self.playback.duration.map(f64::to_bits).hash(&mut h);

        if let Some(song) = current {
            let is_live = song.is_radio_station();
            is_live.hash(&mut h);
            self.display_title(song).as_ref().hash(&mut h);
            self.display_artist(song).as_ref().hash(&mut h);
            if is_live {
                if let Some(now) = &self.playback.stream_now_playing {
                    now.title.as_deref().hash(&mut h);
                    now.artist.as_deref().hash(&mut h);
                    now.raw.hash(&mut h);
                }
            } else {
                song.album.as_deref().hash(&mut h);
                song.duration.hash(&mut h);
            }
            song.youtube_id().hash(&mut h);
            song.local_path.hash(&mut h);
            if song.local_path.is_some() && song.youtube_id().is_none() {
                self.library.rev.hash(&mut h);
                self.library_ui.downloaded_rev.hash(&mut h);
                self.library.favorites.len().hash(&mut h);
                self.library.history.len().hash(&mut h);
                self.library_ui.downloaded.len().hash(&mut h);
            }
            self.library.is_favorite(&song.video_id).hash(&mut h);
            self.signals.is_disliked(&song.video_id).hash(&mut h);
            self.media_art
                .as_ref()
                .filter(|art| art.key == song.video_id)
                .map(|art| (&art.key, &art.path))
                .hash(&mut h);
        }

        h.finish()
    }

    /// Apply one OS media-session command, returning effects for the run loop.
    pub(in crate::app) fn apply_media(&mut self, cmd: MediaCommand) -> Vec<Cmd> {
        tracing::debug!(?cmd, "media command");
        match cmd {
            // Idempotent per the spec: Play while playing / Pause while paused no-op.
            MediaCommand::Play => {
                if self.queue.current().is_some()
                    && (self.playback.paused || self.current_needs_load())
                {
                    return self.on_player_action(Action::TogglePause);
                }
                Vec::new()
            }
            MediaCommand::Pause => {
                if self.queue.current().is_some()
                    && !self.playback.paused
                    && !self.current_needs_load()
                {
                    return self.on_player_action(Action::TogglePause);
                }
                Vec::new()
            }
            MediaCommand::Toggle => {
                if self.queue.current().is_some() {
                    return self.on_player_action(Action::TogglePause);
                }
                Vec::new()
            }
            MediaCommand::Stop => self.media_stop(),
            MediaCommand::Next => {
                if self.queue.peek_next().is_some() {
                    return self.on_player_action(Action::NextTrack);
                }
                Vec::new()
            }
            MediaCommand::Previous => {
                // Mirrors the TUI key: steps back, or restarts the current track at
                // the front of the queue — the usual media-key Previous semantics.
                if self.queue.current().is_some() {
                    return self.on_player_action(Action::PrevTrack);
                }
                Vec::new()
            }
            MediaCommand::SeekBy(secs) => {
                if !self.media_can_seek() || !secs.is_finite() {
                    return Vec::new();
                }
                let from = self.playback.time_pos.unwrap_or(0.0);
                let mut target = (from + secs).max(0.0);
                if let Some(d) = self.playback.duration {
                    target = target.min(d);
                }
                self.player_intent(
                    "seek_relative",
                    PlayerCmd::SeekRelative(secs),
                    PlayerCommit::Seek {
                        optimistic_position: Some(target),
                    },
                )
            }
            MediaCommand::SeekTo(pos) => {
                // Reject non-finite (a NaN/inf `SetPosition` would poison `time_pos`) and
                // negatives — matching the daemon's `pos.is_finite() && pos >= 0.0` guard.
                if !self.media_can_seek() || !pos.is_finite() || pos < 0.0 {
                    return Vec::new();
                }
                // Out-of-range SetPosition is ignored per the MPRIS spec (a stale
                // scrubber drag for a longer, already-gone track).
                if let Some(d) = self.playback.duration
                    && pos > d + 0.5
                {
                    return Vec::new();
                }
                self.player_intent(
                    "seek_absolute",
                    PlayerCmd::SeekAbsolute(pos),
                    PlayerCommit::Seek {
                        optimistic_position: Some(pos),
                    },
                )
            }
            MediaCommand::SetShuffle(on) => {
                // On a live radio stream shuffle is meaningless (the TUI slot shows the
                // live-sync state instead) — an OS widget toggle must not mutate it.
                if self.current_is_radio_stream() || self.queue.shuffle == on {
                    return Vec::new();
                }
                self.queue.set_shuffle(on);
                self.dirty = true;
                vec![self.save_playback_modes_cmd()]
            }
            MediaCommand::SetRepeat(mode) => {
                // Same radio guard as SetShuffle — and never remote-trigger a re-sync. Also
                // enforce the music-mode invariant: an OS widget can't enable repeat while
                // autoplay streaming is on.
                if self.current_is_radio_stream()
                    || self.queue.repeat == mode
                    || mode.set_blocked_by_streaming(self.autoplay_streaming)
                {
                    return Vec::new();
                }
                self.queue.repeat = mode;
                self.dirty = true;
                vec![self.save_playback_modes_cmd()]
            }
            MediaCommand::SetVolume(v) => {
                // A non-finite MPRIS Volume write is ignored rather than silently muting
                // (`NaN.clamp(0,1)*100` rounds to 0). Shared 0..1→percent map with the daemon.
                let Some(volume) = crate::playback_policy::volume_percent_from_unit(v) else {
                    return Vec::new();
                };
                if volume == self.playback.volume {
                    return Vec::new();
                }
                self.player_intent(
                    "set_volume",
                    PlayerCmd::SetVolume(volume),
                    PlayerCommit::Volume {
                        volume,
                        pre_mute_volume: None,
                    },
                )
            }
            MediaCommand::SetRate(rate) => {
                // MPRIS: writing 0.0 to Rate must act as Pause.
                if rate == 0.0 {
                    return self.apply_media(MediaCommand::Pause);
                }
                // Shared with the daemon (`clamp_speed`): rounds/clamps and normalizes a
                // non-finite rate to 1.0 so a stray NaN/inf can't poison `playback.speed`.
                let speed = crate::config::clamp_speed(rate);
                if (speed - self.playback.speed).abs() < f64::EPSILON {
                    return Vec::new();
                }
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
            MediaCommand::Like => self.media_set_rating(true),
            MediaCommand::Dislike => self.media_set_rating(false),
            MediaCommand::OpenUri(uri) => self.media_open_uri(&uri),
            MediaCommand::Quit => self.quit_app(),
        }
    }

    fn media_can_seek(&self) -> bool {
        self.prefetch.loaded_video_id.is_some()
            && self
                .queue
                .current()
                .is_some_and(|song| !song.is_radio_station())
    }

    /// MPRIS `Stop`: halt playback but keep the queue and current track, so a later
    /// Play restarts the track from the top. (The TUI itself has no Stop control —
    /// this exists purely for the OS media surface.)
    fn media_stop(&self) -> Vec<Cmd> {
        if self.queue.current().is_none() && self.prefetch.loaded_video_id.is_none() {
            return Vec::new();
        }
        let recorder = self.prepare_recorder_teardown();
        let mut commands = self.recorder_transition_commands(&recorder);
        commands.push(PlayerCmd::Stop);
        let plan = PlaybackStopPlan {
            expected_queue_rev: self.queue.rev(),
            expected_cursor: self.queue.cursor_pos(),
            expected_video_id: self.queue.current().map(|song| song.video_id.clone()),
            expected_loaded_video_id: self.prefetch.loaded_video_id.clone(),
            recorder,
        };
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch("media_stop", commands, PlayerCommit::Stop(Box::new(plan))),
        )))]
    }

    pub(in crate::app) fn media_stop_is_current(&self, plan: &PlaybackStopPlan) -> bool {
        self.queue.planned_transition_matches(
            plan.expected_queue_rev,
            plan.expected_cursor,
            plan.expected_video_id.as_deref(),
            plan.expected_video_id
                .as_ref()
                .map(|_| plan.expected_cursor),
        ) && self.prefetch.loaded_video_id == plan.expected_loaded_video_id
            && self.recorder_transition_is_current(&plan.recorder)
    }

    pub(in crate::app) fn commit_media_stop(&mut self, plan: PlaybackStopPlan) -> Vec<Cmd> {
        let PlaybackStopPlan {
            expected_queue_rev,
            expected_cursor,
            expected_video_id,
            expected_loaded_video_id,
            recorder,
        } = plan;
        self.queue.validate_planned_transition(
            expected_queue_rev,
            expected_cursor,
            expected_video_id.as_deref(),
            expected_video_id.as_ref().map(|_| expected_cursor),
        );
        assert_eq!(
            self.prefetch.loaded_video_id, expected_loaded_video_id,
            "loaded track changed before media Stop commit"
        );
        self.validate_recorder_transition(&recorder);

        let effects = self.commit_recorder_transition(recorder);
        self.playback.paused = true;
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::Stop);
        self.playback.stream_now_playing = None;
        self.anim.last_shown_sec = -1;
        // Dropping the loaded id makes the next play action reload from the start
        // instead of just unpausing mid-track.
        self.prefetch.loaded_video_id = None;
        self.video.paused_audio = false;
        self.dirty = true;
        effects
    }

    /// macOS like/dislike commands: a targeted set/clear of the same tri-state the
    /// `f` key cycles (favorite ↔ dislike, mutually exclusive), with the same signal
    /// bookkeeping so the streaming engine learns from OS-widget feedback too.
    fn media_set_rating(&mut self, like: bool) -> Vec<Cmd> {
        let Some(song) = self.queue.current().cloned() else {
            return Vec::new();
        };
        if song.is_radio_station() {
            // Stations only have favorite membership; a dislike has no meaning.
            if like {
                self.library.toggle_favorite(&song);
                self.dirty = true;
                return vec![Cmd::Persist(PersistCmd::Library)];
            }
            return Vec::new();
        }
        let artist_key = signals::normalize_artist(&song.artist);
        let now = signals::unix_now();
        let liked = self.library.is_favorite(&song.video_id);
        let disliked = self.signals.is_disliked(&song.video_id);
        self.dirty = true;
        if like {
            if liked {
                // Un-like → neutral (undo the affinity lift).
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
                return vec![
                    Cmd::Persist(PersistCmd::Library),
                    Cmd::Persist(PersistCmd::Signals),
                ];
            }
            if disliked {
                self.signals
                    .toggle_dislike(&song.video_id, &artist_key, now);
            }
            let now_fav = self.library.toggle_favorite(&song);
            self.signals
                .record_like(&song.video_id, &artist_key, now_fav, now);
            let comp = self.playback_completion();
            self.record_session_event(&artist_key, Outcome::Like, comp);
            vec![
                Cmd::Persist(PersistCmd::Library),
                Cmd::Persist(PersistCmd::Signals),
            ]
        } else {
            if disliked {
                // Un-dislike → neutral.
                self.signals
                    .toggle_dislike(&song.video_id, &artist_key, now);
                return vec![Cmd::Persist(PersistCmd::Signals)];
            }
            let mut cmds = Vec::new();
            if liked {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
                cmds.push(Cmd::Persist(PersistCmd::Library));
            }
            self.signals
                .toggle_dislike(&song.video_id, &artist_key, now);
            let comp = self.playback_completion();
            self.record_session_event(&artist_key, Outcome::Dislike, comp);
            cmds.push(Cmd::Persist(PersistCmd::Signals));
            cmds
        }
    }

    /// MPRIS `OpenUri`: parse a YouTube / YouTube Music URL and play it now (inserted
    /// after the current track, like every other "play this" gesture). Known tracks
    /// are recovered from favorites/history so the widget shows real metadata.
    fn media_open_uri(&mut self, uri: &str) -> Vec<Cmd> {
        let Some(id) = parse_youtube_video_id(uri) else {
            tracing::debug!(uri, "media OpenUri ignored: not a recognizable YouTube URL");
            return Vec::new();
        };
        let song = self
            .library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .find(|s| s.youtube_id() == Some(id.as_str()))
            .cloned()
            .unwrap_or_else(|| Song::remote(id.clone(), format!("YouTube {id}"), "", ""));
        self.play_now(song)
    }

    /// Build the point-in-time media-session snapshot from app state. Called by the
    /// run loop after every reducer turn; the session facade diffs it and forwards
    /// only real changes to the OS.
    pub fn media_snapshot(&self) -> MediaSnapshot {
        let current = self.queue.current();
        let track = current.map(|song| {
            let is_live = song.is_radio_station();
            // Live radio: surface the stream's own now-playing metadata when the
            // station exposes it (title = the song on air, artist = the station),
            // mirroring what the player view shows.
            let (title, artist) = match (&self.playback.stream_now_playing, is_live) {
                (Some(now), true) => {
                    let station = self.display_title(song).into_owned();
                    match (&now.title, &now.artist) {
                        (Some(t), Some(a)) => (t.clone(), format!("{a} — {station}")),
                        (Some(t), None) => (t.clone(), station),
                        _ => (now.raw.clone(), station),
                    }
                }
                _ => (
                    self.display_title(song).into_owned(),
                    self.display_artist(song).into_owned(),
                ),
            };
            let duration = if is_live {
                None
            } else {
                // mpv's reported duration is authoritative once the track opens;
                // until then fall back to the catalog's "3:45"-style string.
                self.playback
                    .duration
                    .filter(|d| *d > 0.0)
                    .or_else(|| parse_duration_secs(&song.duration).map(f64::from))
            };
            let youtube_id = self.recover_youtube_id(song);
            let url = youtube_id
                .as_deref()
                .map(|id| format!("https://music.youtube.com/watch?v={id}"));
            // Prefer embedded tag art for local files (no network); otherwise the
            // YouTube thumbnail. Radio stations have no artwork source.
            let art_query = match (&song.local_path, &youtube_id) {
                (Some(path), _) => Some(crate::media::artwork::ArtQuery::LocalFile(path.clone())),
                (None, Some(id)) if !is_live => {
                    Some(crate::media::artwork::ArtQuery::Youtube { id: id.clone() })
                }
                _ => None,
            };
            MediaTrack {
                key: song.video_id.clone(),
                title,
                artist,
                album: if is_live { None } else { song.album.clone() },
                duration,
                is_live,
                url,
                art_remote_url: youtube_id
                    .as_deref()
                    .filter(|_| !is_live)
                    .map(crate::media::artwork::remote_thumbnail_url),
                art_file: self
                    .media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| art.path.clone()),
                art_query,
                liked: self.library.is_favorite(&song.video_id),
                disliked: self.signals.is_disliked(&song.video_id),
            }
        });
        let status = if track.is_none() {
            MediaPlaybackStatus::Stopped
        } else if self.playback.paused {
            MediaPlaybackStatus::Paused
        } else {
            MediaPlaybackStatus::Playing
        };
        let caps = MediaCaps {
            can_next: self.queue.peek_next().is_some(),
            can_previous: track.is_some(),
            can_play: track.is_some(),
            can_pause: track.is_some(),
            can_seek: self.media_can_seek() && track.as_ref().is_some_and(|t| t.duration.is_some()),
        };
        MediaSnapshot {
            track,
            status,
            position: self.playback.time_pos.unwrap_or(0.0),
            captured_at: self.playback.time_pos_at.unwrap_or_else(Instant::now),
            rate: self.playback.speed,
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
            volume: (self.playback.volume as f64 / 100.0).clamp(0.0, 1.0),
            caps,
            position_epoch: self.playback.position_epoch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;
    use crate::media::MediaCommand;
    use crate::queue::Repeat;

    fn app_with_queue(n: usize) -> App {
        let mut app = App::new(50);
        app.queue.set(
            (0..n)
                .map(|i| Song::remote(format!("id{i}"), format!("Track {i}"), "Artist", "3:00"))
                .collect(),
            0,
        );
        app
    }

    /// Simulate the track being loaded into mpv (media seeks require a loaded track).
    fn mark_loaded(app: &mut App) {
        let id = app.queue.current().unwrap().video_id.clone();
        app.prefetch.loaded_video_id = Some(id);
        app.playback.duration = Some(180.0);
        app.playback.time_pos = Some(10.0);
        app.playback.paused = false;
    }

    #[test]
    fn media_fingerprint_ignores_non_media_selection_state() {
        let mut app = app_with_queue(2);
        mark_loaded(&mut app);
        let before = app.media_fingerprint();

        app.library_ui.selected = 1;
        app.search.selected = 1;
        app.queue_popup.cursor = 1;
        app.queue_popup.anchor = 1;

        assert_eq!(app.media_fingerprint(), before);
    }

    #[test]
    fn media_fingerprint_tracks_position_epochs_not_time_pos_ticks() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let before = app.media_fingerprint();

        app.playback.time_pos = Some(11.0);
        app.playback.time_pos_at = Some(Instant::now());
        assert_eq!(app.media_fingerprint(), before);

        app.bump_position_epoch(PositionEpochReason::Seek);
        assert_ne!(app.media_fingerprint(), before);
    }

    #[test]
    fn play_pause_are_idempotent() {
        let mut app = app_with_queue(2);
        mark_loaded(&mut app);
        // Playing → Play is a no-op (no toggle command emitted).
        assert!(app.update(Msg::Media(MediaCommand::Play)).is_empty());
        assert!(!app.playback.paused);
        // Playing → Pause toggles.
        let cmds = app.update(Msg::Media(MediaCommand::Pause));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(true)
        )));
        assert!(!app.playback.paused, "pause waits for player admission");
        app.admit_player_intents_for_test(&cmds);
        assert!(app.playback.paused);
        // Paused → Pause is a no-op.
        assert!(app.update(Msg::Media(MediaCommand::Pause)).is_empty());
        // Paused → Play resumes.
        let cmds = app.update(Msg::Media(MediaCommand::Play));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(false)
        )));
        assert!(app.playback.paused, "resume waits for player admission");
        app.admit_player_intents_for_test(&cmds);
        assert!(!app.playback.paused);
    }

    #[test]
    fn play_on_seeded_track_loads_it() {
        let mut app = app_with_queue(1);
        // Seeded but never loaded (restored session): Play must load, not unpause.
        app.playback.paused = true;
        let cmds = app.update(Msg::Media(MediaCommand::Play));
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::Load(_))))
        );
    }

    #[test]
    fn transport_ignored_on_empty_queue() {
        let mut app = App::new(50);
        for cmd in [
            MediaCommand::Play,
            MediaCommand::Pause,
            MediaCommand::Toggle,
            MediaCommand::Next,
            MediaCommand::Previous,
            MediaCommand::Stop,
            MediaCommand::Like,
            MediaCommand::SeekTo(10.0),
        ] {
            assert!(app.update(Msg::Media(cmd)).is_empty());
        }
    }

    #[test]
    fn set_volume_ignores_non_finite_and_clamps_range() {
        let mut app = app_with_queue(1);
        app.playback.volume = 50;
        // A NaN/inf MPRIS Volume write must be ignored, not silently mute (the bug:
        // `NaN.clamp(0,1)*100` rounds to 0).
        assert!(
            app.update(Msg::Media(MediaCommand::SetVolume(f64::NAN)))
                .is_empty()
        );
        assert_eq!(app.playback.volume, 50, "NaN volume write must not mute");
        assert!(
            app.update(Msg::Media(MediaCommand::SetVolume(f64::INFINITY)))
                .is_empty()
        );
        assert_eq!(app.playback.volume, 50);
        // A valid unit maps to percent and emits the player command.
        let cmds = app.update(Msg::Media(MediaCommand::SetVolume(0.3)));
        assert_eq!(app.playback.volume, 50, "volume waits for player admission");
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(30))))
        );
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.volume, 30);
        // A finite out-of-range unit clamps into the band rather than overflowing.
        let cmds = app.update(Msg::Media(MediaCommand::SetVolume(9.0)));
        assert_eq!(
            app.playback.volume, 30,
            "clamped write still waits for admission"
        );
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.volume, 100);
    }

    #[test]
    fn next_ignored_at_queue_end_without_repeat() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        assert!(app.update(Msg::Media(MediaCommand::Next)).is_empty());
        // With repeat-all the queue wraps, so Next works again.
        app.queue.repeat = Repeat::All;
        let cmds = app.update(Msg::Media(MediaCommand::Next));
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::Load(_))))
        );
    }

    #[test]
    fn seek_to_updates_position_and_bumps_epoch() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let epoch = app.playback.position_epoch;
        let cmds = app.update(Msg::Media(MediaCommand::SeekTo(42.0)));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SeekAbsolute(pos)) if (*pos - 42.0).abs() < 1e-9
        )));
        assert_eq!(app.playback.time_pos, Some(10.0));
        assert_eq!(
            app.playback.position_epoch, epoch,
            "seek state and epoch wait for player admission"
        );
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.time_pos, Some(42.0));
        assert_eq!(app.playback.position_epoch, epoch + 1);
    }

    #[test]
    fn seek_to_out_of_range_is_ignored() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        assert!(
            app.update(Msg::Media(MediaCommand::SeekTo(500.0)))
                .is_empty()
        );
        assert_eq!(app.playback.time_pos, Some(10.0));
    }

    #[test]
    fn non_finite_rate_and_seek_do_not_poison_playback_state() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let pos_before = app.playback.time_pos;

        // A NaN Rate normalizes to a finite speed (via clamp_speed) instead of NaN.
        let _ = app.update(Msg::Media(MediaCommand::SetRate(f64::NAN)));
        assert!(app.playback.speed.is_finite());

        // Non-finite SetPosition / Seek are ignored; time_pos stays finite (unchanged).
        assert!(
            app.update(Msg::Media(MediaCommand::SeekTo(f64::NAN)))
                .is_empty()
        );
        assert!(
            app.update(Msg::Media(MediaCommand::SeekBy(f64::INFINITY)))
                .is_empty()
        );
        assert_eq!(app.playback.time_pos, pos_before);
    }

    #[test]
    fn seek_ignored_for_live_radio() {
        let mut app = App::new(50);
        let mut station = Song::remote("radio1", "Some FM", "", "");
        station.playable = Some(crate::api::PlayableRef::RadioStream {
            url: "https://radio.example/stream".to_owned(),
        });
        app.queue.set(vec![station], 0);
        app.prefetch.loaded_video_id = Some("radio1".to_owned());
        assert!(
            app.update(Msg::Media(MediaCommand::SeekBy(10.0)))
                .is_empty()
        );
        assert!(
            app.update(Msg::Media(MediaCommand::SeekTo(10.0)))
                .is_empty()
        );
        assert!(!app.media_snapshot().caps.can_seek);
        assert!(app.media_snapshot().track.unwrap().is_live);
    }

    #[test]
    fn shuffle_repeat_ignored_for_live_radio() {
        let mut app = App::new(50);
        let mut station = Song::remote("radio1", "Some FM", "", "");
        station.playable = Some(crate::api::PlayableRef::RadioStream {
            url: "https://radio.example/stream".to_owned(),
        });
        app.queue.set(vec![station], 0);
        // The TUI reinterprets these slots as live-sync controls on radio; an OS widget
        // toggle must neither mutate queue modes nor trigger a re-sync.
        assert!(
            app.update(Msg::Media(MediaCommand::SetShuffle(true)))
                .is_empty()
        );
        assert!(!app.queue.shuffle);
        assert!(
            app.update(Msg::Media(MediaCommand::SetRepeat(Repeat::All)))
                .is_empty()
        );
        assert_eq!(app.queue.repeat, Repeat::Off);
    }

    #[test]
    fn shuffle_and_repeat_set_explicitly_and_persist() {
        let mut app = app_with_queue(3);
        let cmds = app.update(Msg::Media(MediaCommand::SetShuffle(true)));
        assert!(app.queue.shuffle);
        assert!(cmds.iter().any(
            |c| matches!(c, Cmd::Persist(PersistCmd::Config(cfg)) if cfg.shuffle == Some(true))
        ));
        // Same value again → no-op, no config churn.
        assert!(
            app.update(Msg::Media(MediaCommand::SetShuffle(true)))
                .is_empty()
        );

        let cmds = app.update(Msg::Media(MediaCommand::SetRepeat(Repeat::One)));
        assert_eq!(app.queue.repeat, Repeat::One);
        assert!(cmds.iter().any(
            |c| matches!(c, Cmd::Persist(PersistCmd::Config(cfg)) if cfg.repeat == Repeat::One)
        ));
    }

    #[test]
    fn volume_maps_unit_range_to_percent() {
        let mut app = app_with_queue(1);
        let cmds = app.update(Msg::Media(MediaCommand::SetVolume(0.37)));
        assert_eq!(app.playback.volume, 50);
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(37))))
        );
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.volume, 37);
        // Out-of-range writes clamp (MPRIS spec: negative → 0).
        let cmds = app.update(Msg::Media(MediaCommand::SetVolume(-3.0)));
        assert_eq!(app.playback.volume, 37);
        app.admit_player_intents_for_test(&cmds);
        assert_eq!(app.playback.volume, 0);
    }

    #[test]
    fn rate_zero_pauses_and_rate_sets_speed() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let cmds = app.update(Msg::Media(MediaCommand::SetRate(0.0)));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(true)
        )));
        assert!(!app.playback.paused);
        app.admit_player_intents_for_test(&cmds);
        assert!(app.playback.paused);
        let cmds = app.update(Msg::Media(MediaCommand::SetRate(1.5)));
        assert!((app.playback.speed - 1.0).abs() < 1e-9);
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, .. }) if name == "speed"
        )));
        app.admit_player_intents_for_test(&cmds);
        assert!((app.playback.speed - 1.5).abs() < 1e-9);
        // Clamped to the app's speed range.
        let cmds = app.update(Msg::Media(MediaCommand::SetRate(9.0)));
        assert!((app.playback.speed - 1.5).abs() < 1e-9);
        app.admit_player_intents_for_test(&cmds);
        assert!(app.playback.speed <= crate::config::SPEED_MAX);
    }

    #[test]
    fn like_toggles_favorite_and_clears_dislike() {
        let mut app = app_with_queue(1);
        let id = app.queue.current().unwrap().video_id.clone();
        // Start disliked → Like clears the dislike and favorites the track.
        app.update(Msg::Media(MediaCommand::Dislike));
        assert!(app.signals.is_disliked(&id));
        app.update(Msg::Media(MediaCommand::Like));
        assert!(app.library.is_favorite(&id));
        assert!(!app.signals.is_disliked(&id));
        // Like again → back to neutral.
        app.update(Msg::Media(MediaCommand::Like));
        assert!(!app.library.is_favorite(&id));
    }

    #[test]
    fn dislike_clears_favorite() {
        let mut app = app_with_queue(1);
        let id = app.queue.current().unwrap().video_id.clone();
        app.update(Msg::Media(MediaCommand::Like));
        assert!(app.library.is_favorite(&id));
        app.update(Msg::Media(MediaCommand::Dislike));
        assert!(!app.library.is_favorite(&id));
        assert!(app.signals.is_disliked(&id));
        app.update(Msg::Media(MediaCommand::Dislike));
        assert!(!app.signals.is_disliked(&id));
    }

    #[test]
    fn stop_keeps_queue_but_unloads() {
        let mut app = app_with_queue(2);
        mark_loaded(&mut app);
        let epoch = app.playback.position_epoch;
        let cmds = app.update(Msg::Media(MediaCommand::Stop));
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::Stop)))
        );
        assert!(!app.playback.paused, "Stop waits for player admission");
        assert_eq!(app.playback.time_pos, Some(10.0));
        assert_eq!(app.playback.position_epoch, epoch);
        assert!(!app.current_needs_load());
        app.admit_player_intents_for_test(&cmds);
        assert!(app.playback.paused);
        assert_eq!(app.playback.time_pos, None);
        assert_eq!(app.playback.position_epoch, epoch.wrapping_add(1));
        assert_eq!(app.queue.len(), 2);
        assert!(app.current_needs_load());
        // Play after Stop reloads from the start.
        let cmds = app.update(Msg::Media(MediaCommand::Play));
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::Load(_))))
        );
    }

    #[test]
    fn open_uri_plays_parsed_video() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let cmds = app.update(Msg::Media(MediaCommand::OpenUri(
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share".to_owned(),
        )));
        let _follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
        assert_eq!(app.queue.current().unwrap().video_id, "dQw4w9WgXcQ");
        assert!(cmds.iter().any(|cmd| matches!(
            cmd.player_command(),
            Some(PlayerCmd::Load(url)) if url.contains("dQw4w9WgXcQ")
        )));
        // The rest of the queue is preserved (play-now semantics).
        assert_eq!(app.queue.len(), 2);
    }

    #[test]
    fn open_uri_rejects_foreign_urls() {
        let mut app = app_with_queue(1);
        for uri in [
            "https://example.com/watch?v=dQw4w9WgXcQ",
            "not a url",
            "https://youtube.com/playlist?list=abc",
        ] {
            assert!(
                app.update(Msg::Media(MediaCommand::OpenUri(uri.to_owned())))
                    .is_empty()
            );
        }
    }

    #[test]
    fn parse_video_id_variants() {
        for uri in [
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10",
            "https://youtube.com/watch?feature=x&v=dQw4w9WgXcQ",
            "http://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            // New forms recognized by the widened parser.
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://music.youtube.com/embed/dQw4w9WgXcQ?rel=0",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            // Host case-folding, trailing-dot, and scheme case-folding.
            "https://WWW.YOUTUBE.COM/watch?v=dQw4w9WgXcQ",
            "https://youtube.com./watch?v=dQw4w9WgXcQ",
            "HTTPS://www.youtube.com/watch?v=dQw4w9WgXcQ",
            // watch + list resolves to the VIDEO (user's choice), not the playlist.
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabcdef",
        ] {
            assert_eq!(
                parse_youtube_video_id(uri).as_deref(),
                Some("dQw4w9WgXcQ"),
                "{uri}"
            );
        }
        assert_eq!(parse_youtube_video_id("https://youtu.be/x"), None);
        // Negatives that must stay None.
        for uri in [
            "https://example.com/shorts/dQw4w9WgXcQ", // wrong host
            "https://www.youtube-nocookie.com/watch?v=dQw4w9WgXcQ", // nocookie only via /embed/
            "https://www.youtube.com/shorts/-UfI1X-MSighttps://youtu.be/x", // concatenated paste
        ] {
            assert_eq!(parse_youtube_video_id(uri), None, "{uri}");
        }
        // watch+list stays a video for the id parser AND is not a playlist for the playlist parser.
        assert_eq!(
            crate::media::parse_youtube_playlist_id(
                "https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PLabcdef"
            ),
            None
        );
    }

    #[test]
    fn snapshot_reflects_transport_and_caps() {
        let mut app = app_with_queue(2);
        // A restored (seeded, never loaded) session parks paused.
        app.playback.paused = true;
        let snap = app.media_snapshot();
        // Seeded but paused/not loaded → Paused with metadata (resume-friendly).
        assert_eq!(snap.status, MediaPlaybackStatus::Paused);
        let track = snap.track.as_ref().unwrap();
        assert_eq!(track.title, "Track 0");
        assert_eq!(track.artist, "Artist");
        // Catalog duration string parses until mpv reports the real length.
        assert_eq!(track.duration, Some(180.0));
        assert!(snap.caps.can_next && snap.caps.can_previous && snap.caps.can_play);
        assert!(!snap.caps.can_seek); // not loaded yet

        mark_loaded(&mut app);
        let snap = app.media_snapshot();
        assert_eq!(snap.status, MediaPlaybackStatus::Playing);
        assert!(snap.caps.can_seek);
        assert!((snap.position - 10.0).abs() < 1e-9);
        assert_eq!(
            snap.track.as_ref().unwrap().url.as_deref(),
            Some("https://music.youtube.com/watch?v=id0")
        );
    }

    #[test]
    fn snapshot_idle_is_stopped_without_track() {
        let app = App::new(50);
        let snap = app.media_snapshot();
        assert!(snap.track.is_none());
        assert_eq!(snap.status, MediaPlaybackStatus::Stopped);
        assert!(!snap.caps.can_play);
    }

    #[test]
    fn snapshot_artwork_only_matches_current_track() {
        let mut app = app_with_queue(2);
        app.media_art = Some(crate::media::artwork::MediaArtworkReady {
            key: "id1".to_owned(),
            path: std::path::PathBuf::from("/tmp/id1.jpg"),
        });
        // Art for a *different* track is not surfaced.
        assert!(app.media_snapshot().track.unwrap().art_file.is_none());
        app.media_art = Some(crate::media::artwork::MediaArtworkReady {
            key: "id0".to_owned(),
            path: std::path::PathBuf::from("/tmp/id0.jpg"),
        });
        assert!(app.media_snapshot().track.unwrap().art_file.is_some());
    }

    #[test]
    fn pause_flip_rebases_position_clock() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        app.playback.time_pos_at = None;
        app.update(PlayerMsg::Paused(true));
        assert!(
            app.playback.time_pos_at.is_some(),
            "pause flip rebases the clock"
        );
    }
}
