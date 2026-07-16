//! OS media-session integration for the headless engine: the command handler the
//! platform media keys drive, its capability probes, and the snapshot projection.
//! Mirrors [`crate::app`]'s `apply_media` half of the dual-owner contract.

use std::time::Instant;

use serde_json::Value;

use super::{DaemonEngine, EngineEffect};
use crate::api::Song;
use crate::config::clamp_speed;
use crate::playback_policy::{PlaybackModeAction, PlaybackModeState};
use crate::player::PlayerCmd;
use crate::signals;

impl DaemonEngine {
    pub fn set_media_art(&mut self, ready: crate::media::artwork::MediaArtworkReady) {
        self.media_art = Some(ready);
    }

    /// Apply one OS media-session command. Returns `(shutdown, effects)`; commands the
    /// current state can't honor are ignored quietly (their buttons were reported
    /// disabled). Mirrors [`crate::app`]'s `apply_media` for the headless engine.
    pub async fn handle_media(
        &mut self,
        cmd: crate::media::MediaCommand,
    ) -> (bool, Vec<EngineEffect>) {
        use crate::media::MediaCommand;
        tracing::debug!(?cmd, "daemon media command");
        let mut effects = Vec::new();
        match cmd {
            MediaCommand::Play => {
                if self.queue.current().is_some() && (self.playback.paused || self.needs_load()) {
                    let _ = self.toggle_pause().await;
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Pause => {
                if !self.playback.paused && !self.needs_load() {
                    let _ = self.toggle_pause().await;
                }
            }
            MediaCommand::Toggle => {
                if self.queue.current().is_some() {
                    let _ = self.toggle_pause().await;
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Stop => {
                if self.queue.current().is_some() || self.loaded_video_id.is_some() {
                    self.stop_playback();
                    self.save_session();
                }
            }
            MediaCommand::Next => {
                if self.queue.peek_next().is_some() {
                    let outgoing = self.prepare_outgoing(false);
                    let response = self.next_track().await;
                    if response.ok
                        && let Some(outgoing) = outgoing
                    {
                        self.commit_outgoing(outgoing);
                    }
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Previous => {
                if self.queue.current().is_some() {
                    let _ = self.prev_track().await;
                }
            }
            MediaCommand::SeekBy(seconds) => {
                if self.media_can_seek() && seconds.is_finite() {
                    let _ = self.seek(seconds);
                }
            }
            MediaCommand::SeekTo(pos) => {
                // `pos.is_finite()` rejects NaN/±inf (a NaN also fails `>= 0.0`, but inf would
                // slip past it); mirrors the App reducer's non-finite guard for parity.
                if self.media_can_seek() && pos.is_finite() && pos >= 0.0 {
                    // Out-of-range SetPosition is ignored per the MPRIS spec.
                    if let Some(d) = self.playback.duration
                        && pos > d + 0.5
                    {
                        return (false, effects);
                    }
                    let _ = self.seek_to(pos);
                }
            }
            MediaCommand::SetShuffle(on) => {
                if !self.current_is_radio_stream() && self.queue.shuffle != on {
                    self.queue.set_shuffle(on);
                    self.config.shuffle = Some(on);
                    self.save_config("daemon shuffle setting");
                    self.save_session();
                }
            }
            MediaCommand::SetRepeat(mode) => {
                // Live-radio parity with the TUI: these UI slots are reinterpreted as live-sync
                // controls, so OS widgets must not mutate shuffle/repeat while a station plays.
                // Music-mode invariant: an OS widget can't enable repeat while streaming is on.
                if !self.current_is_radio_stream() {
                    let transition = PlaybackModeState::new(self.queue.repeat, self.streaming)
                        .transition(PlaybackModeAction::SetRepeat(mode));
                    if let Ok(transition) = transition
                        && transition.changed
                    {
                        self.queue.repeat = transition.state.repeat;
                        self.config.repeat = transition.state.repeat;
                        self.save_config("daemon repeat setting");
                        self.save_session();
                    }
                }
            }
            MediaCommand::SetVolume(v) => {
                // Shared 0..1→percent map with the TUI; a non-finite write is ignored.
                if let Some(volume) = crate::playback_policy::volume_percent_from_unit(v)
                    && volume != self.playback.volume
                {
                    let _ = self.adjust_volume(volume - self.playback.volume);
                }
            }
            MediaCommand::SetRate(rate) => {
                if rate == 0.0 {
                    return Box::pin(self.handle_media(MediaCommand::Pause)).await;
                }
                let speed = clamp_speed(rate);
                if (speed - self.playback.speed).abs() > f64::EPSILON {
                    let delivery = self.send_player_command_if_active(
                        "set_speed",
                        PlayerCmd::SetProperty {
                            name: "speed".to_owned(),
                            value: Value::from(speed),
                        },
                    );
                    if let Err(error) = delivery {
                        self.last_error = Some(error.to_string());
                    } else {
                        self.playback.speed = speed;
                    }
                }
            }
            MediaCommand::Like => self.media_set_rating(true),
            MediaCommand::Dislike => self.media_set_rating(false),
            MediaCommand::OpenUri(uri) => {
                if let Some(id) = crate::media::parse_youtube_video_id(&uri) {
                    let song = self
                        .library
                        .favorites
                        .iter()
                        .chain(self.library.history.iter())
                        .find(|s| s.youtube_id() == Some(id.as_str()))
                        .cloned()
                        .unwrap_or_else(|| {
                            Song::remote(id.clone(), format!("YouTube {id}"), "", "")
                        });
                    let previous = self.queue.snapshot();
                    if self.queue.play_now(song) {
                        if let Err(e) = self.load_current_or_restore_queue(previous).await {
                            self.last_error = Some(e.to_string());
                            self.stop_playback();
                        }
                        effects.extend(self.maybe_autoplay_extend());
                    }
                }
            }
            MediaCommand::Quit => {
                self.stop_playback();
                self.suppress_transport_recovery_for_shutdown();
                self.save_session();
                return (true, effects);
            }
        }
        (false, effects)
    }

    fn needs_load(&self) -> bool {
        self.loaded_video_id.as_deref() != self.queue.current().map(|song| song.video_id.as_str())
    }

    pub(super) fn media_can_seek(&self) -> bool {
        self.loaded_video_id.is_some()
            && self
                .queue
                .current()
                .is_some_and(|song| !song.is_radio_station())
    }

    pub(super) fn current_is_radio_stream(&self) -> bool {
        self.queue
            .current()
            .is_some_and(|song| song.is_radio_station())
    }

    /// Like/dislike from the OS surface: same favorite/dislike bookkeeping the TUI's
    /// rating cycle performs, persisted immediately (the daemon has no Cmd loop).
    fn media_set_rating(&mut self, like: bool) {
        let Some(song) = self.queue.current().cloned() else {
            return;
        };
        if song.is_radio_station() {
            if like {
                self.library.toggle_favorite(&song);
                self.save_library("daemon radio favorite");
                self.library_invalidations = self.library_invalidations.wrapping_add(1);
            }
            return;
        }
        let artist_key = signals::normalize_artist(&song.artist);
        let now = signals::unix_now();
        let liked = self.library.is_favorite(&song.video_id);
        let disliked = self.signals.is_disliked(&song.video_id);
        if like {
            if liked {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
            } else {
                if disliked {
                    self.signals
                        .toggle_dislike(&song.video_id, &artist_key, now);
                }
                let now_fav = self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, now_fav, now);
            }
        } else if disliked {
            self.signals
                .toggle_dislike(&song.video_id, &artist_key, now);
        } else {
            if liked {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
            }
            self.signals
                .toggle_dislike(&song.video_id, &artist_key, now);
        }
        self.save_library("daemon media rating library");
        self.save_signals("daemon media rating signals");
        // Favorites membership changed: a subscribed GUI's paged library view is stale.
        self.library_invalidations = self.library_invalidations.wrapping_add(1);
    }

    /// Build the OS media-session snapshot from engine state (the daemon analog of
    /// the TUI's `App::media_snapshot`).
    pub fn media_snapshot(&self) -> crate::media::MediaSnapshot {
        use crate::media::{MediaCaps, MediaPlaybackStatus, MediaSnapshot, MediaTrack};
        let current = self.queue.current();
        let track = current.map(|song| {
            let is_live = song.is_radio_station();
            let duration = if is_live {
                None
            } else {
                self.playback.duration.filter(|d| *d > 0.0).or_else(|| {
                    crate::streaming::candidate::parse_duration_secs(&song.duration).map(f64::from)
                })
            };
            let youtube_id = song.youtube_id().map(str::to_owned);
            let art_query = match (&song.local_path, &youtube_id) {
                (Some(path), _) => Some(crate::media::artwork::ArtQuery::LocalFile(path.clone())),
                (None, Some(id)) if !is_live => {
                    Some(crate::media::artwork::ArtQuery::Youtube { id: id.clone() })
                }
                _ => None,
            };
            MediaTrack {
                key: song.video_id.clone(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                album: if is_live { None } else { song.album.clone() },
                duration,
                is_live,
                url: youtube_id
                    .as_deref()
                    .map(|id| format!("https://music.youtube.com/watch?v={id}")),
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
        } else if self.playback.paused || self.loaded_video_id.is_none() {
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
