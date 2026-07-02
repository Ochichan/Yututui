//! Runtime event adapter between leaf actors and the app reducer.
//!
//! Actors emit domain-specific events so they do not depend on `crate::app::Msg`.
//! This module is the single orchestration boundary that maps those events back into
//! reducer messages.

use ratatui_image::thread::ResizeResponse;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{App, Cmd, Msg};
use crate::config::PlayerRuntimeConfig;
use crate::player::{PlayerCmd, PlayerHandle};

pub enum RuntimeEvent {
    App(Msg),
    Ai(crate::ai::AiEvent),
    Api(crate::api::ApiEvent),
    Artwork(crate::artwork::ArtworkEvent),
    ArtworkResized(ResizeResponse),
    Download(crate::download::DownloadEvent),
    Lyrics(crate::lyrics::LyricsEvent),
    Player(crate::player::PlayerEvent),
    Remote(crate::remote::server::RemoteEvent),
    Resolver(crate::resolver::ResolverEvent),
    Scrobble(crate::scrobble::ScrobbleEvent),
    Signal(crate::player::lifetime::SignalEvent),
    Transfer(crate::transfer::actor::TransferEvent),
}

impl From<RuntimeEvent> for Msg {
    fn from(event: RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::App(msg) => msg,
            RuntimeEvent::Ai(event) => match event {
                crate::ai::AiEvent::Thinking(on) => Msg::AiThinking(on),
                crate::ai::AiEvent::Chat(text) => Msg::AiChat(text),
                crate::ai::AiEvent::Error(text) => Msg::AiError(text),
                crate::ai::AiEvent::PlayTracks(songs) => Msg::AiPlayTracks(songs),
                crate::ai::AiEvent::Enqueue(songs) => Msg::AiEnqueue(songs),
                crate::ai::AiEvent::Suggestions(songs) => Msg::AiSuggestions(songs),
                crate::ai::AiEvent::SetAutoplay(on) => Msg::AiSetAutoplay(on),
                crate::ai::AiEvent::SetStationProfile {
                    query,
                    explore,
                    avoid_artists,
                } => Msg::AiSetStationProfile {
                    query,
                    explore,
                    avoid_artists,
                },
                crate::ai::AiEvent::CreatePlaylist(name) => Msg::AiCreatePlaylist(name),
                crate::ai::AiEvent::AddToPlaylist { playlist, songs } => {
                    Msg::AiAddToPlaylist { playlist, songs }
                }
                crate::ai::AiEvent::PlayPlaylist(key) => Msg::AiPlayPlaylist(key),
                crate::ai::AiEvent::StreamingPicks {
                    seed_video_id,
                    picks,
                    conf,
                } => Msg::StreamingAiPicks {
                    seed_video_id,
                    picks,
                    conf,
                },
                crate::ai::AiEvent::StationPatch {
                    down_artists,
                    boost_artists,
                } => Msg::StationPatch {
                    down_artists,
                    boost_artists,
                },
                crate::ai::AiEvent::RomanizedTitles {
                    request_id,
                    keys,
                    entries,
                } => Msg::RomanizedTitles {
                    request_id,
                    keys,
                    entries,
                },
            },
            RuntimeEvent::Api(event) => match event {
                crate::api::ApiEvent::ModeResolved { mode, had_cookie } => {
                    Msg::ApiModeResolved { mode, had_cookie }
                }
                crate::api::ApiEvent::SearchResults {
                    query,
                    source,
                    songs,
                } => Msg::SearchResults {
                    query,
                    source,
                    songs,
                },
                crate::api::ApiEvent::SearchError { source, error } => {
                    Msg::SearchError { source, error }
                }
                crate::api::ApiEvent::StreamingResults {
                    seed_video_id,
                    candidates,
                } => Msg::StreamingResults {
                    seed_video_id,
                    candidates,
                },
                crate::api::ApiEvent::StreamingPreflighted {
                    seed_video_id,
                    songs,
                } => Msg::StreamingPreflighted {
                    seed_video_id,
                    songs,
                },
                crate::api::ApiEvent::StreamingError {
                    seed_video_id,
                    error,
                } => Msg::StreamingError {
                    seed_video_id,
                    error,
                },
            },
            RuntimeEvent::Artwork(crate::artwork::ArtworkEvent::Result { video_id, image }) => {
                Msg::ArtworkResult { video_id, image }
            }
            RuntimeEvent::ArtworkResized(response) => Msg::ArtworkResized(response),
            RuntimeEvent::Download(event) => match event {
                crate::download::DownloadEvent::Progress { video_id, percent } => {
                    Msg::DownloadProgress { video_id, percent }
                }
                crate::download::DownloadEvent::Done { video_id, path } => {
                    Msg::DownloadDone { video_id, path }
                }
                crate::download::DownloadEvent::Error { video_id, error } => {
                    Msg::DownloadError { video_id, error }
                }
            },
            RuntimeEvent::Lyrics(crate::lyrics::LyricsEvent::Result { video_id, lines }) => {
                Msg::LyricsResult { video_id, lines }
            }
            RuntimeEvent::Player(event) => match event {
                crate::player::PlayerEvent::TimePos(t) => Msg::PlayerTimePos(t),
                crate::player::PlayerEvent::Duration(d) => Msg::PlayerDuration(d),
                crate::player::PlayerEvent::Paused(paused) => Msg::PlayerPaused(paused),
                crate::player::PlayerEvent::Volume(volume) => Msg::PlayerVolume(volume),
                crate::player::PlayerEvent::Metadata(metadata) => Msg::PlayerMetadata(metadata),
                crate::player::PlayerEvent::Eof => Msg::PlayerEof,
                crate::player::PlayerEvent::Error(error) => Msg::PlayerError(error),
            },
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(cmd, reply)) => {
                Msg::Remote(cmd, reply)
            }
            RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Resolved {
                video_id,
                stream_url,
            }) => Msg::Resolved {
                video_id: video_id.into_string(),
                stream_url: stream_url.into_string(),
            },
            RuntimeEvent::Scrobble(event) => Msg::Scrobble(event),
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit) => Msg::Quit,
            RuntimeEvent::Transfer(event) => Msg::Transfer(event),
        }
    }
}

pub fn sink<T, F>(tx: UnboundedSender<RuntimeEvent>, wrap: F) -> impl Fn(T) + Send + Sync + 'static
where
    T: 'static,
    F: Fn(T) -> RuntimeEvent + Send + Sync + 'static,
{
    move |event| {
        let _ = tx.send(wrap(event));
    }
}

pub fn remote_sink(
    tx: UnboundedSender<RuntimeEvent>,
) -> impl Fn(crate::remote::server::RemoteEvent) -> bool + Send + Sync + 'static {
    move |event| tx.send(RuntimeEvent::Remote(event)).is_ok()
}

pub struct RuntimeHandles {
    worker_tx: UnboundedSender<RuntimeEvent>,
    player_handle: Option<PlayerHandle>,
    pending_player_cmds: Vec<PlayerCmd>,
    player_failed: bool,
    _mpv_guard: Option<crate::player::Mpv>,
    api_handle: crate::api::ApiHandle,
    lyrics_handle: crate::lyrics::LyricsHandle,
    artwork_handle: crate::artwork::ArtworkHandle,
    download_handle: crate::download::DownloadHandle,
    resolver_handle: crate::resolver::ResolverHandle,
    ai_handle: Option<crate::ai::AiHandle>,
    scrobble_handle: crate::scrobble::ScrobbleHandle,
    /// Spawned on the first transfer command — costs nothing until the feature is used.
    transfer_handle: Option<crate::transfer::actor::TransferHandle>,
}

impl RuntimeHandles {
    #[allow(clippy::too_many_arguments)] // one-time construction in `run()`
    pub fn new(
        worker_tx: UnboundedSender<RuntimeEvent>,
        api_handle: crate::api::ApiHandle,
        lyrics_handle: crate::lyrics::LyricsHandle,
        artwork_handle: crate::artwork::ArtworkHandle,
        download_handle: crate::download::DownloadHandle,
        resolver_handle: crate::resolver::ResolverHandle,
        ai_handle: Option<crate::ai::AiHandle>,
        scrobble_handle: crate::scrobble::ScrobbleHandle,
    ) -> Self {
        Self {
            worker_tx,
            player_handle: None,
            pending_player_cmds: Vec::new(),
            player_failed: false,
            _mpv_guard: None,
            api_handle,
            lyrics_handle,
            artwork_handle,
            download_handle,
            resolver_handle,
            ai_handle,
            scrobble_handle,
            transfer_handle: None,
        }
    }

    /// Feed the scrobbler the same snapshot the loop is about to publish to the OS media
    /// session. Deliberately independent of that session's enabled state — scrobbling
    /// must survive `media_controls: false`.
    pub fn scrobble_observe(&mut self, snapshot: &crate::media::MediaSnapshot) {
        self.scrobble_handle.observe(snapshot);
    }

    /// Best-effort queue flush on quit, bounded by `budget`.
    pub async fn scrobble_shutdown(&self, budget: std::time::Duration) {
        let done = self.scrobble_handle.shutdown_flush();
        let _ = tokio::time::timeout(budget, done).await;
    }

    pub fn handle_player_ready(
        &mut self,
        result: Result<(PlayerHandle, crate::player::Mpv), String>,
        cfg: &PlayerRuntimeConfig,
        app: &mut App,
    ) {
        match result {
            Ok((handle, guard)) => {
                handle.send(PlayerCmd::SetVolume(cfg.volume));
                if (app.playback.speed - 1.0).abs() > f64::EPSILON {
                    handle.send(PlayerCmd::SetProperty {
                        name: "speed".to_owned(),
                        value: serde_json::Value::from(app.playback.speed),
                    });
                }
                if let Some(af) = crate::eq::build_af_string(&app.audio.bands, app.audio.normalize)
                {
                    handle.send(PlayerCmd::SetAudioFilter(af));
                }
                if let Ok(url) = std::env::var("YTM_PLAY_URL") {
                    handle.load(url);
                }
                for cmd in self.pending_player_cmds.drain(..) {
                    handle.send(cmd);
                }
                self.player_handle = Some(handle);
                self._mpv_guard = Some(guard);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to start mpv");
                self.player_failed = true;
                self.pending_player_cmds.clear();
                if app.status.text.is_empty() {
                    app.status.text = format!(
                        "{}: {e}",
                        crate::t!("mpv unavailable", "mpv를 사용할 수 없음")
                    );
                    app.dirty = true;
                }
            }
        }
    }

    pub fn dispatch(&mut self, app: &mut App, cmd: Cmd) {
        match cmd {
            Cmd::Player(pc) => {
                if let Some(p) = &self.player_handle {
                    p.send(pc);
                } else if !self.player_failed {
                    self.pending_player_cmds.push(pc);
                }
            }
            Cmd::Search {
                query,
                source,
                config,
            } => self.api_handle.search(query, source, config),
            Cmd::SaveLibrary => {
                if let Err(e) = app.library.save() {
                    tracing::warn!(error = %e, "failed to save library");
                }
            }
            Cmd::SaveDownloads => {
                if let Err(e) = app.download_store.save() {
                    tracing::warn!(error = %e, "failed to save downloads manifest");
                }
            }
            Cmd::SaveSignals => {
                if let Err(e) = app.signals.save() {
                    tracing::warn!(error = %e, "failed to save signals");
                }
            }
            Cmd::SaveRomanizedTitles => {
                if let Err(e) = app.romanization.cache.save() {
                    tracing::warn!(error = %e, "failed to save romanized title cache");
                }
            }
            Cmd::ClearRomanizedTitles => {
                if let Err(e) = crate::romanize::RomanizeCache::delete_saved() {
                    tracing::warn!(error = %e, "failed to delete romanized title cache");
                }
            }
            Cmd::ScanDownloads(dir) => {
                let songs = crate::library::scan_downloads(&dir);
                let _ = self
                    .worker_tx
                    .send(RuntimeEvent::App(Msg::DownloadsScanned(songs)));
            }
            Cmd::FetchLyrics {
                video_id,
                artist,
                title,
            } => {
                self.lyrics_handle.fetch(video_id, artist, title);
            }
            Cmd::FetchArtwork { video_id, source } => {
                self.artwork_handle.fetch(video_id, source);
            }
            Cmd::Download(song) => {
                if let Err(error) = self.download_handle.start(song) {
                    tracing::warn!(video_id = %error.video_id, "download queue full; dropping request");
                    let _ = self.worker_tx.send(RuntimeEvent::App(Msg::DownloadError {
                        video_id: error.video_id,
                        error: "Download queue is full; try again in a moment.".to_owned(),
                    }));
                }
            }
            Cmd::SetDownloadDir(dir) => {
                if !self.download_handle.set_dir(dir) {
                    tracing::warn!("download queue full; could not update download directory");
                }
            }
            Cmd::Resolve {
                video_id,
                watch_url,
            } => {
                self.resolver_handle.resolve_or_log(video_id, watch_url);
            }
            Cmd::SaveConfig(cfg) => {
                if let Err(e) = cfg.save() {
                    tracing::warn!(error = %e, "failed to save config");
                }
            }
            Cmd::SavePlaylists => {
                if let Err(e) = app.playlists.save() {
                    tracing::warn!(error = %e, "failed to save playlists");
                }
            }
            Cmd::SaveStationProfile => {
                if let Err(e) = app.station.save() {
                    tracing::warn!(error = %e, "failed to save station profile");
                }
            }
            Cmd::AskAi { prompt, context } => {
                if let Some(h) = &self.ai_handle {
                    h.ask(prompt, context);
                }
            }
            Cmd::AiRerank {
                seed_video_id,
                prompt,
            } => {
                if let Some(h) = &self.ai_handle {
                    h.rerank(seed_video_id, prompt);
                }
            }
            Cmd::SummarizeFeedback { digest } => {
                if let Some(h) = &self.ai_handle {
                    h.summarize_feedback(digest);
                }
            }
            Cmd::RomanizeTitles { request_id, items } => {
                let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
                if let Some(h) = &self.ai_handle {
                    h.romanize(request_id, items);
                } else {
                    let _ = self.worker_tx.send(RuntimeEvent::App(Msg::RomanizedTitles {
                        request_id,
                        keys,
                        entries: Vec::new(),
                    }));
                }
            }
            Cmd::StreamingFallback {
                seed,
                seed_video_id,
                exclude_ids,
                mode,
                config,
            } => {
                self.api_handle.streaming(
                    seed,
                    seed_video_id,
                    exclude_ids,
                    crate::app::STREAMING_POOL_COUNT,
                    mode,
                    config,
                );
            }
            Cmd::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                self.api_handle
                    .streaming_preflight(seed_video_id, picks, fallback, mode, config);
            }
            Cmd::SetAiModel(model) => {
                if let Some(h) = &self.ai_handle {
                    h.set_model(model);
                }
            }
            Cmd::ReloadAi {
                key,
                model,
                assistant_enabled,
            } => {
                self.ai_handle = key.and_then(|k| {
                    crate::ai::spawn(&k, model, sink(self.worker_tx.clone(), RuntimeEvent::Ai))
                });
                app.ai.available = assistant_enabled && self.ai_handle.is_some();
            }
            Cmd::ScrobbleAuthStart => self.scrobble_handle.auth_start(),
            Cmd::ScrobbleReconfigure(settings) => self.scrobble_handle.reconfigure(*settings),
            Cmd::Transfer(cmd) => {
                let handle = self.transfer_handle.get_or_insert_with(|| {
                    crate::transfer::actor::spawn(sink(
                        self.worker_tx.clone(),
                        RuntimeEvent::Transfer,
                    ))
                });
                handle.send(cmd);
            }
        }
    }
}
