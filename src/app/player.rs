//! Player/playback reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

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
    /// The current track reached its end.
    Eof,
    /// mpv reported a playback error.
    Error(String),
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
    /// Handle an mpv playback error: self-heal a stale-yt-dlp extraction failure
    /// once, otherwise skip the bad track (with a circuit breaker after too many in a
    /// row). Extracted verbatim from the `PlayerMsg::Error` dispatch arm; the
    /// `position_epoch` bump stays in `App::update`.
    pub(in crate::app) fn on_player_error(&mut self, e: String) -> Vec<Cmd> {
        // Log *which* track failed and whether it came from a (possibly stale)
        // prefetched URL. `e` already carries mpv's own reason (its `file_error`
        // end-file field — the closest thing to a "why": HTTP 403, unsupported, …).
        let failed = self
            .queue
            .current()
            .map(|s| format!("{} — {}", s.title, s.artist));
        tracing::warn!(
            error = %e,
            track = failed.as_deref().unwrap_or("?"),
            prefetched = self.prefetch.last_load_prefetched,
            "playback error"
        );
        let extraction = crate::tools::looks_like_extraction_failure(&e);
        if self.prefetch.last_load_prefetched
            && let Some(song) = self.queue.current()
            && let Some(watch_url) = song.prefetch_target()
            && !self.prefetch.watch_retry_attempted.contains(&song.video_id)
        {
            let video_id = song.video_id.clone();
            self.prefetch.resolved.remove(&video_id);
            self.prefetch.watch_retry_attempted.insert(video_id.clone());
            self.prefetch.last_load_prefetched = false;
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Prefetched stream failed — retrying the track",
                "미리 해석한 스트림 실패 — 같은 곡을 다시 시도"
            )
            .to_owned();
            self.dirty = true;
            tracing::info!(video_id = %video_id, "retrying failed prefetched stream via watch URL");
            return vec![Cmd::Player(PlayerCmd::Load(watch_url))];
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
                "스트림 해석 실패 — yt-dlp 업데이트 중…"
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
            let cmds = self.advance(false);
            self.status.text = if extraction {
                t!(
                    "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
                    "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀"
                )
            } else {
                t!(
                    "⚠ Track unavailable — skipped to next",
                    "⚠ 재생할 수 없는 곡 — 다음 곡으로 건너뜀"
                )
            }
            .to_owned();
            self.dirty = true;
            return cmds;
        }
        self.status.text = if self.consecutive_play_errors > MAX_CONSECUTIVE_PLAY_ERRORS {
            if extraction {
                t!(
                            "Several tracks failed — run `ytt tools reset --playback`, then `ytt doctor --verbose` if it continues.",
                            "여러 곡 재생 실패 — `ytt tools reset --playback` 실행 후 계속되면 `ytt doctor --verbose`를 확인하세요."
                        ).to_owned()
            } else {
                t!(
                            "Several tracks failed to play — stopped. Check your connection, or sign in (cookies) for gated tracks.",
                            "여러 곡 재생에 실패해서 중단했어요. 연결을 확인하거나, 제한된 곡은 로그인(쿠키)하세요."
                        ).to_owned()
            }
        } else {
            format!("{}: {e}", t!("Playback error", "재생 오류"))
        };
        self.dirty = true;
        Vec::new()
    }

    /// The mpv `af` filter chain for the current EQ + normalization state, or `None` when
    /// nothing is active (the caller then clears `af`).
    pub(in crate::app) fn current_af(&self) -> Option<String> {
        eq::build_af_string(&self.audio.bands, self.audio.normalize)
    }

    /// Change playback speed by `delta`, clamped and rounded to one decimal, and emit the
    /// `set_property speed` command.
    pub(in crate::app) fn adjust_speed(&mut self, delta: f64) -> Vec<Cmd> {
        self.playback.speed =
            (((self.playback.speed + delta) * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX);
        self.status.text = format!("{}: {:.1}x", t!("Speed", "재생 속도"), self.playback.speed);
        self.dirty = true;
        vec![Cmd::Player(PlayerCmd::SetProperty {
            name: "speed".to_owned(),
            value: serde_json::Value::from(self.playback.speed),
        })]
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
            let _ = child.kill();
            let _ = child.wait();
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
    fn video_overlay_bindings(&self) -> Vec<crate::player::video::VideoKeyBinding> {
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

    /// Spawn the overlay window for YouTube id `id` in `layout` and wire up its IPC client
    /// under a fresh spawn generation. Returns `false` when mpv failed to launch. When the
    /// IPC endpoint path can't be prepared, the window still opens — it just degrades to
    /// the pre-IPC fire-and-forget behavior (no EOF detection, no auto-continue).
    fn open_video_overlay(
        &mut self,
        id: &str,
        layout: crate::config::VideoOverlay,
        cmds: &mut Vec<Cmd>,
    ) -> bool {
        let url = format!("https://www.youtube.com/watch?v={id}");
        let data_dir = crate::paths::data_dir();
        let (cookies, cookies_warning) = self
            .config
            .cookies_file_for_external_tools_with_warning(data_dir.as_deref());
        if let Some(warning) = cookies_warning {
            self.set_status_error(warning);
        }
        self.video.generation = self.video.generation.wrapping_add(1);
        let generation = self.video.generation;
        let ipc_path = crate::player::mpv::video_ipc_path(generation)
            .inspect_err(|e| tracing::warn!(error = %e, "video overlay IPC path unavailable"))
            .ok();
        match spawn_video_overlay(&url, cookies.as_deref(), layout, ipc_path.as_deref()) {
            Some(child) => {
                self.video.proc = Some(child);
                self.video.ipc_path = ipc_path.clone();
                if let Some(ipc_path) = ipc_path {
                    cmds.push(Cmd::VideoConnect {
                        ipc_path,
                        generation,
                        bindings: self.video_overlay_bindings(),
                    });
                }
                true
            }
            None => false,
        }
    }

    /// Close the overlay and resume the audio the overlay paused (per
    /// [`Video::paused_audio`]), leaving `status` as the transient info line. Shared by
    /// the manual close (`v`), the overlay's own end/quit events, and the fallback paths.
    fn finish_video_overlay(&mut self, status: &str) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        self.close_video();
        if self.video.paused_audio {
            self.video.paused_audio = false;
            self.playback.paused = false;
            cmds.push(Cmd::Player(PlayerCmd::SetProperty {
                name: "pause".to_owned(),
                value: serde_json::Value::Bool(false),
            }));
        }
        self.status.kind = StatusKind::Info;
        self.status.text = status.to_owned();
        self.dirty = true;
        cmds
    }

    /// `v`: toggle the external mpv video overlay. Open → close it and resume the audio we
    /// paused; closed → launch it for the current track and pause the audio.
    pub(in crate::app) fn toggle_video_overlay(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if self.video_open() {
            return self.finish_video_overlay(t!("Video closed", "영상 닫음"));
        }
        if let Some(song) = self.queue.current().cloned() {
            let Some(id) = self.recover_youtube_id(&song) else {
                // Local-only track with no recoverable YouTube origin → nothing to show.
                self.status.text = t!(
                    "This track is local-only — no video",
                    "로컬 전용 트랙이라 영상이 없어요"
                )
                .to_owned();
                self.dirty = true;
                return cmds;
            };
            if self.open_video_overlay(&id, self.config.video_layout, &mut cmds) {
                if !self.playback.paused {
                    self.playback.paused = true;
                    self.video.paused_audio = true;
                    cmds.push(Cmd::Player(PlayerCmd::SetProperty {
                        name: "pause".to_owned(),
                        value: serde_json::Value::Bool(true),
                    }));
                }
                self.status.kind = StatusKind::Info;
                self.status.text =
                    t!("Opening video in mpv…", "mpv에서 영상을 여는 중…").to_owned();
            } else {
                self.status.text = t!("Failed to launch mpv", "mpv 실행에 실패했습니다").to_owned();
            }
        } else {
            self.status.text = t!("No track playing", "재생 중인 곡이 없습니다").to_owned();
        }
        self.dirty = true;
        cmds
    }

    /// `Shift+V`: toggle the overlay layout (top-right 30% ↔ center 50%), persist it, and — if
    /// a video is open — respawn it in the new layout (mpv can't reliably resize a live window).
    pub(in crate::app) fn toggle_video_layout(&mut self) -> Vec<Cmd> {
        self.config.video_layout = self.config.video_layout.toggled();
        let layout = self.config.video_layout;
        let mut cmds = vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))];
        if self.video_open() {
            // Respawn in the new layout (mpv can't reliably resize a live window). If the
            // current track has no recoverable YouTube origin, close the overlay and resume
            // audio rather than leave a stale window falsely reporting the new layout.
            let id = self
                .queue
                .current()
                .cloned()
                .and_then(|song| self.recover_youtube_id(&song));
            self.close_video();
            match id {
                Some(id) => {
                    if !self.open_video_overlay(&id, layout, &mut cmds) {
                        // The respawn failed: resume audio rather than strand it paused
                        // behind a window that no longer exists.
                        cmds.extend(self.finish_video_overlay(t!(
                            "Failed to launch mpv",
                            "mpv 실행에 실패했습니다"
                        )));
                        self.status.kind = StatusKind::Error;
                        return cmds;
                    }
                    // Audio stays paused (video.paused_audio unchanged).
                }
                None => {
                    cmds.extend(self.finish_video_overlay(t!(
                        "This track is local-only — no video",
                        "로컬 전용 트랙이라 영상이 없어요"
                    )));
                    return cmds;
                }
            }
        }
        self.status.kind = StatusKind::Info;
        self.status.text = format!("{}: {}", t!("Video", "영상"), layout.label());
        self.dirty = true;
        cmds
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
                    self.finish_video_overlay(t!("Video ended", "영상이 끝났어요"))
                }
            }
            VideoEvent::Failed(detail) => {
                let msg = if detail.is_empty() {
                    t!("Video playback failed", "영상 재생에 실패했어요").to_owned()
                } else {
                    format!(
                        "{} ({detail})",
                        t!("Video playback failed", "영상 재생에 실패했어요")
                    )
                };
                let cmds = self.finish_video_overlay(&msg);
                self.status.kind = StatusKind::Error;
                cmds
            }
            VideoEvent::Quit => self.finish_video_overlay(t!("Video closed", "영상 닫음")),
            VideoEvent::Next => self.video_skip(true),
            VideoEvent::Prev => self.video_skip(false),
            VideoEvent::TogglePause => vec![Cmd::VideoTogglePause],
            VideoEvent::Paused(paused) => {
                self.status.kind = StatusKind::Info;
                self.status.text = if paused {
                    t!("Video paused", "영상 일시정지").to_owned()
                } else {
                    t!("Video playing", "영상 재생 중").to_owned()
                };
                self.dirty = true;
                Vec::new()
            }
            VideoEvent::Close => self.finish_video_overlay(t!("Video closed", "영상 닫음")),
            VideoEvent::ToggleFullscreen => {
                self.status.kind = StatusKind::Info;
                self.status.text =
                    t!("Toggling video fullscreen", "영상 전체 화면 전환").to_owned();
                self.dirty = true;
                vec![Cmd::VideoToggleFullscreen]
            }
            VideoEvent::ToggleMute => {
                self.status.kind = StatusKind::Info;
                self.status.text = t!("Toggling video mute", "영상 음소거 전환").to_owned();
                self.dirty = true;
                vec![Cmd::VideoToggleMute]
            }
            VideoEvent::Closed => {
                // Act only when the process is genuinely gone: an IPC hiccup with a live
                // window degrades to the pre-IPC behavior instead of yanking the overlay.
                if self.video_open() {
                    return Vec::new();
                }
                self.finish_video_overlay(t!("Video closed", "영상 닫음"))
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
        let mut cmds = self.record_outgoing(true);
        cmds.extend(self.advance(true));
        self.video_follow_queue(cmds, t!("Next video…", "다음 영상…"))
    }

    /// The `>`/`<` keys pressed inside the overlay window: move the queue like the
    /// player's own next/prev actions, then show the landed track's video.
    pub(in crate::app) fn video_skip(&mut self, forward: bool) -> Vec<Cmd> {
        let (cmds, status) = if forward {
            // Mirror `Action::NextTrack`: a manual skip (ignores repeat-one).
            let mut cmds = self.record_outgoing(false);
            cmds.extend(self.advance(false));
            (cmds, t!("Next video…", "다음 영상…"))
        } else {
            // Mirror `Action::PrevTrack`.
            let song = self.queue.prev().cloned();
            (self.load_song(song), t!("Previous video…", "이전 영상…"))
        };
        self.video_follow_queue(cmds, status)
    }

    /// After a queue move with the overlay open: keep the audio engine pinned paused
    /// under the video and load the landed track's video into the same window (or wind
    /// the overlay down when the queue ended / the track is local-only).
    fn video_follow_queue(&mut self, mut cmds: Vec<Cmd>, status: &str) -> Vec<Cmd> {
        if self.prefetch.loaded_video_id.is_none() {
            // Queue ended (the move loaded nothing): close the overlay and drop the
            // stale paused track from mpv, mirroring the audio queue-end (idle, paused).
            self.close_video();
            self.video.paused_audio = false;
            cmds.push(Cmd::Player(PlayerCmd::Stop));
            self.status.kind = StatusKind::Info;
            self.status.text = t!("Queue ended", "큐가 끝났어요").to_owned();
            self.dirty = true;
            return cmds;
        }
        // load_song() loaded the landed track into the (still paused) audio engine, but
        // reset_progress() cleared our pause flag — re-pin both sides so audio never
        // plays under the video and a later close resumes at this track.
        self.playback.paused = true;
        self.video.paused_audio = true;
        cmds.push(Cmd::Player(PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(true),
        }));
        match self
            .queue
            .current()
            .cloned()
            .and_then(|song| self.recover_youtube_id(&song))
        {
            Some(id) => {
                cmds.push(Cmd::VideoLoad(format!(
                    "https://www.youtube.com/watch?v={id}"
                )));
                self.status.kind = StatusKind::Info;
                self.status.text = status.to_owned();
            }
            None => {
                // The landed track is local-only (no recoverable video): fall back to
                // audio playback instead of skipping tracks hunting for one.
                cmds.extend(self.finish_video_overlay(t!(
                    "This track is local-only — continuing with audio",
                    "로컬 전용 트랙이라 소리로 이어서 재생해요"
                )));
            }
        }
        self.dirty = true;
        cmds
    }

    /// Apply an EQ preset chosen from the dropdown and close it. Mirrors the `e`-key cycle
    /// ([`Action::CycleEq`]) — applied live to mpv, session-scoped (persisted via Settings).
    pub(in crate::app) fn select_eq_preset(&mut self, preset: EqPreset) -> Vec<Cmd> {
        self.audio.preset = preset;
        self.audio.bands = preset.gains();
        self.dropdowns.eq_open = false;
        self.dropdowns.search_source_open = false;
        self.status.text = format!("EQ: {}", preset.label());
        self.dirty = true;
        vec![Cmd::Player(PlayerCmd::SetAudioFilter(
            self.current_af().unwrap_or_default(),
        ))]
    }

    pub(in crate::app) fn select_streaming_mode(&mut self, mode: StreamingMode) -> Vec<Cmd> {
        self.config.streaming.mode = mode;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.status.text = format!("{}: {}", t!("Streaming", "스트리밍"), mode.label());
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
                    let song = self.queue.current().cloned();
                    return self.load_song(song);
                }
                // Optimistic toggle; mpv confirms via a `pause` property-change.
                self.playback.paused = !self.playback.paused;
                // Manual pause/resume takes over from the video overlay: once the user controls
                // playback themselves, closing the overlay must not auto-resume on their behalf.
                self.video.paused_audio = false;
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::CyclePause)]
            }
            Action::SeekBack => vec![Cmd::Player(PlayerCmd::SeekRelative(
                -self.audio.seek_seconds,
            ))],
            Action::SeekForward => vec![Cmd::Player(PlayerCmd::SeekRelative(
                self.audio.seek_seconds,
            ))],
            Action::VolUp => {
                // A manual volume change takes over from mute, so a later `m` doesn't restore.
                self.playback.pre_mute_volume = None;
                self.playback.volume = (self.playback.volume + VOLUME_STEP).min(VOLUME_MAX);
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(self.playback.volume))]
            }
            Action::VolDown => {
                self.playback.pre_mute_volume = None;
                self.playback.volume = (self.playback.volume - VOLUME_STEP).max(0);
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(self.playback.volume))]
            }
            // mpv-style mute: remember the level and drop to 0; toggling restores it. The
            // volume readout naturally shows 0 while muted, and the change rides the existing
            // SetVolume path so the daemon / OS media session stay in sync.
            Action::ToggleMute => {
                match self.playback.pre_mute_volume.take() {
                    Some(prev) => self.playback.volume = prev,
                    None => {
                        self.playback.pre_mute_volume = Some(self.playback.volume);
                        self.playback.volume = 0;
                    }
                }
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(self.playback.volume))]
            }
            // Manual next: always moves on, even under repeat-one. A manual skip of the
            // current track is a (position-discounted) negative signal before advancing.
            Action::NextTrack => {
                let mut cmds = self.record_outgoing(false);
                cmds.extend(self.advance(false));
                cmds
            }
            Action::PrevTrack => {
                let song = self.queue.prev().cloned();
                self.load_song(song)
            }
            // Cycle the current track's rating through one tri-state control: neutral → 👍 like
            // → 👎 dislike → neutral. `like` is favorite membership (library); `dislike` is the
            // persistent flag the streaming engine treats as a hard block. The two are mutually
            // exclusive, so a single 🤔/👍/👎 glyph (and the `f` key / its click) covers both,
            // replacing the old separate ♥ favorite + ✗ dislike controls. Each leg nudges the
            // artist affinity the engine learns from, and a full cycle nets back to zero.
            Action::CycleRating => {
                if let Some(song) = self.queue.current().cloned() {
                    if song.is_radio_station() {
                        self.library.toggle_favorite(&song);
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
                            let now_fav = self.library.toggle_favorite(&song);
                            self.signals
                                .record_like(&song.video_id, &artist_key, now_fav, now);
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
                            self.library.toggle_favorite(&song);
                            self.signals
                                .record_like(&song.video_id, &artist_key, false, now);
                            self.signals
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
                            self.signals
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
                // Music-mode invariant: turning repeat on while autoplay streaming is on is
                // refused (they can't both be on). Off→All is the only transition that enables it.
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
                    return Vec::new();
                }
                self.queue.cycle_repeat();
                self.dirty = true;
                vec![self.save_playback_modes_cmd()]
            }
            // Cycle the EQ preset and apply it immediately.
            Action::CycleEq => {
                self.audio.preset = self.audio.preset.cycled();
                self.audio.bands = self.audio.preset.gains();
                self.dropdowns.eq_open = false;
                self.dropdowns.streaming_open = false;
                self.dropdowns.search_source_open = false;
                self.status.text = format!("EQ: {}", self.audio.preset.label());
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetAudioFilter(
                    self.current_af().unwrap_or_default(),
                ))]
            }
            Action::ToggleNormalize => {
                self.audio.normalize = !self.audio.normalize;
                self.status.text = format!(
                    "{}: {}",
                    t!("Normalize", "음량 평준화"),
                    if self.audio.normalize { "✓" } else { "✗" }
                );
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetAudioFilter(
                    self.current_af().unwrap_or_default(),
                ))]
            }
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
                        "라디오 방송 중에 녹음 목록이 여기에 표시돼요"
                    )
                    .to_owned();
                }
                self.dirty = true;
                Vec::new()
            }
            Action::OpenSearch => {
                self.mode = Mode::Search;
                self.search.focus = SearchFocus::Input;
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
                            "✓ 링크가 클립보드에 복사됐어요"
                        )
                        .to_owned();
                        self.dirty = true;
                    }
                    None if had_track => {
                        // Current track is genuinely local-only — no YouTube origin to share.
                        self.status.text = t!(
                            "This track is local-only — no YouTube link",
                            "로컬 전용 트랙이라 유튜브 링크가 없어요"
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
        if !self.queue.play_now(song) {
            self.status.kind = StatusKind::Error;
            self.status.text = t!("Queue is full", "큐가 가득 찼어요").to_string();
            self.dirty = true;
            return Vec::new();
        }
        self.mode = Mode::Player;
        self.status.text.clear();
        let song = self.queue.current().cloned();
        self.load_song(song)
    }

    /// Play several tracks now without wiping the queue: insert them immediately after the
    /// current track, jump to the first inserted track, and let the rest follow in order.
    pub(in crate::app) fn play_now_many(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        if songs.is_empty() {
            return Vec::new();
        }
        let requested_songs = songs.clone();
        if self.queue.play_now_many(songs) == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text = t!("Queue is full", "큐가 가득 찼어요").to_string();
            self.dirty = true;
            return Vec::new();
        }
        self.mode = Mode::Player;
        self.status.text.clear();
        let song = self.queue.current().cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.request_romanization_for_songs(&requested_songs));
        cmds
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
        let old_len = self.queue.len();
        let was_idle = self.prefetch.loaded_video_id.is_none();
        let enqueue_next = self.config.effective_enqueue_next() && !was_idle;
        let added = if enqueue_next {
            self.queue.insert_next_many(songs)
        } else {
            self.queue.extend(songs)
        };
        if added == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text = t!("Queue is full", "큐가 가득 찼어요").to_string();
            self.dirty = true;
            return Vec::new();
        }
        if was_idle {
            // Nothing was playing → jump to the first track we just appended and start it.
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            self.mode = Mode::Player;
            self.status.text.clear();
            let song = self.queue.current().cloned();
            let mut cmds = self.load_song(song);
            cmds.extend(self.request_romanization_for_songs(&queued_songs));
            return cmds;
        }
        let cmds = self.request_romanization_for_songs(&queued_songs);
        let first_title = self.display_title(&queued_songs[0]).into_owned();
        // A track is already playing → queue it by policy, with no interruption.
        self.status.kind = StatusKind::Info;
        self.status.text = if requested == 1 && added == 1 {
            let prefix = if enqueue_next {
                t!("Added next:", "다음 곡으로 추가:")
            } else {
                t!("Added to queue:", "큐에 추가:")
            };
            format!("{prefix} {first_title}")
        } else if enqueue_next {
            format!(
                "{} {}",
                added,
                t!("tracks added next", "곡을 다음 곡으로 추가")
            )
        } else {
            format!(
                "{} {}",
                added,
                t!("tracks added to queue", "곡을 큐에 추가")
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
            self.signals
                .record_play(&song.video_id, &artist_key, 1.0, now);
            self.record_session_event(&artist_key, Outcome::FullPlay, 1.0);
        } else {
            let completion = self.playback_completion();
            let scale = self.skip_feedback_scale();
            self.signals
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

    /// Move to the next queue track (auto = end-of-track) and load it, or stop. Also runs
    /// the autoplay/streaming top-up check now that the queue has advanced.
    pub(in crate::app) fn advance(&mut self, auto: bool) -> Vec<Cmd> {
        let song = self.queue.next(auto).cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.maybe_autoplay_extend());
        cmds
    }

    /// Given an optional track, record it in history, reset progress, and emit a load
    /// command (or nothing when the queue produced no track). Always marks the UI dirty.
    pub(in crate::app) fn load_song(&mut self, song: Option<Song>) -> Vec<Cmd> {
        self.dirty = true;
        match song {
            Some(mut song) => {
                let mut skip_budget = self.queue.len();
                let playback_target = loop {
                    if let Some(reason) = song.unplayable_youtube_ref_reason() {
                        tracing::warn!(
                            video_id = %song.video_id,
                            title = %song.title,
                            artist = %song.artist,
                            reason = %reason,
                            "skipping non-playable YouTube entry"
                        );
                        self.status.text = t!(
                            "Skipped a non-playable YouTube entry",
                            "재생할 수 없는 YouTube 항목을 건너뜀"
                        )
                        .to_owned();
                    } else {
                        match song.playback_target_checked() {
                            Ok(target) => break target,
                            Err(error) => {
                                tracing::warn!(
                                    video_id = %song.video_id,
                                    title = %song.title,
                                    artist = %song.artist,
                                    %error,
                                    "skipping track with invalid playback URL"
                                );
                                self.status.text = t!(
                                    "Skipped a track with an invalid playback URL",
                                    "잘못된 재생 URL의 트랙을 건너뜀"
                                )
                                .to_owned();
                            }
                        }
                    }
                    self.status.kind = StatusKind::Error;
                    self.prefetch.loaded_video_id = None;
                    // Skip forward to the next playable track iteratively, bounded to one pass
                    // over the queue. The old form recursed (`load_song` -> `advance` ->
                    // `load_song`), so a run of unplayable refs -- or repeat-all wrapping over an
                    // all-unplayable queue -- drove the stack to overflow.
                    if skip_budget == 0 || self.queue.peek_next().is_none() {
                        return Vec::new();
                    }
                    skip_budget -= 1;
                    let Some(next) = self.queue.next(false).cloned() else {
                        return Vec::new();
                    };
                    song = next;
                };
                self.reset_progress();
                // A new track is a clean slate: drop any stale status (e.g. a prior
                // "Playback error" / "Track unavailable") so the UI matches what's loading.
                self.status.text.clear();
                self.library.record_play(&song);
                if !song.is_radio_station() {
                    self.note_session_activity();
                }
                self.prefetch.loaded_video_id = Some(song.video_id.clone());
                // Drop the previous track's lyrics; refresh if the panel is open.
                self.lyrics.track = None;
                // Drop the previous track's art; a fetch (below) refreshes it when enabled.
                self.clear_artwork();
                // Use a prefetched direct URL if we have one (instant skip); else hand mpv
                // the track's own playback target (watch URL or local file path).
                let prefetched_url = self.prefetch.resolved.get_fresh_url(&song.video_id);
                let (url, prefetched) = match prefetched_url {
                    Some(prefetched) => {
                        match crate::api::validate_playable_url(song.source, &prefetched) {
                            Ok(url) => (url, true),
                            Err(error) => {
                                tracing::warn!(
                                    video_id = %song.video_id,
                                    %error,
                                    "dropping invalid prefetched stream URL"
                                );
                                self.prefetch.resolved.remove(&song.video_id);
                                (playback_target, false)
                            }
                        }
                    }
                    None => (playback_target, false),
                };
                self.prefetch.last_load_prefetched = prefetched;
                tracing::info!(url = %url, prefetched, "load track");
                // Stop any in-progress radio recording BEFORE the new `Load`, so mpv can't
                // append the incoming track onto the previous recording's temp file.
                let mut cmds = self.recorder_teardown();
                cmds.push(Cmd::Player(PlayerCmd::Load(url)));
                cmds.push(Cmd::Persist(PersistCmd::Library));
                // Re-apply the EQ/normalization chain: a gapless graph rebuild on track
                // change can drop the labeled `@eqN` filters, so push it after every load.
                // While the settings screen is open the *draft* is the source of truth (it's
                // been previewing live), so a track change mid-edit keeps mpv matching what
                // the user sees — and leaves the labels in place for the next `af-command`.
                let af = match self.settings.as_deref() {
                    Some(st) => eq::build_af_string(&st.draft.eq_bands, st.draft.normalize),
                    None => self.current_af(),
                };
                if let Some(af) = af {
                    cmds.push(Cmd::Player(PlayerCmd::SetAudioFilter(af)));
                }
                if self.lyrics.visible {
                    self.lyrics.loading = true;
                    cmds.push(fetch_lyrics_cmd(&song));
                }
                // Fetch album art for the new track when the feature is on.
                if let Some(source) = self.artwork_source(&song) {
                    self.art.loading = true;
                    cmds.push(Cmd::FetchArtwork {
                        video_id: song.video_id.clone(),
                        source,
                    });
                }
                // Prefetch the upcoming track's stream so the next skip is instant.
                if let Some(next) = self.queue.peek_next()
                    && let Some(watch_url) = next.prefetch_target()
                {
                    let video_id = next.video_id.clone();
                    if !self.prefetch.resolved.contains_fresh(&video_id) {
                        cmds.push(Cmd::Resolve {
                            video_id,
                            watch_url,
                        });
                    }
                }
                // Start the autoplay-streaming top-up as the track *starts* (not only at its end)
                // so a low/single-song queue fetches its next tracks while this one still
                // plays — closing the silent gap. Guarded + cooldown'd inside, and idempotent
                // with the call in `advance` (the second one sees `streaming.pending` and no-ops).
                cmds.extend(self.maybe_autoplay_extend());
                cmds.extend(self.request_romanization_for_songs(std::slice::from_ref(&song)));
                cmds
            }
            None => {
                self.playback.time_pos = None;
                self.playback.time_pos_at = None;
                self.bump_position_epoch(PositionEpochReason::PlaybackCleared);
                self.playback.duration = None;
                self.playback.paused = true;
                self.playback.stream_now_playing = None;
                self.playback.cache_time = None;
                self.playback.cache_time_at = None;
                self.anim.last_shown_sec = -1;
                self.anim.last_shown_cache_sec = -1;
                self.radio_resync_at = None;
                self.prefetch.loaded_video_id = None;
                self.clear_artwork();
                // Playback cleared: cut any in-progress recording (mid-song → dropped).
                self.recorder_teardown()
            }
        }
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
    /// [`Self::autoplay_streaming`] preference is left untouched by radio; this getter reports
    /// off whenever streaming would be meaningless — in dedicated Radio mode or while a live
    /// station is the current track — so the engine skips top-ups and the status line hides the
    /// `streaming:` indicator, yet the user's saved preference survives the radio round-trip.
    pub fn streaming_active(&self) -> bool {
        self.autoplay_streaming && !self.radio_dedicated_mode && !self.current_is_radio_stream()
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
            Some(b) if b <= LIVE_SYNC_THRESHOLD_SECS => {
                t!("Live: at the live edge", "라이브: 실시간 재생 중").to_owned()
            }
            Some(b) => {
                let key = self.keymap.label_for_display(
                    crate::keymap::KeyContext::Player,
                    Action::CycleRepeat,
                    self.retro_mode(),
                );
                if crate::i18n::is_korean() {
                    format!("라이브: {}초 뒤처짐 — {key} 키로 다시 맞추기", b as i64)
                } else {
                    format!("Live: {}s behind — press {key} to re-sync", b as i64)
                }
            }
            None => t!(
                "Live: sync state unknown",
                "라이브: 동기화 상태를 알 수 없어요"
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
        let Some(song) = self.queue.current().cloned() else {
            return Vec::new();
        };
        let behind = self.radio_behind_secs();
        if let Some(b) = behind
            && b <= LIVE_SYNC_THRESHOLD_SECS
            && !self.playback.paused
        {
            self.status.kind = StatusKind::Info;
            self.status.text = t!("Live: at the live edge", "라이브: 실시간 재생 중").to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let seek_failed_recently = self
            .radio_resync_at
            .is_some_and(|at| at.elapsed().as_secs_f64() < RESYNC_RETRY_WINDOW_SECS);
        self.radio_resync_at = Some(Instant::now());
        self.status.kind = StatusKind::Info;
        if let Some(edge) = self.playback.cache_time
            && behind.is_some()
            && !seek_failed_recently
        {
            // Optimistic unpause, like TogglePause: mpv confirms via `pause`.
            let mut cmds = Vec::new();
            if self.playback.paused {
                self.playback.paused = false;
                self.video.paused_audio = false;
                cmds.push(Cmd::Player(PlayerCmd::CyclePause));
            }
            cmds.push(Cmd::Player(PlayerCmd::SeekAbsolute(
                (edge - LIVE_EDGE_SEEK_MARGIN_SECS).max(0.0),
            )));
            self.status.text = t!("Re-synced to live", "실시간으로 다시 맞췄어요").to_owned();
            self.dirty = true;
            return cmds;
        }
        // No usable edge (cache-less stream) or the seek didn't take → reconnect. A fresh
        // connection starts at the live edge by construction; `load_song` resets progress
        // (incl. `paused = false`).
        self.status.text = t!(
            "Reconnected to the live stream",
            "라이브 스트림에 다시 연결했어요"
        )
        .to_owned();
        self.dirty = true;
        self.load_song(Some(song))
    }

    /// Whether we lack lyrics for the current track (so a fetch is warranted).
    pub(in crate::app) fn lyrics_stale(&self) -> bool {
        match (&self.lyrics.track, self.queue.current()) {
            (Some(l), Some(cur)) => l.video_id != cur.video_id,
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
