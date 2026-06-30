//! Player/playback reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
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
    pub fn close_video(&mut self) {
        if let Some(mut child) = self.video.proc.take() {
            let _ = child.kill();
            let _ = child.wait();
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
        let key = song.title.trim().to_lowercase();
        self.library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .chain(self.library_ui.downloaded.iter())
            .find(|e| e.youtube_id().is_some() && e.title.trim().to_lowercase() == key)
            .and_then(|e| e.youtube_id().map(str::to_owned))
    }

    /// `v`: toggle the external mpv video overlay. Open → close it and resume the audio we
    /// paused; closed → launch it for the current track and pause the audio.
    pub(in crate::app) fn toggle_video_overlay(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if self.video_open() {
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
            self.status.text = t!("Video closed", "영상 닫음").to_owned();
        } else if let Some(song) = self.queue.current().cloned() {
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
            let url = format!("https://www.youtube.com/watch?v={id}");
            let cookies = self.config.cookies_file.clone();
            match spawn_video_overlay(&url, cookies.as_deref(), self.config.video_layout) {
                Some(child) => {
                    self.video.proc = Some(child);
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
                }
                None => {
                    self.status.text =
                        t!("Failed to launch mpv", "mpv 실행에 실패했습니다").to_owned();
                }
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
        let mut cmds = vec![Cmd::SaveConfig(Box::new(self.config.clone()))];
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
                    let url = format!("https://www.youtube.com/watch?v={id}");
                    let cookies = self.config.cookies_file.clone();
                    self.video.proc = spawn_video_overlay(&url, cookies.as_deref(), layout);
                    // Audio stays paused (video.paused_audio unchanged).
                }
                None => {
                    if self.video.paused_audio {
                        self.video.paused_audio = false;
                        self.playback.paused = false;
                        cmds.push(Cmd::Player(PlayerCmd::SetProperty {
                            name: "pause".to_owned(),
                            value: serde_json::Value::Bool(false),
                        }));
                    }
                    self.status.kind = StatusKind::Info;
                    self.status.text = t!(
                        "This track is local-only — no video",
                        "로컬 전용 트랙이라 영상이 없어요"
                    )
                    .to_owned();
                    self.dirty = true;
                    return cmds;
                }
            }
        }
        self.status.kind = StatusKind::Info;
        self.status.text = format!("{}: {}", t!("Video", "영상"), layout.label());
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

    pub(in crate::app) fn select_radio_mode(&mut self, mode: RadioMode) -> Vec<Cmd> {
        self.config.radio.mode = mode;
        self.dropdowns.radio_open = false;
        self.dropdowns.search_source_open = false;
        self.status.text = format!("{}: {}", t!("Radio", "라디오"), mode.label());
        self.dirty = true;
        vec![Cmd::SaveConfig(Box::new(self.config.clone()))]
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
                self.playback.volume = (self.playback.volume + VOLUME_STEP).min(VOLUME_MAX);
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(self.playback.volume))]
            }
            Action::VolDown => {
                self.playback.volume = (self.playback.volume - VOLUME_STEP).max(0);
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
            // persistent flag the radio engine treats as a hard block. The two are mutually
            // exclusive, so a single 🤔/👍/👎 glyph (and the `f` key / its click) covers both,
            // replacing the old separate ♥ favorite + ✗ dislike controls. Each leg nudges the
            // artist affinity the engine learns from, and a full cycle nets back to zero.
            Action::CycleRating => {
                if let Some(song) = self.queue.current().cloned() {
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
                            return vec![Cmd::SaveLibrary, Cmd::SaveSignals];
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
                            return vec![Cmd::SaveLibrary, Cmd::SaveSignals];
                        }
                        // dislike → neutral: clear the flag, restoring the affinity it pushed down.
                        (false, true) => {
                            self.signals
                                .toggle_dislike(&song.video_id, &artist_key, now);
                            self.dirty = true;
                            return vec![Cmd::SaveSignals];
                        }
                    }
                }
                Vec::new()
            }
            Action::OpenLibrary => {
                self.mode = Mode::Library;
                // Start each library visit with a clean, unfiltered list (also resets the
                // cursor, the multi-select anchor, and the scroll offset).
                self.clear_library_filter();
                self.dropdowns.eq_open = false;
                self.dropdowns.radio_open = false;
                self.dropdowns.search_source_open = false;
                self.dirty = true;
                Vec::new()
            }
            Action::OpenQueue => {
                self.open_queue_popup();
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
            Action::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.dirty = true;
                Vec::new()
            }
            Action::CycleRepeat => {
                self.queue.cycle_repeat();
                self.dirty = true;
                Vec::new()
            }
            // Cycle the EQ preset and apply it immediately.
            Action::CycleEq => {
                self.audio.preset = self.audio.preset.cycled();
                self.audio.bands = self.audio.preset.gains();
                self.dropdowns.eq_open = false;
                self.dropdowns.radio_open = false;
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
            Action::OpenSearch => {
                self.mode = Mode::Search;
                self.search.focus = SearchFocus::Input;
                self.dropdowns.eq_open = false;
                self.dropdowns.radio_open = false;
                self.dropdowns.search_source_open = false;
                self.dirty = true;
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
        if self.queue.play_now_many(songs) == 0 {
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

    /// Append `song` to the end of the queue without interrupting playback — the unified `\` /
    /// right-click "add to queue" gesture in the Library and Search results. If nothing is
    /// currently playing we jump to it and start; if a track is already playing we simply
    /// enqueue it (no interruption) and confirm with a toast.
    pub(in crate::app) fn enqueue(&mut self, song: Song) -> Vec<Cmd> {
        self.enqueue_many(vec![song])
    }

    /// Append several tracks to the end of the queue without interrupting playback. If idle,
    /// start the first appended track.
    pub(in crate::app) fn enqueue_many(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        if songs.is_empty() {
            return Vec::new();
        }
        let requested = songs.len();
        let first_title = songs[0].title.clone();
        let old_len = self.queue.len();
        let was_idle = self.prefetch.loaded_video_id.is_none();
        let added = self.queue.extend(songs);
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
            return self.load_song(song);
        }
        // A track is already playing → just queue it up behind the rest, no interruption.
        self.status.kind = StatusKind::Info;
        self.status.text = if requested == 1 && added == 1 {
            format!("{} {}", t!("Added to queue:", "큐에 추가:"), first_title)
        } else {
            format!(
                "{} {}",
                added,
                t!("tracks added to queue", "곡을 큐에 추가")
            )
        };
        self.dirty = true;
        Vec::new()
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
        let mut cmds = vec![Cmd::SaveSignals];
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
    /// [`SESSION_EVENTS_CAP`]. Feeds the AI reranker's recovery context.
    pub(in crate::app) fn record_session_event(
        &mut self,
        artist_key: &str,
        outcome: Outcome,
        completion: f32,
    ) {
        let buf = &mut self.radio.session_events;
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
    /// the autoplay/radio top-up check now that the queue has advanced.
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
            Some(song) => {
                self.reset_progress();
                // A new track is a clean slate: drop any stale status (e.g. a prior
                // "Playback error" / "Track unavailable") so the UI matches what's loading.
                self.status.text.clear();
                self.library.record_play(&song);
                self.note_session_activity();
                self.prefetch.loaded_video_id = Some(song.video_id.clone());
                // Drop the previous track's lyrics; refresh if the panel is open.
                self.lyrics.track = None;
                // Drop the previous track's art; a fetch (below) refreshes it when enabled.
                self.clear_artwork();
                // Use a prefetched direct URL if we have one (instant skip); else hand mpv
                // the track's own playback target (watch URL or local file path).
                let prefetched = self.prefetch.resolved.contains_key(&song.video_id);
                self.prefetch.last_load_prefetched = prefetched;
                let url = self
                    .prefetch
                    .resolved
                    .get(&song.video_id)
                    .cloned()
                    .unwrap_or_else(|| song.playback_target());
                tracing::info!(url = %url, prefetched, "load track");
                let mut cmds = vec![Cmd::Player(PlayerCmd::Load(url)), Cmd::SaveLibrary];
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
                    if !self.prefetch.resolved.contains_key(&video_id) {
                        cmds.push(Cmd::Resolve {
                            video_id,
                            watch_url,
                        });
                    }
                }
                // Start the autoplay-radio top-up as the track *starts* (not only at its end)
                // so a low/single-song queue fetches its next tracks while this one still
                // plays — closing the silent gap. Guarded + cooldown'd inside, and idempotent
                // with the call in `advance` (the second one sees `radio.pending` and no-ops).
                cmds.extend(self.maybe_autoplay_extend());
                cmds
            }
            None => {
                self.playback.time_pos = None;
                self.playback.duration = None;
                self.playback.paused = true;
                self.last_shown_sec = -1;
                self.prefetch.loaded_video_id = None;
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn current_needs_load(&self) -> bool {
        self.queue.current().is_some_and(|song| {
            self.prefetch.loaded_video_id.as_deref() != Some(song.video_id.as_str())
        })
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
        self.playback.duration = None;
        self.playback.paused = false;
        self.last_shown_sec = -1;
    }
}
