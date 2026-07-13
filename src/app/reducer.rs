//! Top-level TEA reducer wrapper and message dispatcher.

use std::time::Instant;

use super::*;

impl App {
    /// The reducer: apply one message, returning effects for the run loop to dispatch.
    /// Reducer entry point. Wraps [`Self::dispatch`] to centrally track when a transient
    /// `status` notification is set or cleared (any of the ~40 `self.status.text = …` sites), so
    /// the main loop can expire it after [`STATUS_TTL`] and bring the song title back —
    /// without each call site having to remember to arm a timer. See [`Self::status_visible`].
    pub fn update(&mut self, msg: impl Into<Msg>) -> Vec<Cmd> {
        let msg = msg.into();
        // Rendering is the only place that knows the real cell grid (text zoom can change it
        // without a terminal resize). Apply the bridged tier before routing this event so a
        // hidden Search/DJ input cannot consume the first global key after entering Mini.
        self.sync_ui_tier();
        let admitted_seek = matches!(
            &msg,
            Msg::Player(PlayerMsg::IntentAdmitted(commit)) if commit.is_seek()
        );
        let mut status_before = std::mem::take(&mut self.status_text_prev);
        status_before.clear();
        status_before.push_str(&self.status.text);
        let kind_before = self.status.kind;
        let paused_before = self.playback.paused;
        let animations_were_on = self.animations().master;
        // Default this turn's status to the error styling; the few positive handlers override
        // it to `Info` while they run. This keeps the kind in lock-step with the status text:
        // an error set by one of the ~40 plain `self.status.text = …` sites can never inherit a
        // leftover `Info` color from a previous green toast.
        self.status.kind = StatusKind::Error;
        let cmds = self.dispatch(msg);
        if self.status.text.is_empty()
            && let Some(warning) = self.recorder.health_warning.as_ref()
        {
            self.status.kind = StatusKind::Error;
            self.status.text.clone_from(warning);
        }
        let status_changed = self.status.text != status_before;
        if status_changed {
            let persistent_recorder_health =
                self.recorder.health_warning.as_deref() == Some(self.status.text.as_str());
            self.status.set_at = if self.status.text.is_empty() || persistent_recorder_health {
                None
            } else {
                Some(Instant::now())
            };
        } else {
            // Text unchanged this turn — keep the color the still-showing message already had.
            self.status.kind = kind_before;
        }
        // Media-session position clock, kept centrally so no seek/pause site can forget it:
        // any seek command emitted this turn is a position discontinuity (bump the epoch so
        // the OS session re-announces the position), and any pause/resume flip rebases the
        // interpolation anchor so a long pause never reads as elapsed progress.
        if admitted_seek {
            self.bump_position_epoch(PositionEpochReason::Seek);
        }
        if self.playback.paused != paused_before {
            self.playback.time_pos_at = Some(Instant::now());
        }
        // One-shot animation feedback, detected centrally for the same reason as the status
        // TTL above: every input path (key, mouse, remote, DJ Gem) changes the same state, so
        // diffing it here means no call site can forget to trigger the matching effect.
        self.detect_fx(status_changed, admitted_seek);
        if animations_were_on && !self.animations().master {
            self.fx.cancel();
        }
        self.sync_art_overlay_state();
        self.sync_art_geometry();
        self.sync_ui_tier();
        self.status_text_prev = status_before; // return the buffer's capacity for next turn
        cmds
    }

    fn dispatch(&mut self, msg: Msg) -> Vec<Cmd> {
        if self.tool_setup.is_some()
            && matches!(
                &msg,
                Msg::MouseDoubleClick { .. }
                    | Msg::MouseRightClick { .. }
                    | Msg::MouseRightDoubleClick { .. }
                    | Msg::MouseDrag { .. }
                    | Msg::MouseLeftUp
                    | Msg::MouseScroll { .. }
            )
        {
            return Vec::new();
        }
        match msg {
            Msg::Noop => return Vec::new(),
            Msg::Key(k) => return self.on_key(k),
            Msg::MouseClick { col, row, multi } => return self.on_mouse_click(col, row, multi),
            Msg::MouseDoubleClick { col, row } => return self.on_mouse_double_click(col, row),
            Msg::MouseRightClick { col, row } => return self.on_mouse_right_click(col, row),
            Msg::MouseRightDoubleClick { col, row } => {
                return self.on_mouse_right_double_click(col, row);
            }
            Msg::MouseDrag { col, row } => return self.on_mouse_drag(col, row),
            Msg::MouseLeftUp => return self.on_mouse_left_up(),
            Msg::MouseScroll { up, col, row, ctrl } => {
                return self.on_mouse_scroll(up, col, row, ctrl);
            }
            Msg::Resize => {
                // A centered art band moves with the grid height; ratatui's diff can't
                // repaint parked native-image bytes, so resync with one full clear.
                if self.native_art_active() {
                    self.request_native_image_clear();
                }
                self.dirty = true;
            }
            Msg::Quit => self.should_quit = true,
            Msg::Remote(cmd, reply) => {
                if let crate::remote::proto::RemoteCommand::ExportPersonalData { directory } = cmd {
                    return self.start_personal_export(PathBuf::from(directory), Some(reply));
                }
                let deferred = Self::remote_reply_plan(&cmd);
                let (resp, mut cmds) = self.apply_remote(cmd);
                // Admission-sensitive success snapshots wait for their player intent, while a
                // command rejected during validation (for example an empty resume session) must
                // preserve its explicit error instead of being replaced by a fake status reply.
                let response = if resp.ok {
                    deferred.unwrap_or(RemoteReplyPlan::Fixed(Box::new(resp)))
                } else {
                    RemoteReplyPlan::Fixed(Box::new(resp))
                };
                if let Err((reply, response)) =
                    Self::attach_remote_reply(&mut cmds, reply, response)
                {
                    let _ = reply.send(self.resolve_remote_reply(response));
                }
                return cmds;
            }
            Msg::Data(DataMsg::PersonalDataExport(PersonalDataExportMsg::Finished {
                result,
                reply,
            })) => {
                return self.finish_personal_export(result, reply);
            }
            Msg::Media(cmd) => return self.apply_media(cmd),
            Msg::MediaArtworkReady(ready) => {
                // No redraw: this only feeds the OS media session, not the TUI.
                self.media_art = Some(ready);
            }
            Msg::Autoplay => return self.autoplay_on_start_cmds(),
            Msg::ApiModeResolved { mode, had_cookie } => {
                self.authenticated = mode == ApiMode::Authenticated;
                if mode == ApiMode::Anonymous && had_cookie {
                    self.status.text = crate::t!(
                        "Cookie rejected — anonymous mode (search & play only)",
                        "쿠키가 거부됨 — 익명 모드 (검색·재생만 가능)"
                    )
                    .to_owned();
                }
                self.dirty = true;
                let results = self.search.results.clone();
                return self.request_romanization_for_songs(&results);
            }
            Msg::StatusTick => {
                // Transient status expires back to the recorder's persistent recovery/
                // backpressure condition. That condition clears only when its exact source is
                // settled or explicitly discarded, never merely because three seconds elapsed.
                if matches!(self.status.set_at, Some(t) if t.elapsed() >= STATUS_TTL) {
                    if let Some(warning) = self.recorder.health_warning.as_ref() {
                        if self.status.text != *warning {
                            self.status.kind = StatusKind::Error;
                            self.status.text.clone_from(warning);
                            self.dirty = true;
                        } else {
                            self.status.set_at = None;
                        }
                    } else {
                        self.status.text.clear();
                        self.dirty = true;
                    }
                }
                return self.tick_search_onboarding(Instant::now());
            }
            Msg::AnimTick => {
                // Advance the logical animation phase on every configured tick, but only request
                // an actual terminal redraw when the active effect mix is due. This keeps visual
                // timing stable while cutting the expensive render/terminal/compositor path.
                self.advance_animation();
            }
            Msg::Focus(f) => {
                // Terminal focus toggled. `animation_active()` reads `focused` to park the ~30 fps
                // tick while we're hidden; one redraw repaints cleanly on the transition (freeze a
                // tidy frame on blur, resume instantly on focus). The seekbar keeps advancing via
                // `PlayerTimePos`, which is independent of this tick.
                self.focused = f;
                self.dirty = true;
            }
            Msg::Player(pm) => match pm {
                PlayerMsg::TimePos(t) => {
                    // Normalize at the mpv trust boundary: a NaN/inf/negative time-pos must not
                    // reach the interpolation clock, the OS media session, or the seekbar gauge.
                    let t = crate::playback_policy::norm_position(t);
                    self.playback.time_pos = Some(t);
                    self.playback.time_pos_at = Some(Instant::now());
                    // Real progress means the current track opened and is playing, so the
                    // auto-skip streak is broken — clear it.
                    if t > 0.0 {
                        self.consecutive_play_errors = 0;
                    }
                    // Keep the whole-second anchor current in every mode, but only redraw when
                    // the Player view that renders the scalar is visible. Navigation back to the
                    // Player already requests a frame, which reads the latest stored position.
                    let sec = t as i64;
                    if sec != self.anim.last_shown_sec {
                        self.anim.last_shown_sec = sec;
                        if matches!(self.mode, Mode::Player) {
                            self.dirty = true;
                        }
                        tracing::debug!(time_pos = t, "progress");
                    }
                }
                PlayerMsg::Duration(d) => {
                    self.playback.duration = d.map(crate::playback_policy::norm_duration);
                    self.dirty = true;
                }
                PlayerMsg::CacheTime(t) => {
                    let t = t.map(crate::playback_policy::norm_position);
                    let had = self.playback.cache_time.is_some();
                    self.playback.cache_time = t;
                    self.playback.cache_time_at = t.map(|_| Instant::now());
                    // Track whole seconds and Some↔None transitions in every mode. Only the
                    // Player renders the live-sync state, and returning there requests a frame.
                    let sec = t.map_or(-1, |v| v as i64);
                    if sec != self.anim.last_shown_cache_sec || had != t.is_some() {
                        self.anim.last_shown_cache_sec = sec;
                        if matches!(self.mode, Mode::Player) {
                            self.dirty = true;
                        }
                    }
                }
                PlayerMsg::AudioCodec(codec) => {
                    // Passthrough container hint for the recorder; no redraw needed.
                    self.playback.audio_codec = codec;
                }
                PlayerMsg::FileFormat(format) => {
                    self.playback.file_format = format;
                }
                PlayerMsg::AudioDeviceList(devices) => {
                    return self.on_audio_device_list(devices);
                }
                PlayerMsg::AudioDeviceRefreshFailed(error) => {
                    return self.on_audio_device_refresh_failed(error);
                }
                PlayerMsg::AudioDeviceChanged(device) => {
                    return self.on_audio_device_changed(device);
                }
                PlayerMsg::CurrentAudioOutput(output) => {
                    return self.on_current_audio_output(output);
                }
                PlayerMsg::AudioDeviceSelectionResult {
                    correlation_id,
                    device,
                    result,
                } => {
                    return self.finish_audio_device_selection(correlation_id, device, result);
                }
                PlayerMsg::Paused(p) => {
                    self.playback.paused = p;
                    self.dirty = true;
                }
                PlayerMsg::Volume(v) => {
                    // A non-finite report is ignored (leave the current level) rather than
                    // muting (`NaN.round() as i64` == 0) or storing a garbage level.
                    if let Some(volume) = crate::playback_policy::norm_volume_event(v) {
                        self.playback.volume = volume;
                        self.dirty = true;
                        tracing::info!(volume = self.playback.volume, "volume");
                    }
                }
                PlayerMsg::Metadata(metadata) => {
                    let parsed = self.queue.current().cloned().and_then(|song| {
                        if !song.is_radio_station() {
                            return None;
                        }
                        let station_label = self.display_song_label(&song);
                        stream_metadata::parse_stream_now_playing(
                            &metadata,
                            &[song.title.as_str(), station_label.as_str()],
                        )
                    });
                    if self.playback.stream_now_playing != parsed {
                        self.playback.stream_now_playing = parsed.clone();
                        self.dirty = true;
                        // Rotate the recorder first (finalize the ended track, start the next),
                        // then let the overlay re-populate from the fresh ICY title (a
                        // favorite-resolve in flight for the old title is now stale).
                        let mut cmds = self.recorder_on_title(parsed.as_ref());
                        cmds.extend(self.on_stream_title_changed());
                        return cmds;
                    }
                }
                PlayerMsg::Eof => {
                    tracing::info!("track ended (eof)");
                    // The just-finished track played to its end → a full-play signal, then advance.
                    return self.advance_with_outgoing(true, true);
                }
                PlayerMsg::VideoOverlay { generation, event } => {
                    return self.on_video_overlay_event(generation, event);
                }
                PlayerMsg::Error(e) => return self.on_player_error(e),
                PlayerMsg::TransportClosed(reason) => {
                    self.audio_output_transport_closed(&reason);
                    return self.recover_player_transport(reason);
                }
                PlayerMsg::IntentAdmitted(commit) => {
                    return self.commit_player_intent(commit);
                }
            },
            Msg::RecordingTick => {
                return self.recorder_on_tick();
            }
            Msg::Recorder(event) => {
                return self.on_recorder_event(event);
            }
            Msg::TrackResolved { seq, result } => {
                return self.on_track_resolved(seq, result);
            }
            Msg::PlaylistTracks {
                title,
                intent,
                songs,
            } => {
                return self.on_playlist_tracks(title, intent, songs);
            }
            Msg::PlaylistTracksError { title, error } => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!("{title}: {error}");
                self.dirty = true;
            }
            Msg::YtdlpHealResult { video_id, updated } => {
                if self.heal.pending_video_id.as_deref() != Some(video_id.as_str()) {
                    return Vec::new(); // stale: the user already moved on
                }
                let still_current = self.queue.current().is_some_and(|s| s.video_id == video_id);
                if updated && still_current {
                    // A fresh binary landed. Resolve a direct URL with it (StreamingMsg::Resolved
                    // below finishes the retry); Msg::ResolveFailed ends the heal.
                    let watch_url = self.queue.current().and_then(Song::prefetch_target);
                    if let Some(watch_url) = watch_url {
                        return vec![Cmd::ResolveForSelfHeal {
                            video_id,
                            watch_url,
                        }];
                    }
                }
                // No update available / track changed — give up on this heal and skip
                // like the plain error path would have.
                self.heal.pending_video_id = None;
                if !still_current {
                    return Vec::new();
                }
                self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
                let mut cmds = if self.queue.peek_next().is_some() {
                    self.advance(false)
                } else {
                    Vec::new()
                };
                self.status.kind = StatusKind::Error;
                self.status.text = t!(
                    "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
                    "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀"
                )
                .to_owned();
                Self::attach_track_commit_status(
                    &mut cmds,
                    StatusKind::Error,
                    self.status.text.clone(),
                );
                self.dirty = true;
                return cmds;
            }
            Msg::ResolveFailed { video_id } => {
                // Only meaningful while a self-heal retry waits on this exact resolve;
                // ordinary prefetch failures were already logged by the resolver.
                if self.heal.pending_video_id.as_deref() != Some(video_id.as_str()) {
                    return Vec::new();
                }
                self.heal.pending_video_id = None;
                if self.queue.current().is_none_or(|s| s.video_id != video_id) {
                    return Vec::new();
                }
                self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
                let mut cmds = if self.queue.peek_next().is_some() {
                    self.advance(false)
                } else {
                    Vec::new()
                };
                self.status.kind = StatusKind::Error;
                self.status.text = t!(
                    "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
                    "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀"
                )
                .to_owned();
                Self::attach_track_commit_status(
                    &mut cmds,
                    StatusKind::Error,
                    self.status.text.clone(),
                );
                self.dirty = true;
                return cmds;
            }
            Msg::SearchResults {
                request_id,
                query,
                songs,
                timed_out,
                ..
            } => {
                // Drop results from a superseded search: a slow older response must never
                // overwrite a newer one, even after the newer one already cleared `searching`.
                // The request id is authoritative — comparing the live `input`/`source` would
                // wrongly reject the current search's results the moment the user types more
                // (or changes the source) without submitting, since those change without a
                // new request.
                if request_id != self.search.request_id {
                    return Vec::new();
                }
                self.search.searching = false;
                // The filter popup indexes into the rows it opened over; a fresh result
                // set makes those stale, so it closes rather than filtering the new list.
                self.search_filter.close();
                if songs.is_empty() {
                    self.status.text = if crate::i18n::is_korean() {
                        format!("\"{query}\" 검색 결과 없음")
                    } else {
                        format!("No results for \"{query}\"")
                    };
                    self.search.results.clear();
                    self.search.selected = 0;
                    self.collapse_search_selection();
                } else {
                    // A partial result set (the operation deadline dropped a slow source) gets a
                    // subtle note so it doesn't read as the complete set; a full result clears it.
                    self.status.text = if timed_out {
                        t!("Some sources timed out", "일부 소스 시간 초과").to_string()
                    } else {
                        String::new()
                    };
                    self.search.results = songs;
                    self.search.selected = 0;
                    self.collapse_search_selection();
                    self.bridges.search_scroll.reset();
                    self.search.focus = SearchFocus::Results;
                }
                self.dirty = true;
            }
            Msg::SearchError {
                request_id, error, ..
            } => {
                // Same stale-guard as SearchResults: a failed older search must not clear the
                // status or `searching` flag of a newer one still in flight.
                if request_id != self.search.request_id {
                    return Vec::new();
                }
                self.search.searching = false;
                self.status.text = format!("{}: {error}", t!("Search error", "검색 오류"));
                self.dirty = true;
            }
            Msg::Data(DataMsg::DownloadsScanned(scan)) => {
                self.library_ui.downloaded_rev = self.library_ui.downloaded_rev.wrapping_add(1);
                let truncated = scan.truncated;
                let limit = scan.limit;
                let downloaded = self.enrich_downloads(scan.songs);
                let romanize_cmds = self.request_romanization_for_songs(&downloaded);
                self.library_ui.downloaded = downloaded;
                let len = self.library_len();
                if self.library_ui.selected >= len {
                    self.library_ui.selected = len.saturating_sub(1);
                }
                if truncated {
                    self.status.text = format!(
                        "{} {limit} {}",
                        t!("Showing first", "처음"),
                        t!(
                            "download files; more are hidden",
                            "개 다운로드 파일만 표시됨; 일부는 숨김"
                        )
                    );
                }
                self.dirty = true;
                return romanize_cmds;
            }
            Msg::Local(msg) => return self.apply_local_msg(msg),
            Msg::LyricsResult { video_id, lines } => {
                self.lyrics.loading = false;
                // Ignore stale results for a track we've already skipped past.
                if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                    self.lyrics.track = Some(TrackLyrics { video_id, lines });
                    self.dirty = true;
                }
            }
            Msg::ArtworkResult { video_id, image } => {
                self.art.loading = false;
                // Drop results for a track we've already skipped past.
                if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                    self.set_artwork(video_id, image);
                    self.dirty = true;
                }
            }
            Msg::ArtworkResized(response) => self.apply_artwork_resize(response),
            Msg::Download(message) => match message {
                DownloadMsg::Progress { video_id, percent } => {
                    self.apply_download_progress(video_id, percent);
                }
                DownloadMsg::ImportProgress { context, percent } => {
                    self.apply_download_progress(context.tracking_key(), percent);
                }
                DownloadMsg::Done { video_id, path } => {
                    return self.apply_download_done(video_id, path);
                }
                DownloadMsg::ImportDone { context, path } => {
                    return self.apply_download_done(context.tracking_key(), path);
                }
                DownloadMsg::Error { video_id, error } => {
                    return self.apply_download_error(video_id, error);
                }
                DownloadMsg::ImportError { context, error } => {
                    return self.apply_download_error(context.tracking_key(), error);
                }
                DownloadMsg::Rejected {
                    tracking_key,
                    error,
                } => return self.apply_download_error(tracking_key, error),
                DownloadMsg::DirError { error } => {
                    self.status.kind = StatusKind::Error;
                    self.status.text = format!(
                        "{}: {error}",
                        t!(
                            "Download directory update failed",
                            "다운로드 폴더 변경 실패"
                        )
                    );
                    self.dirty = true;
                }
            },
            Msg::DownloadsDeleted {
                root,
                deleted,
                failed,
            } => return self.apply_deleted_downloads(root, deleted, failed),
            Msg::PersistFailed { store, error } => {
                self.status.kind = StatusKind::Error;
                self.status.text = if crate::i18n::is_korean() {
                    format!("저장 실패 ({}): {error}", store.label())
                } else {
                    format!("Save failed ({}): {error}", store.label())
                };
                self.dirty = true;
            }
            Msg::Streaming(sm) => match sm {
                StreamingMsg::Resolved {
                    video_id,
                    stream_url,
                    self_heal,
                } => {
                    let healing = self_heal
                        && self.heal.pending_video_id.as_deref() == Some(video_id.as_str());
                    if !healing && !self.prefetch.enabled() {
                        tracing::debug!(
                            video_id = %video_id,
                            "dropping resolved stream while prefetch is paused"
                        );
                        return Vec::new();
                    }
                    // Bounded prefetch cache; no redraw (purely a skip-latency optimization).
                    self.prefetch.resolved.insert(video_id.clone(), stream_url);
                    // A pending self-heal retry: the freshly-updated yt-dlp resolved the
                    // failed track — reload it now through the direct CDN URL just cached
                    // (bypassing the session mpv's stale spawn-time ytdl_hook).
                    if healing {
                        if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                            return self.reload_healed_track(video_id);
                        }
                        // The user moved on while resolution was in flight. No player command is
                        // needed, so this stale background result can retire immediately.
                        self.heal.pending_video_id = None;
                    }
                }
                StreamingMsg::Results {
                    seed_video_id,
                    candidates,
                } => {
                    self.streaming.pending = false;
                    if self.autoplay_streaming && self.queue.contains_video_id(&seed_video_id) {
                        // With a key + reranker enabled, hand the model a diverse local shortlist to
                        // reorder (ids only); otherwise rank the pool purely locally. Either way the
                        // pool went through scoring + MMR + cooldown — never taken verbatim.
                        if self.ai.available && self.config.streaming.ai.enabled {
                            return self.start_ai_rerank(&seed_video_id, candidates);
                        }
                        let picks = self.plan_local_streaming(&seed_video_id, candidates);
                        return self.extend_sanitized_streaming(&seed_video_id, picks, &[]);
                    } else {
                        self.dirty = true;
                    }
                }
                StreamingMsg::Preflighted {
                    seed_video_id,
                    songs,
                } => {
                    self.streaming.pending = false;
                    if self.autoplay_streaming && self.queue.contains_video_id(&seed_video_id) {
                        return self.extend_queue_from_streaming(songs);
                    }
                    self.dirty = true;
                }
                StreamingMsg::AiPicks {
                    seed_video_id,
                    picks,
                    conf,
                } => return self.on_streaming_ai_picks(seed_video_id, picks, conf),
                StreamingMsg::Error {
                    seed_video_id,
                    error,
                } => {
                    self.streaming.pending = false;
                    if self.autoplay_streaming && self.queue.contains_video_id(&seed_video_id) {
                        return self.note_streaming_failure(format!(
                            "{}: {error}",
                            t!("Autoplay failed", "자동재생 실패")
                        ));
                    } else {
                        self.dirty = true;
                    }
                }
            },
            // --- DJ Gem assistant intents ---------------------------------------
            Msg::Ai(am) => match am {
                AiMsg::Thinking(on) => {
                    self.ai.thinking = on;
                    self.bridges.ai_transcript_scroll.scroll_to_end();
                    self.dirty = true;
                }
                AiMsg::Chat(text) => {
                    // Skip empty replies (e.g. a silent autoplay top-up that only ran tools).
                    if !text.trim().is_empty() {
                        self.push_ai_message(AiRole::Ai, text);
                        self.dirty = true;
                    }
                }
                AiMsg::Error(text) => {
                    self.ai.thinking = false;
                    self.push_ai_message(AiRole::Error, text);
                    self.dirty = true;
                }
                AiMsg::PlayTracks(songs) => {
                    if !songs.is_empty() {
                        return self.replace_queue_and_load(
                            songs,
                            0,
                            None,
                            QueueReplacementOptions {
                                romanize_all: true,
                                ..QueueReplacementOptions::default()
                            },
                        );
                    }
                }
                AiMsg::Enqueue(songs) => {
                    return self.extend_queue_from_streaming(songs);
                }
                AiMsg::Suggestions(songs) => {
                    let cmds = self.request_romanization_for_songs(&songs);
                    self.ai.suggestions = songs;
                    self.ai.suggestions_selected = 0;
                    self.bridges.ai_scroll.reset();
                    self.dirty = true;
                    return cmds;
                }
                AiMsg::SetAutoplay(on) => {
                    // Music-mode invariant: DJ Gem can't enable autoplay while repeat is on.
                    let on = on && self.queue.repeat == crate::queue::Repeat::Off;
                    self.set_autoplay_streaming(on);
                    self.dirty = true;
                    let mut cmds = vec![self.save_playback_modes_cmd()];
                    if on {
                        // Same proactive top-up as the manual toggle (see Action::ToggleStreaming).
                        cmds.extend(self.maybe_autoplay_extend());
                    }
                    return cmds;
                }
                AiMsg::SetStationProfile {
                    query,
                    explore,
                    avoid_artists,
                } => {
                    // Distill the vibe into engine knobs the local streaming can actually act on:
                    // adventurousness (→ mode) and artists to keep out (→ banned_artist_keys, read
                    // live in `build_station_state`). Persist both so the station survives a restart.
                    let profile = crate::station::StationProfile::from_intent(
                        &query,
                        explore.as_deref(),
                        &avoid_artists,
                    );
                    self.config.streaming.mode = profile.explore.to_mode();
                    self.station.active = Some(profile);
                    self.dirty = true;
                    return vec![
                        Cmd::Persist(PersistCmd::StationProfile),
                        Cmd::Persist(PersistCmd::Config(Box::new(self.config.clone()))),
                    ];
                }
                AiMsg::CreatePlaylist(name) => {
                    if self.playlists.create(&name).is_some() {
                        self.dirty = true;
                        return vec![Cmd::Persist(PersistCmd::Playlists)];
                    }
                }
                AiMsg::AddToPlaylist { playlist, songs } => {
                    let mut any = false;
                    for song in songs {
                        if matches!(
                            self.playlists.add(&playlist, song),
                            crate::playlists::AddResult::Added
                        ) {
                            any = true;
                        }
                    }
                    if any {
                        self.dirty = true;
                        return vec![Cmd::Persist(PersistCmd::Playlists)];
                    }
                }
                AiMsg::PlayPlaylist(key) => {
                    if let Some(songs) = self.playlists.find(&key).map(|p| p.songs.clone())
                        && !songs.is_empty()
                    {
                        return self.replace_queue_and_load(
                            songs,
                            0,
                            None,
                            QueueReplacementOptions {
                                romanize_all: true,
                                ..QueueReplacementOptions::default()
                            },
                        );
                    }
                }
                AiMsg::StationPatch {
                    down_artists,
                    boost_artists,
                } => {
                    // The off-path feedback summary landed (possibly empty on failure) — always clear
                    // the in-flight guard so the next streak can trigger again. Fold the avoid/boost
                    // into the active station and persist only when the avoid list actually changed.
                    self.streaming.feedback_in_flight = false;
                    if let Some(profile) = self.station.active.as_mut()
                        && profile.apply_feedback(&down_artists, &boost_artists)
                    {
                        self.dirty = true;
                        return vec![Cmd::Persist(PersistCmd::StationProfile)];
                    }
                }
                AiMsg::RomanizedTitles {
                    request_id,
                    keys,
                    entries,
                } => {
                    return self.apply_romanized_titles(request_id, keys, entries);
                }
            },
            Msg::Scrobble(event) => return self.on_scrobble_event(event),
            Msg::UpdateChecked(status) => {
                let mut cmds = Vec::new();
                // One-time status toast + desktop notification the first time a newer
                // release is accepted by the reducer. The persistent surfaces — About
                // notice + brand dot — read `update_status` directly on every frame.
                if status.available && status.first_seen {
                    self.status.kind = StatusKind::Info;
                    self.status.text = if crate::i18n::is_korean() {
                        format!("새 버전 v{} 사용 가능 — About(F1)", status.latest_display())
                    } else {
                        format!(
                            "Update available: v{} — see About (F1)",
                            status.latest_display()
                        )
                    };
                    self.dirty = true;
                    let instructions = crate::update::update_instructions(status.method);
                    let action = instructions.command.unwrap_or(instructions.note);
                    cmds.push(Cmd::DesktopNotify {
                        title: format!("YuTuTui! v{} available", status.latest_display()),
                        body: format!(
                            "Latest: v{} (current: v{}). {action}",
                            status.latest_display(),
                            status.current
                        ),
                    });
                    cmds.push(Cmd::UpdateSeen {
                        tag: status.latest.clone(),
                    });
                }
                self.overlays.update_status = Some(status);
                return cmds;
            }
            Msg::Tools(event) => match event {
                crate::tools::ToolsEvent::Progress { channel, percent } => {
                    self.status.kind = StatusKind::Info;
                    let label = channel.label();
                    let head = t!("Downloading yt-dlp", "yt-dlp 다운로드 중");
                    self.status.text = match percent {
                        Some(p) => format!("{head} ({label})… {p}%"),
                        None => format!("{head} ({label})…"),
                    };
                    self.dirty = true;
                }
                crate::tools::ToolsEvent::Installed { version } => {
                    self.status.kind = StatusKind::Info;
                    self.status.text = if crate::i18n::is_korean() {
                        format!("yt-dlp {version} 준비 완료")
                    } else {
                        format!("yt-dlp {version} ready")
                    };
                    self.dirty = true;
                }
                crate::tools::ToolsEvent::Failed { error } => {
                    // A failed background refresh of a *working* setup stays log-only
                    // (check_and_update already traced it); only an app with no usable
                    // yt-dlp at all needs the user's attention.
                    if crate::tools::ytdlp_selection().is_none() {
                        tracing::warn!(%error, "yt-dlp setup requires attention");
                        self.show_tool_setup(ToolSetupContext::Startup, vec!["yt-dlp"]);
                    }
                }
            },
            Msg::Transfer(event) => return self.on_transfer_event(event),
        }
        Vec::new()
    }
}
