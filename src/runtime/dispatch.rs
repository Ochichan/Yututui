//! Dispatch reducer commands to runtime-owned actors and background jobs.

use super::*;

mod data;
mod download;
mod linked_playlists;
mod local;
mod music_server;
mod server_library;
mod services;

impl RuntimeHandles {
    pub fn dispatch(&mut self, app: &mut App, cmd: Cmd) {
        self.background_tasks.reap_finished();
        if let Some(component) = read_only::durable_mutation_component(&cmd) {
            let reason = durable_mutation_rejection_reason(self.persistence_read_only.as_ref());
            if let Some(reason) = reason {
                for follow_up in read_only::reject_mutation(app, &cmd, component, &reason) {
                    self.dispatch(app, follow_up);
                }
                return;
            }
        }
        match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => {
                self.handle_player_transport_closed(app, restore);
            }
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => {
                self.dispatch_player_intent(app, intent);
            }
            // dispatch runs synchronously right after each update, so the connect for a
            // spawn generation is always installed before any VideoLoad that follows it.
            Cmd::VideoConnect {
                ipc_path,
                generation,
                bindings,
            } => {
                let tx = self.worker_tx.clone();
                self.video_handle = Some(crate::player::video::connect(
                    ipc_path,
                    generation,
                    bindings,
                    move |generation, event| {
                        emit_callback_observed(&tx, RuntimeEvent::Video { generation, event });
                    },
                ));
            }
            Cmd::VideoLoad(url) => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::Load(url), "video_load");
                if result.is_err() {
                    // Drop the rejected generation before closing its process so no stale
                    // pending load can later reach an overlay which no longer represents state.
                    self.video_handle = None;
                }
                for follow_up in settle_video_load_delivery(app, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::VideoTogglePause => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::CyclePause, "video_pause");
                report_player_delivery(app, "video_pause", result);
            }
            Cmd::VideoToggleFullscreen => {
                let result = self.send_video_cmd(
                    crate::player::video::VideoCmd::CycleFullscreen,
                    "video_fullscreen",
                );
                report_player_delivery(app, "video_fullscreen", result);
            }
            Cmd::VideoToggleMute => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::CycleMute, "video_mute");
                report_player_delivery(app, "video_mute", result);
            }
            Cmd::UpdateSeen { tag } => crate::update::mark_notified(&tag),
            Cmd::Search(search_cmd) => self.dispatch_search(app, search_cmd),
            Cmd::MusicServer(command) => self.dispatch_music_server(command),
            Cmd::ServerLibrary(command) => self.dispatch_server_library(command),
            // Persist: hand the persistence actor an owned snapshot (or clear one). Cloning a
            // store is a couple ms of memcpy at worst; the fsync it replaces on this task was
            // 5-50ms. The marker variants clone the live snapshot from `app` here; `Config`
            // carries its own owned snapshot.
            Cmd::Persist(PersistCmd::TransferPlaylistCommit(commit)) => {
                self.dispatch_transfer_playlist_commit(app, commit);
            }
            Cmd::Persist(PersistCmd::PersonalSyncCommit(commit)) => {
                self.dispatch_personal_sync_commit(app, commit);
            }
            Cmd::Persist(PersistCmd::SyncActivationCommit(commit)) => {
                self.dispatch_sync_activation_commit(app, commit);
            }
            Cmd::Persist(p) => {
                let result = persist_delivery::admit(&self.persist, app, p);
                report_actor_delivery(app, "persistence", result);
            }
            Cmd::Data(cmd) => self.dispatch_data(app, cmd),
            Cmd::Download(cmd) => self.dispatch_download(app, cmd),
            Cmd::Local(cmd) => self.dispatch_local(cmd),
            Cmd::Recorder(job) => {
                self.dispatch_recorder(app, job);
            }
            Cmd::FetchLyrics(request) => {
                if !report_actor_delivery(app, "lyrics", self.lyrics_handle.fetch(request)) {
                    recover_actor_rejection(app, ActorRejectionRecovery::Lyrics);
                }
            }
            Cmd::FetchArtwork { video_id, source } => {
                if !report_actor_delivery(
                    app,
                    "artwork",
                    self.artwork_handle.fetch(video_id, source),
                ) {
                    recover_actor_rejection(app, ActorRejectionRecovery::Artwork);
                }
            }
            Cmd::Resolve {
                video_id,
                watch_url,
            } => {
                let result = self.resolver_handle.resolve(video_id.clone(), watch_url);
                for follow_up in settle_resolver_admission(app, video_id, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::ResolveForSelfHeal {
                video_id,
                watch_url,
            } => {
                let result = self
                    .resolver_handle
                    .resolve_for_self_heal(video_id.clone(), watch_url);
                for follow_up in settle_resolver_admission(app, video_id, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::YtdlpSelfHeal { video_id, tools } => {
                // Off-loop: an update check downloads up to ~40 MiB. Progress rides the
                // same Tools status-line events as the maintainer; the verdict returns
                // as Msg::YtdlpHealResult for the reducer's retry-or-skip decision.
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_cancellable("ytdlp_self_heal", async move {
                        let progress_emitter = emitter.clone();
                        crate::tools::ytdlp::clear_probe_cache();
                        let outcome = crate::tools::ytdlp::rollback_or_check_and_update(
                            &tools,
                            &move |event| {
                                progress_emitter.emit(RuntimeEvent::Tools(event));
                            },
                            "playback self-heal",
                        )
                        .await;
                        let updated = matches!(
                            outcome,
                            crate::tools::ytdlp::UpdateOutcome::Installed { .. }
                        );
                        emitter
                            .emit_terminal(RuntimeEvent::App(Msg::YtdlpHealResult {
                                video_id,
                                updated,
                            }))
                            .await;
                    });
            }
            Cmd::AskAi { prompt, context } => self.handle_ai_ask(app, prompt, context),
            Cmd::ResolveTrack { seq, query, config } => {
                self.handle_track_resolve(app, seq, query, config);
            }
            Cmd::AiRerank {
                request_id,
                seed_video_id,
                prompt,
            } => self.handle_ai_rerank(app, request_id, seed_video_id, prompt),
            Cmd::SummarizeFeedback { digest } => self.handle_ai_feedback(app, digest),
            Cmd::RomanizeTitles { request_id, items } => {
                self.handle_ai_romanize(app, request_id, items);
            }
            Cmd::StreamingFallback {
                request_id,
                seed,
                seed_video_id,
                exclude_ids,
                mode,
                config,
            } => self.handle_streaming_fallback(
                app,
                request_id,
                seed,
                seed_video_id,
                exclude_ids,
                mode,
                config,
            ),
            Cmd::StreamingPreflight {
                request_id,
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => self.handle_streaming_preflight(
                app,
                request_id,
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            ),
            Cmd::SetAiModel(model) => self.handle_ai_model(app, model),
            Cmd::ReloadAi {
                key,
                model,
                assistant_enabled,
            } => self.handle_ai_reload(app, key, model, assistant_enabled),
            Cmd::Scrobble(scrobble) => self.dispatch_scrobble(app, scrobble),
            Cmd::Transfer(cmd) => self.dispatch_transfer(app, cmd),
            // Handled in the main loop (the OSC path writes to the terminal this scope doesn't
            // own); never reaches here. Listed for exhaustiveness.
            Cmd::DesktopNotify { .. } => {}
        }
    }
}

async fn refresh_music_server_runtime(
    read_only: bool,
    bridge_sink: Option<crate::open_subsonic::OpenSubsonicBridgeSink>,
) -> (
    Result<Option<crate::open_subsonic::OpenSubsonicRuntime>, crate::open_subsonic::ServiceError>,
    Result<crate::app::MusicServerRefreshOutcome, crate::app::MusicServerFailure>,
) {
    let paths = match crate::open_subsonic::OpenSubsonicPaths::current() {
        Ok(paths) => paths,
        Err(error) => {
            let error = crate::open_subsonic::ServiceError::from(error);
            return (Err(error), Err(music_server_failure(error)));
        }
    };
    let local_status = match crate::open_subsonic::read_status(&paths) {
        Ok(status) => status,
        Err(error) => return (Err(error), Err(music_server_failure(error))),
    };
    let mut local_summary = music_server_summary(local_status);
    let runtime_result = if read_only {
        crate::open_subsonic::load_actor_read_only(&paths).await
    } else {
        crate::open_subsonic::load_actor_with_bridge_sink(&paths, bridge_sink).await
    };
    let outcome = match &runtime_result {
        Ok(Some(_)) => {
            let mut summary = crate::open_subsonic::read_status(&paths)
                .map(music_server_summary)
                .unwrap_or(local_summary);
            summary.health = live_music_server_health(
                summary.playback_reports_needing_decision,
                summary.playlist_creates_needing_decision,
                summary.playlist_links_needing_decision,
                summary.playlist_projections_needing_decision,
                summary.playlist_contents_needing_decision,
            );
            summary.configured = true;
            crate::app::MusicServerRefreshOutcome {
                summary,
                failure: None,
            }
        }
        Ok(None) => crate::app::MusicServerRefreshOutcome {
            summary: crate::app::MusicServerSummary::default(),
            failure: None,
        },
        Err(error) => {
            local_summary.health = crate::app::MusicServerHealth::NeedsAttention;
            crate::app::MusicServerRefreshOutcome {
                summary: local_summary,
                failure: Some(music_server_failure(*error)),
            }
        }
    };
    (runtime_result, Ok(outcome))
}

async fn reload_music_server_runtime(
    bridge_sink: Option<crate::open_subsonic::OpenSubsonicBridgeSink>,
) -> Result<Option<crate::open_subsonic::OpenSubsonicRuntime>, crate::open_subsonic::ServiceError> {
    let paths = crate::open_subsonic::OpenSubsonicPaths::current()?;
    crate::open_subsonic::load_actor_with_bridge_sink(&paths, bridge_sink).await
}

pub(super) fn resolve_music_server_remove(
    removal_error: Option<crate::open_subsonic::ServiceError>,
    reload: Result<bool, crate::open_subsonic::ServiceError>,
) -> Result<(), crate::app::MusicServerFailure> {
    match reload {
        // The coherent store is absent. This is also the proof that an error returned after the
        // removal commit marker was an ambiguous success.
        Ok(false) => Ok(()),
        Ok(true) => Err(removal_error
            .map(music_server_failure)
            .unwrap_or(crate::app::MusicServerFailure::Unavailable)),
        Err(error) => Err(music_server_failure(error)),
    }
}

async fn prepare_music_server_setup(
    input: crate::app::MusicServerSetupInput,
) -> Result<crate::open_subsonic::PreparedSetup, crate::open_subsonic::ServiceError> {
    const MAX_CUSTOM_CA_BYTES: u64 = 192 * 1024;

    let crate::app::MusicServerSetupInput {
        mut display_name,
        mut origin,
        mut username,
        mut secret,
        credential_mode,
        mut custom_ca_path,
        allow_lan_http,
        identity_intent,
    } = input;
    let custom_ca_pem = if custom_ca_path.trim().is_empty() {
        None
    } else {
        let path = std::path::PathBuf::from(custom_ca_path.as_str());
        custom_ca_path.clear();
        Some(
            tokio::task::spawn_blocking(move || {
                let bytes =
                    crate::util::safe_fs::read_no_symlink_limited(&path, MAX_CUSTOM_CA_BYTES)
                        .map_err(|_| crate::open_subsonic::ServiceError::InvalidSetup)?;
                if bytes.is_empty() {
                    return Err(crate::open_subsonic::ServiceError::InvalidSetup);
                }
                Ok(bytes)
            })
            .await
            .map_err(|_| crate::open_subsonic::ServiceError::ActorUnavailable)??,
        )
    };
    let secret_value = std::mem::take(&mut *secret);
    let credential = match credential_mode {
        crate::app::MusicServerCredentialMode::Password => {
            let username_value = std::mem::take(&mut *username);
            crate::open_subsonic::ServerCredential::password(
                username_value,
                age::secrecy::SecretString::from(secret_value),
            )
            .map_err(|_| crate::open_subsonic::ServiceError::InvalidSetup)?
        }
        crate::app::MusicServerCredentialMode::ApiKey => {
            crate::open_subsonic::ServerCredential::api_key(age::secrecy::SecretString::from(
                secret_value,
            ))
            .map_err(|_| crate::open_subsonic::ServiceError::InvalidSetup)?
        }
    };
    let paths = crate::open_subsonic::OpenSubsonicPaths::current()?;
    crate::open_subsonic::test_and_prepare_setup(
        &paths,
        crate::open_subsonic::SetupInput::new(
            std::mem::take(&mut *display_name),
            std::mem::take(&mut *origin),
            allow_lan_http,
            custom_ca_pem,
            credential,
            match identity_intent {
                crate::app::MusicServerIdentityIntent::Create => {
                    crate::open_subsonic::SetupIdentityIntent::Create
                }
                crate::app::MusicServerIdentityIntent::UpdateSameServerAndAccount => {
                    crate::open_subsonic::SetupIdentityIntent::UpdateSameServerAndAccount
                }
                crate::app::MusicServerIdentityIntent::ReplaceServerOrAccount => {
                    crate::open_subsonic::SetupIdentityIntent::ReplaceServerOrAccount
                }
            },
        ),
    )
    .await
}

fn music_server_summary(
    status: crate::open_subsonic::OpenSubsonicStatus,
) -> crate::app::MusicServerSummary {
    let configured = status.kind != crate::open_subsonic::OpenSubsonicStatusKind::Off;
    let credential_kind = status.credential_kind.map(|kind| match kind {
        crate::open_subsonic::CredentialKind::Password => {
            crate::app::MusicServerCredentialMode::Password
        }
        crate::open_subsonic::CredentialKind::ApiKey => {
            crate::app::MusicServerCredentialMode::ApiKey
        }
    });
    crate::app::MusicServerSummary {
        health: match status.kind {
            crate::open_subsonic::OpenSubsonicStatusKind::Off => crate::app::MusicServerHealth::Off,
            crate::open_subsonic::OpenSubsonicStatusKind::UpToDate => {
                crate::app::MusicServerHealth::UpToDate
            }
            crate::open_subsonic::OpenSubsonicStatusKind::NeedsAttention => {
                crate::app::MusicServerHealth::NeedsAttention
            }
        },
        configured,
        display_name: status.display_name,
        credential_kind,
        lan_http: status.uses_lan_http,
        custom_ca: status.uses_custom_ca,
        playback_reports_needing_decision: status.outbound_scrobbles_needing_attention,
        playlist_creates_needing_decision: status.playlist_creates_needing_attention,
        playlist_create_attention: status.playlist_create_attention,
        playlist_links_needing_decision: status.playlist_links_needing_decision,
        playlist_projections_needing_decision: status.playlist_projections_needing_attention,
        playlist_contents_needing_decision: status.playlist_contents_needing_attention,
        history: match status.native_history_health {
            crate::open_subsonic::NativeHistoryHealth::Off => {
                crate::app::MusicServerHistoryHealth::Off
            }
            crate::open_subsonic::NativeHistoryHealth::Probing => {
                crate::app::MusicServerHistoryHealth::Probing
            }
            crate::open_subsonic::NativeHistoryHealth::Detailed => {
                crate::app::MusicServerHistoryHealth::Detailed
            }
            crate::open_subsonic::NativeHistoryHealth::PlayCountsOnly => {
                crate::app::MusicServerHistoryHealth::PlayCountsOnly
            }
            crate::open_subsonic::NativeHistoryHealth::UpdatePassword => {
                crate::app::MusicServerHistoryHealth::UpdatePassword
            }
        },
    }
}

pub(super) const fn live_music_server_health(
    playback_reports_needing_decision: usize,
    playlist_creates_needing_decision: usize,
    playlist_links_needing_decision: usize,
    playlist_projections_needing_decision: usize,
    playlist_contents_needing_decision: usize,
) -> crate::app::MusicServerHealth {
    if playback_reports_needing_decision == 0
        && playlist_creates_needing_decision == 0
        && playlist_links_needing_decision == 0
        && playlist_projections_needing_decision == 0
        && playlist_contents_needing_decision == 0
    {
        crate::app::MusicServerHealth::UpToDate
    } else {
        crate::app::MusicServerHealth::NeedsAttention
    }
}

fn music_server_failure(
    error: crate::open_subsonic::ServiceError,
) -> crate::app::MusicServerFailure {
    match error {
        crate::open_subsonic::ServiceError::Store(_) => crate::app::MusicServerFailure::Storage,
        crate::open_subsonic::ServiceError::Server(error) => match error {
            crate::open_subsonic::ServerError::AuthenticationRequired
            | crate::open_subsonic::ServerError::PermissionDenied => {
                crate::app::MusicServerFailure::Authentication
            }
            crate::open_subsonic::ServerError::CertificateFailed => {
                crate::app::MusicServerFailure::Certificate
            }
            crate::open_subsonic::ServerError::OriginRejected
            | crate::open_subsonic::ServerError::WrongAccountScope => {
                crate::app::MusicServerFailure::InvalidInput
            }
            crate::open_subsonic::ServerError::Offline
            | crate::open_subsonic::ServerError::RateLimited(_)
            | crate::open_subsonic::ServerError::TemporarilyUnavailable => {
                crate::app::MusicServerFailure::Connection
            }
            crate::open_subsonic::ServerError::UnsupportedFeature
            | crate::open_subsonic::ServerError::NotFound
            | crate::open_subsonic::ServerError::InvalidResponse
            | crate::open_subsonic::ServerError::ResponseTooLarge => {
                crate::app::MusicServerFailure::InvalidInput
            }
        },
        crate::open_subsonic::ServiceError::InvalidSetup => {
            crate::app::MusicServerFailure::InvalidInput
        }
        crate::open_subsonic::ServiceError::ActorUnavailable
        | crate::open_subsonic::ServiceError::ProxyUnavailable => {
            crate::app::MusicServerFailure::Unavailable
        }
    }
}

fn server_library_failure(
    error: crate::open_subsonic::ServerError,
) -> crate::app::ServerLibraryFailure {
    match error {
        crate::open_subsonic::ServerError::AuthenticationRequired
        | crate::open_subsonic::ServerError::PermissionDenied => {
            crate::app::ServerLibraryFailure::Authentication
        }
        crate::open_subsonic::ServerError::UnsupportedFeature
        | crate::open_subsonic::ServerError::NotFound => {
            crate::app::ServerLibraryFailure::Unsupported
        }
        crate::open_subsonic::ServerError::InvalidResponse
        | crate::open_subsonic::ServerError::ResponseTooLarge
        | crate::open_subsonic::ServerError::WrongAccountScope => {
            crate::app::ServerLibraryFailure::InvalidResponse
        }
        crate::open_subsonic::ServerError::Offline
        | crate::open_subsonic::ServerError::CertificateFailed
        | crate::open_subsonic::ServerError::OriginRejected
        | crate::open_subsonic::ServerError::RateLimited(_)
        | crate::open_subsonic::ServerError::TemporarilyUnavailable => {
            crate::app::ServerLibraryFailure::Offline
        }
    }
}
