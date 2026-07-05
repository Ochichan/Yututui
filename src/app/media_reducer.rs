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

use std::time::Instant;

use super::*;
use crate::media::{
    MediaCaps, MediaCommand, MediaPlaybackStatus, MediaSnapshot, MediaTrack, parse_youtube_video_id,
};
use crate::streaming::candidate::parse_duration_secs;

impl App {
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
                if !self.media_can_seek() {
                    return Vec::new();
                }
                // Optimistic position (like the pause toggle): the snapshot pushed
                // right after this turn already carries the target, so the OS
                // progress bar doesn't briefly rubber-band to the stale position.
                // mpv confirms via its next `time-pos` report. Seeking past the end
                // lets mpv hit EOF → the queue advances, per the MPRIS spec.
                let from = self.playback.time_pos.unwrap_or(0.0);
                let mut target = (from + secs).max(0.0);
                if let Some(d) = self.playback.duration {
                    target = target.min(d);
                }
                self.playback.time_pos = Some(target);
                self.playback.time_pos_at = Some(Instant::now());
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SeekRelative(secs))]
            }
            MediaCommand::SeekTo(pos) => {
                if !self.media_can_seek() || pos < 0.0 {
                    return Vec::new();
                }
                // Out-of-range SetPosition is ignored per the MPRIS spec (a stale
                // scrubber drag for a longer, already-gone track).
                if let Some(d) = self.playback.duration
                    && pos > d + 0.5
                {
                    return Vec::new();
                }
                self.playback.time_pos = Some(pos);
                self.playback.time_pos_at = Some(Instant::now());
                // No epoch bump here: `App::update` bumps it centrally for every turn
                // that emits a seek command, this one included.
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SeekAbsolute(pos))]
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
                    || (mode != crate::queue::Repeat::Off && self.autoplay_streaming)
                {
                    return Vec::new();
                }
                self.queue.repeat = mode;
                self.dirty = true;
                vec![self.save_playback_modes_cmd()]
            }
            MediaCommand::SetVolume(v) => {
                let volume = (v.clamp(0.0, 1.0) * 100.0).round() as i64;
                if volume == self.playback.volume {
                    return Vec::new();
                }
                self.playback.volume = volume;
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(volume))]
            }
            MediaCommand::SetRate(rate) => {
                // MPRIS: writing 0.0 to Rate must act as Pause.
                if rate == 0.0 {
                    return self.apply_media(MediaCommand::Pause);
                }
                let speed = ((rate * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX);
                if (speed - self.playback.speed).abs() < f64::EPSILON {
                    return Vec::new();
                }
                self.playback.speed = speed;
                self.status.kind = StatusKind::Info;
                self.status.text = format!("{}: {speed:.1}x", t!("Speed", "재생 속도"));
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetProperty {
                    name: "speed".to_owned(),
                    value: serde_json::Value::from(speed),
                })]
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
    fn media_stop(&mut self) -> Vec<Cmd> {
        if self.queue.current().is_none() && self.prefetch.loaded_video_id.is_none() {
            return Vec::new();
        }
        self.playback.paused = true;
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
        self.playback.stream_now_playing = None;
        self.last_shown_sec = -1;
        // Dropping the loaded id makes the next play action reload from the start
        // instead of just unpausing mid-track.
        self.prefetch.loaded_video_id = None;
        self.video.paused_audio = false;
        self.dirty = true;
        // Cut any in-progress radio recording before stopping mpv (mid-song → dropped).
        let mut cmds = self.recorder_teardown();
        cmds.push(Cmd::Player(PlayerCmd::Stop));
        cmds
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
                return vec![Cmd::SaveLibrary];
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
                return vec![Cmd::SaveLibrary, Cmd::SaveSignals];
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
            vec![Cmd::SaveLibrary, Cmd::SaveSignals]
        } else {
            if disliked {
                // Un-dislike → neutral.
                self.signals
                    .toggle_dislike(&song.video_id, &artist_key, now);
                return vec![Cmd::SaveSignals];
            }
            let mut cmds = Vec::new();
            if liked {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
                cmds.push(Cmd::SaveLibrary);
            }
            self.signals
                .toggle_dislike(&song.video_id, &artist_key, now);
            let comp = self.playback_completion();
            self.record_session_event(&artist_key, Outcome::Dislike, comp);
            cmds.push(Cmd::SaveSignals);
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
    fn play_pause_are_idempotent() {
        let mut app = app_with_queue(2);
        mark_loaded(&mut app);
        // Playing → Play is a no-op (no toggle command emitted).
        assert!(app.update(Msg::Media(MediaCommand::Play)).is_empty());
        assert!(!app.playback.paused);
        // Playing → Pause toggles.
        let cmds = app.update(Msg::Media(MediaCommand::Pause));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::CyclePause)))
        );
        assert!(app.playback.paused);
        // Paused → Pause is a no-op.
        assert!(app.update(Msg::Media(MediaCommand::Pause)).is_empty());
        // Paused → Play resumes.
        let cmds = app.update(Msg::Media(MediaCommand::Play));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::CyclePause)))
        );
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
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::Load(_))))
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
    fn next_ignored_at_queue_end_without_repeat() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        assert!(app.update(Msg::Media(MediaCommand::Next)).is_empty());
        // With repeat-all the queue wraps, so Next works again.
        app.queue.repeat = Repeat::All;
        let cmds = app.update(Msg::Media(MediaCommand::Next));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::Load(_))))
        );
    }

    #[test]
    fn seek_to_updates_position_and_bumps_epoch() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let epoch = app.playback.position_epoch;
        let cmds = app.update(Msg::Media(MediaCommand::SeekTo(42.0)));
        assert!(cmds.iter().any(
            |c| matches!(c, Cmd::Player(PlayerCmd::SeekAbsolute(p)) if (*p - 42.0).abs() < 1e-9)
        ));
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
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SaveConfig(cfg) if cfg.shuffle == Some(true)))
        );
        // Same value again → no-op, no config churn.
        assert!(
            app.update(Msg::Media(MediaCommand::SetShuffle(true)))
                .is_empty()
        );

        let cmds = app.update(Msg::Media(MediaCommand::SetRepeat(Repeat::One)));
        assert_eq!(app.queue.repeat, Repeat::One);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SaveConfig(cfg) if cfg.repeat == Repeat::One))
        );
    }

    #[test]
    fn volume_maps_unit_range_to_percent() {
        let mut app = app_with_queue(1);
        let cmds = app.update(Msg::Media(MediaCommand::SetVolume(0.37)));
        assert_eq!(app.playback.volume, 37);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::SetVolume(37))))
        );
        // Out-of-range writes clamp (MPRIS spec: negative → 0).
        app.update(Msg::Media(MediaCommand::SetVolume(-3.0)));
        assert_eq!(app.playback.volume, 0);
    }

    #[test]
    fn rate_zero_pauses_and_rate_sets_speed() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let cmds = app.update(Msg::Media(MediaCommand::SetRate(0.0)));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::CyclePause)))
        );
        let cmds = app.update(Msg::Media(MediaCommand::SetRate(1.5)));
        assert!((app.playback.speed - 1.5).abs() < 1e-9);
        assert!(cmds.iter().any(|c| matches!(
            c,
            Cmd::Player(PlayerCmd::SetProperty { name, .. }) if name == "speed"
        )));
        // Clamped to the app's speed range.
        app.update(Msg::Media(MediaCommand::SetRate(9.0)));
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
        let cmds = app.update(Msg::Media(MediaCommand::Stop));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::Stop)))
        );
        assert!(app.playback.paused);
        assert_eq!(app.queue.len(), 2);
        assert!(app.current_needs_load());
        // Play after Stop reloads from the start.
        let cmds = app.update(Msg::Media(MediaCommand::Play));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Player(PlayerCmd::Load(_))))
        );
    }

    #[test]
    fn open_uri_plays_parsed_video() {
        let mut app = app_with_queue(1);
        mark_loaded(&mut app);
        let cmds = app.update(Msg::Media(MediaCommand::OpenUri(
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share".to_owned(),
        )));
        assert_eq!(app.queue.current().unwrap().video_id, "dQw4w9WgXcQ");
        assert!(cmds.iter().any(
            |c| matches!(c, Cmd::Player(PlayerCmd::Load(url)) if url.contains("dQw4w9WgXcQ"))
        ));
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
        ] {
            assert_eq!(
                parse_youtube_video_id(uri).as_deref(),
                Some("dQw4w9WgXcQ"),
                "{uri}"
            );
        }
        assert_eq!(parse_youtube_video_id("https://youtu.be/x"), None);
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
        app.update(Msg::PlayerPaused(true));
        assert!(
            app.playback.time_pos_at.is_some(),
            "pause flip rebases the clock"
        );
    }
}
