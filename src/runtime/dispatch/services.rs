//! Runtime dispatch for search, assistant, streaming, scrobble, and transfer actors.

use super::super::*;

impl RuntimeHandles {
    pub(super) fn dispatch_search(&mut self, app: &mut App, search_cmd: SearchCmd) {
        match search_cmd {
            SearchCmd::Query {
                request_id,
                query,
                source,
                config,
            } => {
                if let Err(error) = self.api_handle.search(request_id, query, source, config) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Search(SearchMsg::Error {
                            request_id,
                            source,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            SearchCmd::Playlists { request_id, query } => {
                if let Err(error) = self.api_handle.search_playlists(request_id, query) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Search(SearchMsg::Error {
                            request_id,
                            source: crate::search_source::SearchSource::Youtube,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            SearchCmd::Artists { request_id, query } => {
                if let Err(error) = self.api_handle.search_artists(request_id, query) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Search(SearchMsg::Error {
                            request_id,
                            source: crate::search_source::SearchSource::Youtube,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            SearchCmd::PlaylistTracks {
                playlist_id,
                title,
                intent,
            } => {
                if let Err(error) =
                    self.api_handle
                        .playlist_tracks(playlist_id, title.clone(), intent)
                {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Search(SearchMsg::PlaylistTracksError {
                            title,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            SearchCmd::ArtistPage {
                channel_id,
                title,
                intent,
            } => {
                if let Err(error) = self
                    .api_handle
                    .artist_page(channel_id, title.clone(), intent)
                {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Search(SearchMsg::ArtistPageError {
                            title,
                            error: error.to_string(),
                        }),
                    );
                }
            }
        }
    }

    pub(super) fn handle_ai_ask(
        &mut self,
        app: &mut App,
        prompt: String,
        context: Box<crate::ai::AiContext>,
    ) {
        let result = self.ai_handle.as_ref().map_or_else(
            || Err(crate::util::delivery::DeliveryError::Closed),
            |handle| handle.ask(prompt, context),
        );
        if !report_actor_delivery(app, "ai.ask", result) {
            recover_actor_rejection(app, ActorRejectionRecovery::AiTurn);
        }
    }

    pub(super) fn handle_track_resolve(
        &mut self,
        app: &mut App,
        seq: u64,
        query: String,
        config: crate::search_source::SearchConfig,
    ) {
        if let Err(error) = self.api_handle.resolve_track(seq, query, config) {
            tracing::warn!(%error, "api command enqueue failed");
            self.reduce_owner_msg(
                app,
                Msg::TrackResolved {
                    seq,
                    result: Err(error.to_string()),
                },
            );
        }
    }

    pub(super) fn handle_ai_rerank(
        &mut self,
        app: &mut App,
        request_id: u64,
        seed_video_id: String,
        prompt: String,
    ) {
        let recovery_seed = seed_video_id.clone();
        let result = self.ai_handle.as_ref().map_or_else(
            || Err(crate::util::delivery::DeliveryError::Closed),
            |handle| handle.rerank(request_id, seed_video_id, prompt),
        );
        if !report_actor_delivery(app, "ai.rerank", result)
            && let Some(msg) = recover_actor_rejection(
                app,
                ActorRejectionRecovery::AiRerank {
                    request_id,
                    seed_video_id: recovery_seed,
                },
            )
        {
            self.reduce_owner_msg(app, msg);
        }
    }

    pub(super) fn handle_ai_feedback(&mut self, app: &mut App, digest: String) {
        let result = self.ai_handle.as_ref().map_or_else(
            || Err(crate::util::delivery::DeliveryError::Closed),
            |handle| handle.summarize_feedback(digest),
        );
        if !report_actor_delivery(app, "ai.feedback", result) {
            recover_actor_rejection(app, ActorRejectionRecovery::AiFeedback);
        }
    }

    pub(super) fn handle_ai_romanize(
        &mut self,
        app: &mut App,
        request_id: u64,
        items: Vec<crate::romanize::RomanizeItem>,
    ) {
        let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
        if let Some(h) = &self.ai_handle {
            if !report_actor_delivery(app, "ai.romanize", h.romanize(request_id, items)) {
                self.reduce_owner_msg(
                    app,
                    Msg::Ai(AiMsg::RomanizedTitles {
                        request_id,
                        keys,
                        entries: Vec::new(),
                    }),
                );
            }
        } else {
            self.reduce_owner_msg(
                app,
                Msg::Ai(AiMsg::RomanizedTitles {
                    request_id,
                    keys,
                    entries: Vec::new(),
                }),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_streaming_fallback(
        &mut self,
        app: &mut App,
        request_id: u64,
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        mode: crate::streaming::StreamingMode,
        config: crate::search_source::SearchConfig,
    ) {
        if let Err(error) = self.api_handle.streaming(
            request_id,
            seed,
            seed_video_id.clone(),
            exclude_ids,
            crate::playback_policy::STREAMING_POOL_COUNT,
            mode,
            config,
        ) {
            tracing::warn!(%error, "api command enqueue failed");
            self.reduce_owner_msg(
                app,
                Msg::Streaming(StreamingMsg::Error {
                    request_id,
                    seed_video_id,
                    error: error.to_string(),
                }),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_streaming_preflight(
        &mut self,
        app: &mut App,
        request_id: u64,
        seed_video_id: String,
        picks: Vec<crate::api::Song>,
        fallback: Vec<crate::api::Song>,
        mode: crate::streaming::StreamingMode,
        config: crate::streaming::StreamingConfig,
    ) {
        if let Err(error) = self.api_handle.streaming_preflight(
            request_id,
            seed_video_id.clone(),
            picks,
            fallback,
            mode,
            config,
        ) {
            tracing::warn!(%error, "api command enqueue failed");
            self.reduce_owner_msg(
                app,
                Msg::Streaming(StreamingMsg::PreflightError {
                    request_id,
                    seed_video_id,
                    error: error.to_string(),
                }),
            );
        }
    }

    pub(super) fn handle_ai_model(&self, app: &mut App, model: crate::ai::GeminiModel) {
        if let Some(h) = &self.ai_handle {
            report_actor_delivery(app, "ai.model", h.set_model(model));
        }
    }

    pub(super) fn handle_ai_reload(
        &mut self,
        app: &mut App,
        key: Option<String>,
        model: crate::ai::GeminiModel,
        assistant_enabled: bool,
    ) {
        self.ai_handle = key.and_then(|k| {
            crate::ai::spawn(&k, model, sink(self.worker_tx.clone(), RuntimeEvent::Ai))
        });
        app.ai.available = assistant_enabled && self.ai_handle.is_some();
    }

    pub(super) fn dispatch_scrobble(&self, app: &mut App, scrobble: ScrobbleCmd) {
        match scrobble {
            ScrobbleCmd::AuthStart => {
                let result = self.scrobble_handle.as_ref().map_or(
                    Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.auth_start(),
                );
                report_actor_delivery(app, "scrobble.auth", result);
            }
            ScrobbleCmd::Reconfigure(settings) => {
                let result = self.scrobble_handle.as_ref().map_or(
                    Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.reconfigure(*settings),
                );
                report_actor_delivery(app, "scrobble.reconfigure", result);
            }
        }
    }

    pub(super) fn dispatch_transfer(
        &mut self,
        app: &mut App,
        cmd: crate::transfer::actor::TransferCmd,
    ) {
        let recovery = match &cmd {
            crate::transfer::actor::TransferCmd::StartJob(_)
            | crate::transfer::actor::TransferCmd::WriteReviewedLocal { .. } => {
                Some(ActorRejectionRecovery::TransferStart)
            }
            crate::transfer::actor::TransferCmd::CancelJob => {
                Some(ActorRejectionRecovery::TransferCancel)
            }
            crate::transfer::actor::TransferCmd::AuthStart { .. }
            | crate::transfer::actor::TransferCmd::Disconnect
            | crate::transfer::actor::TransferCmd::ListSpotifyPlaylists => None,
        };
        let transfer_tx = self.worker_tx.clone();
        let handle = self.transfer_handle.get_or_insert_with(|| {
            crate::transfer::actor::spawn(move |event| {
                emit(&transfer_tx, RuntimeEvent::Transfer(event))
            })
        });
        if !report_actor_delivery(app, "transfer", handle.send(cmd))
            && let Some(recovery) = recovery
        {
            recover_actor_rejection(app, recovery);
        }
    }
}
