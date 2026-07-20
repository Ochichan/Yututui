//! Remote-command admission and dispatch for the daemon playback owner.

use super::*;

impl DaemonEngine {
    pub async fn handle_remote(
        &mut self,
        command: RemoteCommand,
    ) -> (RemoteResponse, bool, Vec<EngineEffect>) {
        self.handle_remote_scoped(command, None).await
    }

    pub async fn handle_session_remote(
        &mut self,
        command: RemoteCommand,
        requester: RequesterKey,
    ) -> (RemoteResponse, bool, Vec<EngineEffect>) {
        self.handle_remote_scoped(command, Some(requester)).await
    }

    async fn handle_remote_scoped(
        &mut self,
        command: RemoteCommand,
        requester: Option<RequesterKey>,
    ) -> (RemoteResponse, bool, Vec<EngineEffect>) {
        if command
            .expected_queue_rev()
            .is_some_and(|revision| revision != self.queue.rev())
        {
            return (RemoteResponse::err("stale_rev"), false, Vec::new());
        }
        if let Some(response) = self.preflight_remote_persistence(&command) {
            return (response, false, Vec::new());
        }
        let mut effects = Vec::new();
        let shutdown = matches!(command, RemoteCommand::Quit);
        let response = match command {
            RemoteCommand::ExportPersonalData { .. } => {
                unreachable!("personal export is intercepted by the daemon owner loop")
            }
            RemoteCommand::Status => RemoteResponse::status(self.status()),
            RemoteCommand::Quit => {
                self.stop_playback();
                // `stop_playback` rearms normal transport recovery for future loads. Process
                // teardown is terminal, so close that gate again before the stopped actor can
                // enqueue its final TransportClosed event.
                self.suppress_transport_recovery_for_shutdown();
                self.save_session();
                RemoteResponse::ok("stopping daemon".to_string())
            }
            RemoteCommand::Next => {
                let outgoing = self.prepare_outgoing(false);
                let response = self.next_track().await;
                if (response.ok || response.reason.as_deref() == Some("queue_end"))
                    && let Some(outgoing) = outgoing
                {
                    self.commit_outgoing(outgoing);
                }
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Prev => self.prev_track().await,
            RemoteCommand::TogglePause => {
                let response = self.toggle_pause().await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Play { query } => {
                let response = self.search_and_play(query).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Enqueue { query } => {
                let response = self.search_and_enqueue(query).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::VolumeUp => self.adjust_volume(VOLUME_STEP),
            RemoteCommand::VolumeDown => self.adjust_volume(-VOLUME_STEP),
            RemoteCommand::SetVolume { percent } => self.set_volume(percent),
            RemoteCommand::SeekBack => self.seek(-self.config.effective_seek_seconds()),
            RemoteCommand::SeekForward => self.seek(self.config.effective_seek_seconds()),
            RemoteCommand::SeekTo { ms } => self.seek_to(ms as f64 / 1000.0),
            RemoteCommand::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.config.shuffle = Some(self.queue.shuffle);
                self.save_config("daemon shuffle setting");
                self.save_session();
                RemoteResponse::status(self.status())
            }
            RemoteCommand::CycleRepeat => {
                let transition = PlaybackModeState::new(self.queue.repeat, self.streaming)
                    .transition(PlaybackModeAction::CycleRepeat);
                match transition {
                    Ok(transition) => {
                        self.queue.repeat = transition.state.repeat;
                        self.config.repeat = self.queue.repeat;
                        self.save_config("daemon repeat setting");
                        self.save_session();
                        RemoteResponse::status(self.status())
                    }
                    Err(_) => RemoteResponse::err("incompatible_playback_modes"),
                }
            }
            RemoteCommand::QueuePlay { position }
            | RemoteCommand::QueuePlayIfRevision { position, .. } => {
                let response = self.queue_play(position).await;
                if response.ok {
                    effects.extend(self.maybe_autoplay_extend());
                }
                response
            }
            RemoteCommand::QueueRemove { position }
            | RemoteCommand::QueueRemoveIfRevision { position, .. } => {
                let response = self.queue_remove(position).await;
                if response.ok {
                    effects.extend(self.maybe_autoplay_extend());
                }
                response
            }
            RemoteCommand::Streaming { state } => {
                let (response, streaming_effects) = self.set_streaming(state);
                effects.extend(streaming_effects);
                response
            }
            RemoteCommand::SetSetting { change } => {
                let (response, setting_effects) = self.set_setting(change);
                effects.extend(setting_effects);
                response
            }
            RemoteCommand::ResumeSession => {
                let response = self.resume_session().await;
                if response.ok {
                    effects.extend(self.force_autoplay_extend());
                }
                response
            }
            RemoteCommand::RunSearch {
                ticket,
                query,
                source,
            } => {
                let query = query.trim().to_string();
                if query.is_empty() {
                    RemoteResponse::err("empty_query")
                } else if query.len() > REMOTE_MAX_QUERY_BYTES {
                    RemoteResponse::err("query_too_long")
                } else if let Some(requester) = requester.clone() {
                    match self
                        .gui_search_index
                        .begin(&requester, ticket, &query, source)
                    {
                        GuiSearchAdmission::Start => {
                            effects.push(EngineEffect::GuiSearch {
                                requester,
                                ticket,
                                query,
                                source,
                                config: self.config.effective_search(),
                            });
                            RemoteResponse::ok("searching".to_string())
                        }
                        GuiSearchAdmission::DuplicateActive => {
                            RemoteResponse::ok("searching".to_string())
                        }
                        GuiSearchAdmission::StaleTicket => RemoteResponse::err("stale_ticket"),
                        GuiSearchAdmission::TicketConflict => {
                            RemoteResponse::err("ticket_conflict")
                        }
                    }
                } else {
                    RemoteResponse::err("session_required")
                }
            }
            RemoteCommand::PlayTracks { video_ids } => {
                let response = self.play_tracks(requester.as_ref(), video_ids).await;
                if response.ok {
                    effects.extend(self.maybe_autoplay_extend());
                }
                response
            }
            RemoteCommand::EnqueueTracks { video_ids } => {
                let response = self.enqueue_tracks(requester.as_ref(), video_ids).await;
                if response.ok {
                    effects.extend(self.maybe_autoplay_extend());
                }
                response
            }
            RemoteCommand::Apply { change } => {
                let (response, setting_effects) = self.apply_gui_setting(change);
                effects.extend(setting_effects);
                response
            }
            RemoteCommand::SetGeminiKey { key } => {
                let key = key.trim();
                if key.len() > REMOTE_MAX_GEMINI_KEY_BYTES {
                    RemoteResponse::err("key_too_long")
                } else {
                    self.config.gemini_api_key = (!key.is_empty()).then(|| key.to_string());
                    self.save_config("daemon gemini key");
                    RemoteResponse::ok("gemini key updated".to_string())
                }
            }
            RemoteCommand::ResetAllSettings => {
                // Danger zone (GUI double-confirms). Keep playback rolling; the fresh
                // defaults apply live where cheap and at next launch elsewhere.
                let defaults = Config::default();
                if let Err(error) = self.send_player_command_if_active(
                    "reset_long_form_seek_optimization",
                    PlayerCmd::SetLongFormSeekOptimization(
                        defaults.audio.mpv.long_form_seek_optimization,
                    ),
                ) {
                    return (self.reject_player_command(error), false, effects);
                }
                self.cancel_pending_streaming_request();
                self.config = defaults;
                self.save_config("daemon settings reset");
                RemoteResponse::ok("settings reset".to_string())
            }
            // Queue order surgery never touches the current track or playback position
            // (no position_epoch interaction); the shared Queue methods keep both
            // owners byte-identical for the parity harness.
            RemoteCommand::QueueMove { from, to, .. } => {
                if self.queue.move_item(from, to).is_none() {
                    RemoteResponse::err("queue_index")
                } else {
                    self.save_session();
                    RemoteResponse::status(self.status())
                }
            }
            RemoteCommand::QueueClearUpcoming { .. } => {
                if self.queue.clear_upcoming() > 0 {
                    self.save_session();
                }
                RemoteResponse::status(self.status())
            }
            RemoteCommand::PlayVideo { video_id } => self.play_video(requester.as_ref(), video_id),
            RemoteCommand::LibraryPlay { scope, filter } => {
                self.gui_library_play(&scope, &filter).await
            }
            RemoteCommand::LibraryEnqueue { scope, filter } => {
                self.gui_library_enqueue(&scope, &filter).await
            }
            RemoteCommand::LibraryRemove { scope, video_id } => {
                self.gui_library_remove(&scope, &video_id)
            }
            RemoteCommand::FetchLibraryPage {
                scope,
                filter,
                offset,
                limit,
            } => self.gui_fetch_library_page(&scope, &filter, offset, limit),
            RemoteCommand::PlaylistCreate { name } => self.gui_playlist_create(&name),
            RemoteCommand::PlaylistDelete { playlist_id } => self.gui_playlist_delete(&playlist_id),
            RemoteCommand::PlaylistAddTracks {
                playlist_id,
                video_ids,
            } => self.gui_playlist_add_tracks(requester.as_ref(), &playlist_id, &video_ids),
            RemoteCommand::PlaylistRemoveTrack {
                playlist_id,
                video_id,
            } => self.gui_playlist_remove_track(&playlist_id, &video_id),
            RemoteCommand::PlaylistPlay { playlist_id } => {
                self.gui_playlist_play(&playlist_id).await
            }
            RemoteCommand::FetchPlaylistDetail { playlist_id } => {
                self.gui_fetch_playlist_detail(&playlist_id)
            }
            RemoteCommand::Rate { video_id, rating } => self.gui_rate(&video_id, rating),
            RemoteCommand::FetchWhyGem { video_id } => self.gui_fetch_why_gem(&video_id),
            // Deferred v8 GUI surface (gui/WIRING.md §1.5): variants exist so the
            // gateway stops answering bad_command; each stream replaces its arms with
            // real dispatch. Until then the reason is an honest not_supported (the
            // frontend gates these paths behind the v8-commands capability anyway).
            // Defensive backstop: the daemon owner loop owns the download actor and
            // intercepts these before engine dispatch.
            RemoteCommand::Download { .. } | RemoteCommand::DeleteDownload { .. } => {
                RemoteResponse::err("not_supported")
            }
            RemoteCommand::KeymapBind {
                context,
                action,
                chord,
            } => self.gui_keymap_bind(&context, &action, &chord),
            RemoteCommand::KeymapUnbind { context, action } => {
                self.gui_keymap_unbind(&context, &action)
            }
            RemoteCommand::KeymapResetAll => self.gui_keymap_reset_all(),
            RemoteCommand::ThemeSetOverride { role, hex } => {
                let (response, theme_effects) = self.gui_theme_set_override(role, hex);
                effects.extend(theme_effects);
                response
            }
            RemoteCommand::ThemeClearOverride { role } => self.gui_theme_clear_override(&role),
            RemoteCommand::ClearRomanizationCache => self.gui_clear_romanization_cache(),
            // queue_remove_many has no frontend sender yet; ask_ai and lastfm_connect
            // are intercepted by the owner-loop hosts before engine dispatch (backstops).
            RemoteCommand::QueueRemoveMany { .. }
            | RemoteCommand::AskAi { .. }
            | RemoteCommand::LastfmConnect => RemoteResponse::err("not_supported"),
            // Defensive backstop: the daemon owner loop owns the transfer actor and
            // intercepts these before engine dispatch.
            RemoteCommand::TransferListSpotify
            | RemoteCommand::TransferStart { .. }
            | RemoteCommand::TransferCancel
            | RemoteCommand::SpotifyConnect => RemoteResponse::err("not_supported"),
            RemoteCommand::ListenBrainzConfigure {
                submit,
                token,
                custom_url,
            } => self.gui_listen_brainz_configure(submit, token, custom_url),
            RemoteCommand::AccountSet {
                service,
                field,
                value,
            } => self.gui_account_set(&service, &field, &value),
        };
        self.finish_remote_persistence(response, shutdown, effects)
    }
}
