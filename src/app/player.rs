//! Player/playback reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;
use crate::tools::PlaybackFailureClass;

/// A player/playback runtime message: mpv property changes, EOF, playback errors, and
/// video-overlay IPC events. Bucketed under [`Msg::Player`] to keep the flat `Msg` lean.
/// Constructed in `runtime.rs` from the leaf `PlayerEvent`/`VideoEvent`; never imported by a
/// leaf actor (see `scripts/check-architecture.sh`).
pub enum PlayerMsg {
    /// mpv playback position, in seconds.
    TimePos(f64),
    /// Current track duration, in seconds; `None` when mpv reported the property as
    /// unavailable after it had a value (live edge / teardown) — clears the stored length.
    Duration(Option<f64>),
    /// mpv pause state changed.
    Paused(bool),
    /// mpv volume changed (0-100, but mpv can report fractional/over-100 values).
    Volume(f64),
    /// mpv stream metadata changed. Live radio streams often expose ICY now-playing titles here.
    Metadata(serde_json::Value),
    /// mpv `demuxer-cache-time`: the newest demuxed timestamp (≈ the live edge on a radio
    /// stream), or `None` when the property became unavailable.
    CacheTime(Option<f64>),
    /// mpv `audio-codec-name` for the active stream (radio recorder container hint).
    AudioCodec(Option<String>),
    /// mpv `file-format` (container) for the active stream (radio recorder container hint).
    FileFormat(Option<String>),
    /// Latest cross-platform audio endpoint inventory reported by mpv.
    AudioDeviceList(Vec<crate::player::AudioDevice>),
    /// A manual device-list refresh failed without affecting playback.
    AudioDeviceRefreshFailed(String),
    /// The configured mpv `audio-device` value (`None` means system/default auto routing).
    AudioDeviceChanged(Option<String>),
    /// The active audio output driver, or `None` while no real output is open.
    CurrentAudioOutput(Option<String>),
    /// Correlated acknowledgement for a Settings audio-output selection request.
    AudioDeviceSelectionResult {
        correlation_id: u64,
        device: Option<String>,
        result: Result<(), String>,
    },
    /// The current track reached its end.
    Eof,
    /// mpv reported a playback error.
    Error(String),
    /// The mpv IPC transport ended unexpectedly. This is a runtime-lifetime failure, not
    /// a bad track, so recovery must not advance the queue or record skip/play signals.
    TransportClosed(String),
    /// Managed disk-cache safety recycle with an exact, one-shot RAM-only resume contract.
    CacheEmergency {
        position_secs: f64,
        paused: bool,
        reason: crate::player::long_form_seek::CacheReason,
    },
    /// Cache safety recycle for an already-admitted replacement destination.
    CacheReplacementEmergency {
        reason: crate::player::long_form_seek::CacheReason,
    },
    /// The runtime accepted an admission-sensitive command batch; apply its projected state.
    IntentAdmitted(PlayerCommit),
    /// An event from the video-overlay mpv's IPC client, tagged with the spawn
    /// generation it was connected for — the reducer drops events from a window it
    /// already closed (`v`) or respawned (`Shift+V`).
    VideoOverlay {
        generation: u64,
        event: crate::player::video::VideoEvent,
    },
}

impl From<PlayerMsg> for Msg {
    fn from(msg: PlayerMsg) -> Self {
        Msg::Player(msg)
    }
}

/// How far behind mpv's newest demuxed sample still counts as "in sync". A practical
/// at-edge radio stream can sit around 17s behind `demuxer-cache-time`; 20s covers that
/// normal forward buffer plus jitter while still catching real timeshift drift.
pub(in crate::app) const LIVE_SYNC_THRESHOLD_SECS: f64 = 20.0;
/// Re-sync seeks this far short of the newest demuxed data, never into the undemuxed tail.
pub(in crate::app) const LIVE_EDGE_SEEK_MARGIN_SECS: f64 = 2.0;
/// A `demuxer-cache-time` older than this (while playing) no longer proves we're at the
/// edge — mpv stops updating it once the forward buffer saturates.
pub(in crate::app) const CACHE_TIME_STALE_SECS: f64 = 5.0;
/// A second re-sync inside this window while still behind means the seek didn't take
/// (an unseekable live cache), so re-sync escalates to a stream reconnect.
pub(in crate::app) const RESYNC_RETRY_WINDOW_SECS: f64 = 10.0;

/// Memoized result of [`App::recover_youtube_id`]'s library title scan, keyed on the
/// track and the same rev/length fingerprint the library row cache uses.
pub(in crate::app) struct YidMemo {
    pub(in crate::app) video_id: String,
    pub(in crate::app) state_key: (u64, u64, usize, usize, usize),
    pub(in crate::app) result: Option<String>,
}

impl App {
    /// Show the shared music-mode rejection used when a local or media-session
    /// control tries to enable repeat while autoplay streaming is active.
    pub(in crate::app) fn show_repeat_streaming_conflict(&mut self) {
        self.status.text = t!(
            "Can't use repeat while autoplay is on",
            "자동재생 중에는 반복을 켤 수 없어요",
            "自動再生中はリピートをオンにできません"
        )
        .to_owned();
        self.dirty = true;
    }

    /// Show the inverse playback-mode rejection: autoplay cannot be enabled while repeat is on.
    pub(in crate::app) fn show_streaming_repeat_conflict(&mut self) {
        self.status.text = t!(
            "Can't use autoplay while repeat is on",
            "반복 재생 중에는 자동재생을 켤 수 없어요",
            "リピート再生中は自動再生をオンにできません"
        )
        .to_owned();
        self.dirty = true;
    }

    /// Handle an mpv playback error: self-heal a stale-yt-dlp extraction failure
    /// once, otherwise skip the bad track (with a circuit breaker after too many in a
    /// row). Extracted verbatim from the `PlayerMsg::Error` dispatch arm; the
    /// `position_epoch` bump stays in `App::update`.
    pub(in crate::app) fn on_player_error(&mut self, e: String) -> Vec<Cmd> {
        let recoverable_source = crate::player::recovery::classify_source_failure(&e);
        let midtrack_recoverable = recoverable_source.is_some()
            && self
                .playback
                .time_pos
                .is_some_and(|position| position.is_finite() && position > 0.0);
        // Log *which* track failed and whether it came from a (possibly stale)
        // prefetched URL. `e` already carries mpv's own reason (its `file_error`
        // end-file field — the closest thing to a "why": HTTP 403, unsupported, …).
        let failed = self
            .queue
            .current()
            .map(|s| format!("{} — {}", s.title, s.artist));
        if let Some(failure) = recoverable_source {
            // Source errors can contain the full signed URL. The typed family is enough to
            // diagnose the bounded recovery decision without disclosing either source URL.
            tracing::warn!(
                ?failure,
                track = failed.as_deref().unwrap_or("?"),
                prefetched = self.prefetch.last_load_prefetched,
                "playback source error"
            );
        } else {
            tracing::warn!(
                error = %crate::util::sanitize::sanitize_error_text(&e),
                track = failed.as_deref().unwrap_or("?"),
                prefetched = self.prefetch.last_load_prefetched,
                "playback error"
            );
        }
        if midtrack_recoverable && let Some(commands) = self.source_recovery_intent(&e) {
            return commands;
        }
        let failure_class = crate::tools::classify_playback_failure(&e);
        let extraction = failure_class == PlaybackFailureClass::Extraction;
        if !midtrack_recoverable
            && self.prefetch.last_load_prefetched
            && let Some(song) = self.queue.current()
            && let Some(watch_url) = song.prefetch_target()
            && !self.prefetch.watch_retry_attempted.contains(&song.video_id)
        {
            let video_id = song.video_id.clone();
            return self.prefetch_watch_retry_intent(video_id, watch_url);
        }
        // Self-heal: an extraction-shaped failure on a yt-dlp-resolved track is
        // the stale-yt-dlp signature. Update it in the background and retry this
        // track ONCE — via a resolver-resolved direct URL, because the session
        // mpv keeps its spawn-time ytdl_path (see player::mpv::spawn docs).
        // Deliberately does not touch `consecutive_play_errors`: the heal is an
        // extra chance, not a substitute for the circuit breaker.
        if extraction
            && self.heal.pending_video_id.is_none()
            && let Some(song) = self.queue.current()
            && song.prefetch_target().is_some()
            && !self.heal.attempted.contains(&song.video_id)
            && self
                .heal
                .last_check
                .is_none_or(|at| at.elapsed() >= crate::tools::HEAL_COOLDOWN)
        {
            let video_id = song.video_id.clone();
            // Bound the per-track guard set: after enough distinct healed tracks in one
            // long session, reset it (a re-heal is at worst one wasted retry).
            if self.heal.attempted.len() >= crate::playback_policy::HEAL_ATTEMPTED_MAX {
                self.heal.attempted.clear();
            }
            self.heal.attempted.insert(video_id.clone());
            self.heal.last_check = Some(Instant::now());
            self.heal.pending_video_id = Some(video_id.clone());
            // The failed load may itself have been a stale prefetched URL —
            // drop it so the retry resolves fresh with the updated binary.
            self.prefetch.resolved.remove(&video_id);
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Stream resolution failed — updating yt-dlp…",
                "스트림 해석 실패 — yt-dlp 업데이트 중…",
                "ストリーム解決に失敗 — yt-dlp 更新中…"
            )
            .to_owned();
            self.dirty = true;
            return vec![Cmd::YtdlpSelfHeal {
                video_id,
                tools: self.config.tools.clone(),
            }];
        }
        self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
        // A single bad track shouldn't strand the user: skip it and play on. The
        // cursor moves, so the title refreshes to the next track. Bail out once too
        // many fail in a row (offline / bad cookie) so we don't skip-storm.
        if self.consecutive_play_errors <= MAX_CONSECUTIVE_PLAY_ERRORS
            && self.queue.peek_next().is_some()
        {
            // `advance(false)` always moves on (ignores repeat-one), unlike an EOF.
            let mut cmds = self.advance(false);
            self.status.text = playback_error::skipped_status_for_failure(failure_class);
            Self::attach_track_commit_status(
                &mut cmds,
                StatusKind::Error,
                self.status.text.clone(),
            );
            self.dirty = true;
            return cmds;
        }
        self.status.text = if self.consecutive_play_errors > MAX_CONSECUTIVE_PLAY_ERRORS {
            playback_error::breaker_status_for_failure(failure_class)
        } else {
            playback_error::playback_error_status_for_failure(failure_class, &e)
        };
        self.dirty = true;
        Vec::new()
    }

    /// The mpv `af` filter chain for the current EQ + normalization state, or `None` when
    /// nothing is active (the caller then clears `af`).
    pub(in crate::app) fn current_af(&self) -> Option<String> {
        eq::build_af_string(&self.audio.bands, self.audio.normalize)
    }

    /// Whether an mpv video overlay is currently open. Reaps the handle if the user closed mpv
    /// themselves, so a stale exited child reads as closed.
    pub(in crate::app) fn video_open(&mut self) -> bool {
        match self.video.proc.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => {
                    self.video.proc = None;
                    false
                }
                _ => true,
            },
            None => false,
        }
    }

    /// Kill the video overlay if one is open (no-op otherwise). Called from the main loop's
    /// clean-exit path so the overlay never outlives the app, regardless of how we quit.
    /// Also unlinks the overlay's IPC socket; its client task ends on its own when mpv
    /// closes the connection (Windows named pipes self-clean).
    pub fn close_video(&mut self) {
        if let Some(mut child) = self.video.proc.take() {
            child.terminate_and_wait();
        }
        #[cfg(unix)]
        if let Some(path) = self.video.ipc_path.take() {
            let _ = std::fs::remove_file(path);
        }
        #[cfg(not(unix))]
        {
            self.video.ipc_path = None;
        }
    }

    /// Recover a YouTube video id for `song` even when `share_url()`/`youtube_id()` is `None`:
    /// (1) parse an `[id]` tag from its local filename (our downloader embeds it), then (2) match
    /// its normalized title against a remote favorite/history/downloaded entry that has a YouTube
    /// origin. Returns the id (not a URL) so callers can build a share URL or a video URL. `None`
    /// only when the track is genuinely local-only.
    pub(in crate::app) fn recover_youtube_id(&self, song: &Song) -> Option<String> {
        if let Some(id) = song.youtube_id() {
            return Some(id.to_owned());
        }
        if let Some(stem) = song
            .local_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            && let Some((_, id)) = Song::parse_embedded_id(stem)
        {
            return Some(id.to_owned());
        }
        // The title scan lowercases every favorites/history/downloads entry — memoize per
        // (track, library state) since media_snapshot re-asks this at ~1 Hz for local tracks.
        let memo_key = (
            self.library.rev,
            self.library_ui.downloaded_rev,
            self.library.favorites.len(),
            self.library.history.len(),
            self.library_ui.downloaded.len(),
        );
        if let Some(memo) = self.yid_scan_memo.borrow().as_ref()
            && memo.video_id == song.video_id
            && memo.state_key == memo_key
        {
            return memo.result.clone();
        }
        let key = song.title.trim().to_lowercase();
        let result = self
            .library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .chain(self.library_ui.downloaded.iter())
            .find(|e| e.youtube_id().is_some() && e.title.trim().to_lowercase() == key)
            .and_then(|e| e.youtube_id().map(str::to_owned));
        *self.yid_scan_memo.borrow_mut() = Some(YidMemo {
            video_id: song.video_id.clone(),
            state_key: memo_key,
            result: result.clone(),
        });
        result
    }

    /// Convert the current remappable overlay keymap into mpv keybind commands.
    pub(in crate::app) fn video_overlay_bindings(
        &self,
    ) -> Vec<crate::player::video::VideoKeyBinding> {
        use crate::keymap::{Action, KeyContext};
        use crate::player::video::{VideoKeyAction, VideoKeyBinding};

        [
            (Action::VideoTogglePause, VideoKeyAction::TogglePause),
            (Action::VideoNext, VideoKeyAction::Next),
            (Action::VideoPrev, VideoKeyAction::Prev),
            (Action::VideoClose, VideoKeyAction::Close),
            (
                Action::VideoToggleFullscreen,
                VideoKeyAction::ToggleFullscreen,
            ),
            (Action::VideoToggleMute, VideoKeyAction::ToggleMute),
        ]
        .into_iter()
        .filter_map(|(action, video_action)| {
            let chord = self.keymap.chord(KeyContext::MpvOverlay, action)?;
            match crate::keymap::chord_to_mpv_input(chord) {
                Some(key) => Some(VideoKeyBinding::new(key, video_action)),
                None => {
                    tracing::warn!(
                        action = ?action,
                        chord = %crate::keymap::chord_to_config(chord),
                        "skipping unsupported mpv overlay keybinding"
                    );
                    None
                }
            }
        })
        .collect()
    }

    /// An event from the overlay window's IPC client ([`PlayerMsg::VideoOverlay`]). Events carry
    /// the spawn generation they were connected for; anything from a window we already
    /// closed (`v`) or respawned (`Shift+V`) is stale and ignored.
    pub(in crate::app) fn on_video_overlay_event(
        &mut self,
        generation: u64,
        event: crate::player::video::VideoEvent,
    ) -> Vec<Cmd> {
        use crate::player::video::VideoEvent;
        if generation != self.video.generation || self.video.proc.is_none() {
            return Vec::new();
        }
        match event {
            VideoEvent::Eof => {
                if self.config.effective_auto_continue_videos() {
                    self.video_continue_next()
                } else {
                    // Pre-IPC, an ended video meant audio stayed stranded paused until the
                    // user pressed `v` twice; with EOF observable it reads as a close.
                    self.finish_video_overlay(
                        t!("Video ended", "영상이 끝났어요", "動画が終了しました"),
                        StatusKind::Info,
                    )
                }
            }
            VideoEvent::Failed(detail) => {
                let msg = if detail.is_empty() {
                    t!(
                        "Video playback failed",
                        "영상 재생에 실패했어요",
                        "動画の再生に失敗しました"
                    )
                    .to_owned()
                } else {
                    format!(
                        "{} ({detail})",
                        t!(
                            "Video playback failed",
                            "영상 재생에 실패했어요",
                            "動画の再生に失敗しました"
                        )
                    )
                };
                self.finish_video_overlay(&msg, StatusKind::Error)
            }
            VideoEvent::Quit => self.finish_video_overlay(
                t!("Video closed", "영상 닫음", "動画を閉じました"),
                StatusKind::Info,
            ),
            VideoEvent::Next => self.video_skip(true),
            VideoEvent::Prev => self.video_skip(false),
            VideoEvent::TogglePause => vec![Cmd::VideoTogglePause],
            VideoEvent::Paused(paused) => {
                self.status.kind = StatusKind::Info;
                self.status.text = if paused {
                    t!("Video paused", "영상 일시정지", "動画を一時停止").to_owned()
                } else {
                    t!("Video playing", "영상 재생 중", "動画を再生中").to_owned()
                };
                self.dirty = true;
                Vec::new()
            }
            VideoEvent::Close => self.finish_video_overlay(
                t!("Video closed", "영상 닫음", "動画を閉じました"),
                StatusKind::Info,
            ),
            VideoEvent::ToggleFullscreen => {
                self.status.kind = StatusKind::Info;
                self.status.text = t!(
                    "Toggling video fullscreen",
                    "영상 전체 화면 전환",
                    "動画の全画面切り替え"
                )
                .to_owned();
                self.dirty = true;
                vec![Cmd::VideoToggleFullscreen]
            }
            VideoEvent::ToggleMute => {
                self.status.kind = StatusKind::Info;
                self.status.text = t!(
                    "Toggling video mute",
                    "영상 음소거 전환",
                    "動画のミュート切り替え"
                )
                .to_owned();
                self.dirty = true;
                vec![Cmd::VideoToggleMute]
            }
            VideoEvent::Closed => {
                // Act only when the process is genuinely gone: an IPC hiccup with a live
                // window degrades to the pre-IPC behavior instead of yanking the overlay.
                if self.video_open() {
                    return Vec::new();
                }
                self.finish_video_overlay(
                    t!("Video closed", "영상 닫음", "動画を閉じました"),
                    StatusKind::Info,
                )
            }
        }
    }

    /// Auto-continue (Settings › Playback): the overlay reached the current video's end —
    /// advance the queue exactly like an audio EOF, keep the audio engine paused
    /// underneath, and load the next track's video into the same window.
    pub(in crate::app) fn video_continue_next(&mut self) -> Vec<Cmd> {
        // Identical bookkeeping to the audio EOF path (`PlayerMsg::Eof`): full-play
        // signal, repeat/shuffle-aware advance, streaming top-up — so queue semantics
        // never diverge between audio and video continuation.
        let cmds = self.advance_with_outgoing(true, true);
        self.video_follow_queue(cmds, t!("Next video…", "다음 영상…", "次の動画…"))
    }

    /// The `>`/`<` keys pressed inside the overlay window: move the queue like the
    /// player's own next/prev actions, then show the landed track's video.
    pub(in crate::app) fn video_skip(&mut self, forward: bool) -> Vec<Cmd> {
        let (cmds, status) = if forward {
            // Mirror `Action::NextTrack`: a manual skip (ignores repeat-one).
            (
                self.advance_with_outgoing(false, false),
                t!("Next video…", "다음 영상…", "次の動画…"),
            )
        } else {
            // Mirror `Action::PrevTrack`.
            (
                self.previous_track(),
                t!("Previous video…", "이전 영상…", "前の動画…"),
            )
        };
        self.video_follow_queue(cmds, status)
    }

    /// After a queue move with the overlay open: keep the audio engine pinned paused
    /// under the video and load the landed track's video into the same window (or wind
    /// the overlay down when the queue ended / the track is local-only).
    fn video_follow_queue(&mut self, mut cmds: Vec<Cmd>, status: &str) -> Vec<Cmd> {
        if self.attach_video_track_follow_up(&mut cmds, status) {
            return cmds;
        }
        // Next/previous always produces a typed Track intent, including an empty/end queue.
        // If a future caller violates that contract, fail closed through the typed video-finish
        // path instead of projecting pause ownership around an unadmitted raw player command.
        tracing::error!("video queue move did not produce a Track intent");
        cmds.extend(self.finish_video_overlay(
            t!(
                "Video queue transition failed",
                "영상 대기열 전환에 실패했습니다",
                "動画キューの切り替えに失敗しました"
            ),
            StatusKind::Error,
        ));
        cmds
    }

    /// Apply an EQ preset chosen from the dropdown and close it. Mirrors the `e`-key cycle
    /// ([`Action::CycleEq`]) — applied live to mpv, session-scoped (persisted via Settings).
    pub(in crate::app) fn select_eq_preset(&mut self, preset: EqPreset) -> Vec<Cmd> {
        self.eq_preset_intent(preset, false)
    }

    pub(in crate::app) fn select_streaming_mode(&mut self, mode: StreamingMode) -> Vec<Cmd> {
        self.config.streaming.mode = mode;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.status.text = format!(
            "{}: {}",
            t!("Streaming", "스트리밍", "ストリーミング"),
            mode.label()
        );
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    pub(in crate::app) fn on_key_player(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.keymap.action(KeyContext::Player, k.into()) {
            Some(action) => self.on_player_action(action),
            None => Vec::new(),
        }
    }

    pub(in crate::app) fn on_player_action(&mut self, action: Action) -> Vec<Cmd> {
        match action {
            Action::Quit => {
                self.should_quit = true;
                Vec::new()
            }
            Action::Back | Action::Home => self.go_home(),
            Action::ToggleRadioMode => self.request_radio_mode_switch(),
            Action::TogglePause => {
                if self.current_needs_load() {
                    return self.stay_on_current_track();
                }
                let paused = !self.playback.paused;
                // Absolute pause is idempotent when rapid inputs are coalesced/retried. Commit
                // only after admission; manual control then takes ownership from the overlay.
                self.player_intent(
                    "set_pause",
                    PlayerCmd::SetProperty {
                        name: "pause".to_owned(),
                        value: serde_json::Value::Bool(paused),
                    },
                    PlayerCommit::Pause {
                        paused,
                        clear_video_pause: true,
                    },
                )
            }
            Action::SeekBack => self.player_intent(
                "seek_relative",
                PlayerCmd::SeekRelative(-self.audio.seek_seconds),
                PlayerCommit::Seek {
                    optimistic_position: None,
                },
            ),
            Action::SeekForward => self.player_intent(
                "seek_relative",
                PlayerCmd::SeekRelative(self.audio.seek_seconds),
                PlayerCommit::Seek {
                    optimistic_position: None,
                },
            ),
            Action::VolUp => {
                let volume = (self.playback.volume + VOLUME_STEP).min(VOLUME_MAX);
                self.player_intent(
                    "set_volume",
                    PlayerCmd::SetVolume(volume),
                    PlayerCommit::Volume {
                        volume,
                        pre_mute_volume: None,
                    },
                )
            }
            Action::VolDown => {
                let volume = (self.playback.volume - VOLUME_STEP).max(0);
                self.player_intent(
                    "set_volume",
                    PlayerCmd::SetVolume(volume),
                    PlayerCommit::Volume {
                        volume,
                        pre_mute_volume: None,
                    },
                )
            }
            // mpv-style mute: remember the level and drop to 0; toggling restores it. The
            // volume readout naturally shows 0 while muted, and the change rides the existing
            // SetVolume path so the daemon / OS media session stay in sync.
            Action::ToggleMute => {
                let (volume, pre_mute_volume) = match self.playback.pre_mute_volume {
                    Some(previous) => (previous, None),
                    None => (0, Some(self.playback.volume)),
                };
                self.player_intent(
                    "set_volume",
                    PlayerCmd::SetVolume(volume),
                    PlayerCommit::Volume {
                        volume,
                        pre_mute_volume,
                    },
                )
            }
            // Manual next: always moves on, even under repeat-one. A manual skip of the
            // current track is a (position-discounted) negative signal before advancing.
            Action::NextTrack => self.advance_with_outgoing(false, false),
            Action::PrevTrack => self.previous_track(),
            // Cycle the current track's rating through one tri-state control: neutral → 👍 like
            // → 👎 dislike → neutral. `like` is favorite membership (library); `dislike` is the
            // persistent flag the streaming engine treats as a hard block. The two are mutually
            // exclusive, so a single 🤔/👍/👎 glyph (and the `f` key / its click) covers both,
            // replacing the old separate ♥ favorite + ✗ dislike controls. Each leg nudges the
            // artist affinity the engine learns from, and a full cycle nets back to zero.
            Action::CycleRating => {
                if let Some(song) = self.queue.current().cloned() {
                    if song.is_radio_station() {
                        self.library_mut().toggle_favorite(&song);
                        self.dirty = true;
                        return vec![Cmd::Persist(PersistCmd::Library)];
                    }
                    let artist_key = signals::normalize_artist(&song.artist);
                    let now = signals::unix_now();
                    let liked = self.library.is_favorite(&song.video_id);
                    let disliked = self.signals.is_disliked(&song.video_id);
                    match (liked, disliked) {
                        // neutral → like: add to favorites, lift the artist affinity.
                        (false, false) => {
                            let now_fav = self.library_mut().toggle_favorite(&song);
                            self.signals_mut().record_like(
                                &song.video_id,
                                &artist_key,
                                now_fav,
                                now,
                            );
                            let comp = self.playback_completion();
                            self.record_session_event(&artist_key, Outcome::Like, comp);
                            self.dirty = true;
                            return vec![
                                Cmd::Persist(PersistCmd::Library),
                                Cmd::Persist(PersistCmd::Signals),
                            ];
                        }
                        // like → dislike: drop the favorite (undoing its affinity lift) and set
                        // the dislike flag (which pushes the affinity down).
                        (true, _) => {
                            self.library_mut().toggle_favorite(&song);
                            self.signals_mut()
                                .record_like(&song.video_id, &artist_key, false, now);
                            self.signals_mut()
                                .toggle_dislike(&song.video_id, &artist_key, now);
                            let comp = self.playback_completion();
                            self.record_session_event(&artist_key, Outcome::Dislike, comp);
                            self.dirty = true;
                            return vec![
                                Cmd::Persist(PersistCmd::Library),
                                Cmd::Persist(PersistCmd::Signals),
                            ];
                        }
                        // dislike → neutral: clear the flag, restoring the affinity it pushed down.
                        (false, true) => {
                            self.signals_mut()
                                .toggle_dislike(&song.video_id, &artist_key, now);
                            self.dirty = true;
                            return vec![Cmd::Persist(PersistCmd::Signals)];
                        }
                    }
                }
                Vec::new()
            }
            Action::OpenLibrary => {
                self.mode = Mode::Library;
                if !self.library_tab_available(self.library_ui.tab) {
                    self.library_ui.tab = self.library_tabs()[0];
                }
                // Start each library visit with a clean, unfiltered list (also resets the
                // cursor, the multi-select anchor, the scroll offset, and any playlist
                // drill-down or popup left from the previous visit).
                self.reset_playlist_ui_state();
                self.clear_library_filter();
                if self.effective_library_tab() == LibraryTab::Playlists {
                    self.hint_playlist_create();
                }
                self.dropdowns.eq_open = false;
                self.dropdowns.streaming_open = false;
                self.dropdowns.search_source_open = false;
                self.dirty = true;
                Vec::new()
            }
            Action::OpenQueue => {
                self.open_queue_popup();
                Vec::new()
            }
            Action::QueueRemove => {
                if self.queue.is_empty() {
                    Vec::new()
                } else {
                    self.remove_queue_range(self.queue.cursor_pos(), self.queue.cursor_pos())
                }
            }
            Action::LyricsDelayEarlier => {
                if self.adjust_lyrics_delay(LyricsDelayDirection::Earlier, Instant::now()) {
                    self.dirty = true;
                }
                Vec::new()
            }
            Action::LyricsDelayLater => {
                if self.adjust_lyrics_delay(LyricsDelayDirection::Later, Instant::now()) {
                    self.dirty = true;
                }
                Vec::new()
            }
            // Toggle the lyrics panel; fetch on first open for the current track.
            Action::ToggleLyrics => {
                self.lyrics.visible = !self.lyrics.visible;
                self.dirty = true;
                if self.lyrics.visible
                    && self.lyrics_stale()
                    && let Some(song) = self.queue.current().cloned()
                {
                    self.lyrics.loading = true;
                    return vec![fetch_lyrics_cmd(&song)];
                }
                Vec::new()
            }
            Action::Download => match self.queue.current().cloned() {
                Some(song) => self.start_download(song),
                None => Vec::new(),
            },
            // On a live radio stream the shuffle/repeat slots are reinterpreted as the
            // live-transport controls: sync indicator (read-only note) and re-sync. The
            // queue's shuffle/repeat state stays untouched so leaving radio restores the
            // music-mode toggles exactly as they were.
            Action::ToggleShuffle if self.current_is_radio_stream() => {
                self.radio_sync_status_note()
            }
            Action::CycleRepeat if self.current_is_radio_stream() => self.resync_radio_to_live(),
            Action::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.dirty = true;
                vec![self.save_playback_modes_cmd()]
            }
            Action::CycleRepeat => {
                let transition = PlaybackModeState::new(self.queue.repeat, self.autoplay_streaming)
                    .transition(PlaybackModeAction::CycleRepeat);
                let Ok(transition) = transition else {
                    self.show_repeat_streaming_conflict();
                    return Vec::new();
                };
                self.queue.repeat = transition.state.repeat;
                self.dirty = true;
                vec![self.save_playback_modes_cmd()]
            }
            // Cycle the EQ preset and apply it immediately.
            Action::CycleEq => self.eq_preset_intent(self.audio.preset.cycled(), true),
            Action::ToggleNormalize => self.normalize_intent(!self.audio.normalize, false),
            Action::SpeedUp => self.adjust_speed(SPEED_STEP),
            Action::SpeedDown => self.adjust_speed(-SPEED_STEP),
            Action::OpenSettings => {
                self.open_settings();
                Vec::new()
            }
            Action::OpenAi => {
                self.enter_ai();
                Vec::new()
            }
            Action::IdentifyNowPlaying => self.open_now_playing_overlay(),
            Action::ToggleRecordings => {
                if self.overlays.recordings_browser.is_some() {
                    self.overlays.recordings_browser = None;
                } else if self.current_is_radio_stream() || !self.recorder.history.is_empty() {
                    self.overlays.recordings_browser = Some(RecordingsBrowser::default());
                } else {
                    self.status.kind = StatusKind::Info;
                    self.status.text = t!(
                        "Radio recordings appear here while a station plays",
                        "라디오 방송 중에 녹음 목록이 여기에 표시돼요",
                        "ラジオ再生中に録音一覧がここに表示されます"
                    )
                    .to_owned();
                }
                self.dirty = true;
                Vec::new()
            }
            Action::OpenSearch => {
                if self.active_search_surface() == ActiveSearchSurface::Local {
                    return self.open_local_find();
                }
                self.mode = Mode::Search;
                self.search.focus = SearchFocus::Input;
                self.search.input_cursor = TextCursor::at_end(&self.search.input);
                let search = self.search_config_for_mode();
                self.search.source = search.normalized_source(self.search.source);
                self.dropdowns.eq_open = false;
                self.dropdowns.streaming_open = false;
                self.dropdowns.search_source_open = false;
                self.dirty = true;
                Vec::new()
            }
            // `P` opens the add-to-playlist picker for the track that's playing.
            Action::AddToPlaylist => {
                if let Some(song) = self.queue.current().cloned() {
                    self.open_playlist_picker(vec![song]);
                }
                Vec::new()
            }
            Action::CopyLink => {
                // Compute the (owned) URL before touching `self.status` to avoid borrowing
                // `self` both immutably (queue) and mutably (status) at once. `recover_youtube_id`
                // covers the cases plain `share_url()` misses — a downloaded track whose id lives
                // in its `[id]` filename, or a bare `local:` history/queue entry whose twin is in
                // favorites/history — so copying works regardless of which list the song came from.
                let had_track = self.queue.current().is_some();
                let url = self.queue.current().and_then(|s| {
                    s.share_url().or_else(|| {
                        self.recover_youtube_id(s)
                            .map(|id| format!("https://www.youtube.com/watch?v={id}"))
                    })
                });
                match url {
                    Some(url) => {
                        copy_to_clipboard(&url);
                        self.status.kind = StatusKind::Info;
                        self.status.text = t!(
                            "✓ Link copied to clipboard",
                            "✓ 링크가 클립보드에 복사됐어요",
                            "✓ リンクをクリップボードにコピーしました"
                        )
                        .to_owned();
                        self.dirty = true;
                    }
                    None if had_track => {
                        // Current track is genuinely local-only — no YouTube origin to share.
                        self.status.text = t!(
                            "This track is local-only — no YouTube link",
                            "로컬 전용 트랙이라 유튜브 링크가 없어요",
                            "ローカル専用の曲のため YouTube リンクがありません"
                        )
                        .to_owned();
                        self.dirty = true;
                    }
                    None => {}
                }
                Vec::new()
            }
            Action::PlayVideo => self.toggle_video_overlay(),
            Action::ToggleVideoLayout => self.toggle_video_layout(),
            _ => Vec::new(),
        }
    }

    /// Play `song` right now **without wiping the queue**: insert it immediately after the
    /// current track, jump to it, and load it, so whatever was queued resumes after it ends.
    /// Into an empty queue it just becomes the sole track. This is the unified Enter / double-
    /// click "play" gesture in both the Library and the Search results.
    pub(in crate::app) fn play_now(&mut self, song: Song) -> Vec<Cmd> {
        let (plan, outcome) = self.queue.prepare_play_now_many(vec![song]);
        debug_assert_eq!(outcome.requested(), 1);
        if outcome.added() == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text =
                t!("Queue is full", "큐가 가득 찼어요", "キューがいっぱいです").to_string();
            self.dirty = true;
            return Vec::new();
        }
        debug_assert_eq!(outcome.selected_cursor(), Some(plan.cursor_pos()));
        self.load_prepared_queue_mutation(plan, Vec::new())
    }

    /// Play several tracks now without wiping the queue: insert them immediately after the
    /// current track, jump to the first inserted track, and let the rest follow in order.
    pub(in crate::app) fn play_now_many(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        if songs.is_empty() {
            return Vec::new();
        }
        let requested_songs = songs.clone();
        let (plan, outcome) = self.queue.prepare_play_now_many(songs);
        debug_assert_eq!(outcome.requested(), requested_songs.len());
        if outcome.added() == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text =
                t!("Queue is full", "큐가 가득 찼어요", "キューがいっぱいです").to_string();
            self.dirty = true;
            return Vec::new();
        }
        debug_assert_eq!(outcome.selected_cursor(), Some(plan.cursor_pos()));
        self.load_prepared_queue_mutation(plan, requested_songs)
    }

    /// Add `song` to the queue without interrupting playback — the unified `\` / right-click
    /// gesture in the Library and Search results. By default this appends to the end; when the
    /// "enqueue as next" setting is on, it inserts immediately after the current track.
    /// If nothing is currently playing we jump to it and start.
    pub(in crate::app) fn enqueue(&mut self, song: Song) -> Vec<Cmd> {
        self.enqueue_many(vec![song])
    }

    /// Add several tracks to the queue without interrupting playback. If idle, start the first
    /// added track; otherwise append to the end or insert after the current track according to
    /// the user's enqueue policy.
    pub(in crate::app) fn enqueue_many(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        if songs.is_empty() {
            return Vec::new();
        }
        let queued_songs = songs.clone();
        let requested = songs.len();
        let was_idle = self.prefetch.loaded_video_id.is_none();
        if was_idle {
            let (plan, outcome) = self.queue.prepare_idle_enqueue(songs);
            debug_assert_eq!(outcome.requested(), requested);
            if outcome.added() == 0 {
                self.status.kind = StatusKind::Error;
                self.status.text =
                    t!("Queue is full", "큐가 가득 찼어요", "キューがいっぱいです").to_string();
                self.dirty = true;
                return Vec::new();
            }
            debug_assert_eq!(outcome.selected_cursor(), Some(plan.cursor_pos()));
            return self.load_prepared_queue_mutation(plan, queued_songs);
        }

        let enqueue_next = self.config.effective_enqueue_next();
        let added = if enqueue_next {
            self.queue.insert_next_many(songs)
        } else {
            self.queue.extend(songs)
        };
        if added == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text =
                t!("Queue is full", "큐가 가득 찼어요", "キューがいっぱいです").to_string();
            self.dirty = true;
            return Vec::new();
        }
        let cmds = self.request_romanization_for_songs(&queued_songs);
        let first_title = self.display_title(&queued_songs[0]).into_owned();
        // A track is already playing → queue it by policy, with no interruption.
        self.status.kind = StatusKind::Info;
        self.status.text = if requested == 1 && added == 1 {
            let prefix = if enqueue_next {
                t!("Added next:", "다음 곡으로 추가:", "次の曲に追加:")
            } else {
                t!("Added to queue:", "큐에 추가:", "キューに追加:")
            };
            format!("{prefix} {first_title}")
        } else if enqueue_next {
            format!(
                "{} {}",
                added,
                t!(
                    "tracks added next",
                    "곡을 다음 곡으로 추가",
                    "曲を次の曲に追加"
                )
            )
        } else {
            format!(
                "{} {}",
                added,
                t!(
                    "tracks added to queue",
                    "곡을 큐에 추가",
                    "曲をキューに追加"
                )
            )
        };
        self.dirty = true;
        cmds
    }

    /// Feed the outgoing current track into the preference signals. `full` = it played to
    /// its end (EOF) → a full-play signal; otherwise it's a user skip and the completion is
    /// derived from the last reported position (a weak negative when position is unknown).
    /// Call this *before* [`Self::advance`] (it reads `queue.current()`). Playback *errors*
    /// must not call it — a track that failed to play isn't a dislike. Returns the persist cmd.
    pub(in crate::app) fn record_outgoing(&mut self, full: bool) -> Vec<Cmd> {
        let Some(song) = self.queue.current().cloned() else {
            return Vec::new();
        };
        if song.is_radio_station() {
            return Vec::new();
        }
        let artist_key = signals::normalize_artist(&song.artist);
        let now = signals::unix_now();
        if full {
            self.signals_mut()
                .record_play(&song.video_id, &artist_key, 1.0, now);
            self.record_session_event(&artist_key, Outcome::FullPlay, 1.0);
        } else {
            let completion = self.playback_completion();
            let scale = self.skip_feedback_scale();
            self.signals_mut()
                .record_skip(&song.video_id, &artist_key, completion, now, scale);
            // A skip below the strong threshold is a near-instant bail — a louder "wrong way"
            // cue for the reranker than an ordinary skip.
            let outcome = if completion < signals::STRONG_SKIP_FRAC {
                Outcome::QuickSkip
            } else {
                Outcome::Skip
            };
            self.record_session_event(&artist_key, outcome, completion);
        }
        let mut cmds = vec![Cmd::Persist(PersistCmd::Signals)];
        // A skip just landed in the session log — if the listener is rejecting the active
        // station's direction, this may kick off an off-path feedback summary (self-gated).
        if let Some(feedback) = self.maybe_summarize_feedback() {
            cmds.push(feedback);
        }
        cmds
    }

    /// Current track completion ratio in [0,1]. Unknown position (no progress reported yet) →
    /// `0.5`, a weak negative: the user may have skipped before playback even started, so it
    /// mustn't read as a strong dislike.
    pub(in crate::app) fn playback_completion(&self) -> f32 {
        match (self.playback.time_pos, self.playback.duration) {
            (Some(t), Some(d)) if d > 0.0 => (t / d).clamp(0.0, 1.0) as f32,
            _ => 0.5,
        }
    }

    /// Push one ordered session outcome (newest at the back), bounded to the last
    /// [`SESSION_EVENTS_CAP`]. Feeds the DJ Gem reranker's recovery context.
    pub(in crate::app) fn record_session_event(
        &mut self,
        artist_key: &str,
        outcome: Outcome,
        completion: f32,
    ) {
        let buf = &mut self.streaming.session_events;
        buf.push_back(SessionEvent {
            artist_key: artist_key.to_owned(),
            outcome,
            completion,
        });
        while buf.len() > SESSION_EVENTS_CAP {
            buf.pop_front();
        }
    }

    /// How much to trust a skip as a dislike signal: lower early in / in short sessions
    /// (sampling, settling in, inattention), full once the user is clearly engaged. The
    /// skip itself is always counted; this only scales the learned artist penalty.
    pub(in crate::app) fn skip_feedback_scale(&self) -> f32 {
        match self.session.plays {
            0..=4 => 0.3,  // short / early session — barely trust
            5..=10 => 0.6, // warming up
            _ => 1.0,      // deeply engaged
        }
    }

    /// Update session bookkeeping on a track start: a long idle gap begins a fresh session,
    /// otherwise this is the next track in the current one. Feeds [`Self::skip_feedback_scale`].
    pub(in crate::app) fn note_session_activity(&mut self) {
        let now = signals::unix_now();
        if self
            .session
            .last_activity_at
            .is_some_and(|prev| now - prev > SESSION_GAP_SECS)
        {
            self.session.plays = 0;
        }
        self.session.plays = self.session.plays.saturating_add(1);
        self.session.last_activity_at = Some(now);
    }

    pub(in crate::app) fn current_needs_load(&self) -> bool {
        self.queue.current().is_some_and(|song| {
            self.prefetch.loaded_video_id.as_deref() != Some(song.video_id.as_str())
        })
    }

    /// Whether the current queue item is a live radio stream. A property of the *track*,
    /// not the dedicated-radio UI mode — a station playing in normal mode gets the same
    /// live-transport rules (sync indicator, re-sync, timeshift seekbar).
    pub fn current_is_radio_stream(&self) -> bool {
        self.queue.current().is_some_and(Song::is_radio_station)
    }

    /// Whether autoplay streaming is *effectively* active right now. The stored
    /// [`Self::autoplay_streaming`] preference is left untouched by dedicated modes; this getter
    /// reports off whenever streaming would be meaningless — in Radio or Local Deck mode, or
    /// while a live station is the current track — so the engine skips top-ups and the status
    /// line hides the `streaming:` indicator, yet the user's saved preference survives the
    /// dedicated-mode round-trip.
    pub fn streaming_active(&self) -> bool {
        self.autoplay_streaming
            && !self.radio_dedicated_mode
            && !self.local_dedicated_mode
            && !self.current_is_radio_stream()
    }

    /// Seconds the playhead sits behind the live edge (`demuxer-cache-time − time-pos`),
    /// or `None` when it can't be known. A stale report while playing proves nothing
    /// about being *at* the edge, but a stale edge is still a lower bound — so a clearly
    /// behind verdict survives staleness, while a "synced" one degrades to unknown.
    /// While paused the last report is kept (the buffer saturates and freezes; being
    /// behind only grows).
    pub fn radio_behind_secs(&self) -> Option<f64> {
        if !self.current_is_radio_stream() {
            return None;
        }
        let edge = self.playback.cache_time?;
        let at = self.playback.cache_time_at?;
        let behind = (edge - self.playback.time_pos?).max(0.0);
        let stale = !self.playback.paused && at.elapsed().as_secs_f64() > CACHE_TIME_STALE_SECS;
        if stale && behind <= LIVE_SYNC_THRESHOLD_SECS {
            return None;
        }
        Some(behind)
    }

    /// The live-sync verdict for the current radio stream: `Some(true)` at the live edge,
    /// `Some(false)` behind (timeshifted), `None` unknown (no usable cache info).
    pub fn radio_live_synced(&self) -> Option<bool> {
        self.radio_behind_secs()
            .map(|behind| behind <= LIVE_SYNC_THRESHOLD_SECS)
    }

    /// The shuffle slot's radio behavior: report the live-sync state as an Info toast.
    /// Deliberately read-only — an accidental press must never seek or mutate the queue.
    pub(in crate::app) fn radio_sync_status_note(&mut self) -> Vec<Cmd> {
        self.status.kind = StatusKind::Info;
        self.status.text = match self.radio_behind_secs() {
            Some(b) if b <= LIVE_SYNC_THRESHOLD_SECS => t!(
                "Live: at the live edge",
                "라이브: 실시간 재생 중",
                "ライブ: リアルタイム再生中"
            )
            .to_owned(),
            Some(b) => {
                let key = self.keymap.label_for_display(
                    crate::keymap::KeyContext::Player,
                    Action::CycleRepeat,
                    self.retro_mode(),
                );
                match crate::i18n::current() {
                    crate::i18n::Language::Korean => {
                        format!("라이브: {}초 뒤처짐 — {key} 키로 다시 맞추기", b as i64)
                    }
                    crate::i18n::Language::Japanese => {
                        format!("ライブ: {}秒遅れ — {key} キーで再同期", b as i64)
                    }
                    _ => format!("Live: {}s behind — press {key} to re-sync", b as i64),
                }
            }
            None => t!(
                "Live: sync state unknown",
                "라이브: 동기화 상태를 알 수 없어요",
                "ライブ: 同期状態が不明です"
            )
            .to_owned(),
        };
        self.dirty = true;
        Vec::new()
    }

    /// The repeat slot's radio behavior: return to the live edge. Seeks to the newest
    /// demuxed data when a usable edge is known; reconnects the stream when it isn't, or
    /// when a recent seek demonstrably didn't take. Resumes playback either way.
    pub(in crate::app) fn resync_radio_to_live(&mut self) -> Vec<Cmd> {
        if self.queue.current().is_none() {
            return Vec::new();
        }
        let behind = self.radio_behind_secs();
        if let Some(b) = behind
            && b <= LIVE_SYNC_THRESHOLD_SECS
            && !self.playback.paused
        {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Live: at the live edge",
                "라이브: 실시간 재생 중",
                "ライブ: リアルタイム再生中"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let seek_failed_recently = self
            .radio_resync_at
            .is_some_and(|at| at.elapsed().as_secs_f64() < RESYNC_RETRY_WINDOW_SECS);
        if let Some(edge) = self.playback.cache_time
            && behind.is_some()
            && !seek_failed_recently
        {
            return self.radio_live_seek_intent((edge - LIVE_EDGE_SEEK_MARGIN_SECS).max(0.0));
        }
        // No usable edge (cache-less stream) or the seek didn't take → reconnect. A fresh
        // connection starts at the live edge by construction. The typed Stay transition owns
        // recorder teardown, reload bookkeeping, and the success toast.
        self.reconnect_radio_to_live()
    }

    /// Whether we lack lyrics for the current track (so a fetch is warranted).
    pub(in crate::app) fn lyrics_stale(&self) -> bool {
        match (&self.lyrics.track, self.queue.current()) {
            (Some(l), Some(cur)) => l.video_id.as_ref() != cur.video_id.as_str(),
            (None, Some(_)) => true,
            _ => false,
        }
    }

    /// Clear per-track playback state before loading a new track.
    pub(in crate::app) fn reset_progress(&mut self) {
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        // A track (re)start is a position discontinuity — the media session must
        // re-announce position 0 (repeat-one restarts included, where the track key
        // alone wouldn't change).
        self.bump_position_epoch(PositionEpochReason::TrackRestart);
        self.playback.duration = None;
        self.playback.paused = false;
        self.playback.stream_now_playing = None;
        self.playback.cache_time = None;
        self.playback.cache_time_at = None;
        self.anim.last_shown_sec = -1;
        self.anim.last_shown_cache_sec = -1;
        self.radio_resync_at = None;
    }
}
