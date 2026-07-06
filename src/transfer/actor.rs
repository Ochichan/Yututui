//! The TUI-side transfer actor. Owns its own clients (deliberately NOT routed through
//! the interactive API actor, so a ten-minute import never queues behind — or starves —
//! search), runs one job at a time, and throttles progress events to ~1/s.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::checkpoint::TransferReport;
use super::engine::JobCtx;
use super::{JobSpec, TransferDest, TransferProgress, TransferSource, new_job_id, run_job};
use crate::config::Config;
use crate::spotify::auth;
use crate::spotify::client::SpotifyClient;

pub enum TransferCmd {
    /// Start the PKCE flow with the (possibly unsaved) draft Client ID.
    AuthStart {
        client_id: String,
        port: u16,
    },
    Disconnect,
    ListSpotifyPlaylists,
    StartJob(Box<JobSpec>),
    CancelJob,
}

/// Events back to the reducer. No secrets anywhere here.
pub enum TransferEvent {
    AuthUrl(String),
    AuthDone {
        display_name: String,
    },
    AuthError(String),
    Disconnected,
    SpotifyPlaylists(Result<Vec<PickerPlaylist>, String>),
    Progress(TransferProgress),
    JobDone(Box<TransferReport>),
    JobFailed {
        job_id: String,
        error: String,
        resumable: bool,
    },
}

/// What the picker popup needs per row.
#[derive(Clone)]
pub struct PickerPlaylist {
    pub source: TransferSource,
    pub label: String,
    pub total: u32,
}

type EventSink = Arc<dyn Fn(TransferEvent) + Send + Sync>;

pub struct TransferHandle {
    tx: UnboundedSender<TransferCmd>,
}

impl TransferHandle {
    pub fn send(&self, cmd: TransferCmd) {
        let _ = self.tx.send(cmd);
    }
}

pub fn spawn(emit: impl Fn(TransferEvent) + Send + Sync + 'static) -> TransferHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_actor(rx, Arc::new(emit)));
    TransferHandle { tx }
}

async fn run_actor(mut rx: UnboundedReceiver<TransferCmd>, emit: EventSink) {
    let mut auth_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut job_task: Option<(String, tokio::task::JoinHandle<()>)> = None;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            TransferCmd::AuthStart { client_id, port } => {
                if auth_task.as_ref().is_some_and(|t| !t.is_finished()) {
                    continue; // flow already open in the browser
                }
                let emit = Arc::clone(&emit);
                auth_task = Some(tokio::spawn(run_auth(client_id, port, emit)));
            }
            TransferCmd::Disconnect => match crate::spotify::auth::SpotifyToken::delete_saved() {
                Ok(()) => emit(TransferEvent::Disconnected),
                Err(e) => emit(TransferEvent::AuthError(format!(
                    "could not remove the token: {e}"
                ))),
            },
            TransferCmd::ListSpotifyPlaylists => {
                let emit = Arc::clone(&emit);
                tokio::spawn(async move {
                    emit(TransferEvent::SpotifyPlaylists(list_playlists().await));
                });
            }
            TransferCmd::StartJob(spec) => {
                if job_task.as_ref().is_some_and(|(_, t)| !t.is_finished()) {
                    emit(TransferEvent::JobFailed {
                        job_id: String::new(),
                        error: "a transfer is already running".to_owned(),
                        resumable: false,
                    });
                    continue;
                }
                let job_id = new_job_id(match spec.dest {
                    TransferDest::SpotifyNewPlaylist { .. } => "yt2sp",
                    _ => "sp2yt",
                });
                let emit = Arc::clone(&emit);
                let id_clone = job_id.clone();
                job_task = Some((
                    job_id,
                    tokio::spawn(async move { run_one_job(id_clone, *spec, emit).await }),
                ));
            }
            TransferCmd::CancelJob => {
                if let Some((job_id, task)) = job_task.take() {
                    // Aborting between awaits is safe: the checkpoint flushes at every
                    // chunk boundary and resume reconciles against the destination.
                    task.abort();
                    emit(TransferEvent::JobFailed {
                        job_id,
                        error: "cancelled".to_owned(),
                        resumable: true,
                    });
                }
            }
        }
    }
}

async fn run_auth(client_id: String, port: u16, emit: EventSink) {
    let http = reqwest::Client::builder()
        .user_agent(format!("ytm-tui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let emit_url = Arc::clone(&emit);
    let flow = auth::run_pkce_flow(&http, &client_id, port, &mut move |url| {
        emit_url(TransferEvent::AuthUrl(url));
    })
    .await;
    match flow {
        Ok(token) => {
            let mut client = SpotifyClient::with_token(token);
            let display_name = match client.me().await {
                Ok(user) => user.label().to_owned(),
                Err(e) => {
                    emit(TransferEvent::AuthError(e.to_string()));
                    return;
                }
            };
            emit(TransferEvent::AuthDone { display_name });
        }
        Err(e) => emit(TransferEvent::AuthError(
            crate::util::sanitize::sanitize_error_text(format!("{e:#}")),
        )),
    }
}

async fn list_playlists() -> Result<Vec<PickerPlaylist>, String> {
    let cfg = Config::load();
    let mut client =
        SpotifyClient::from_saved(cfg.spotify.client_id.as_deref()).map_err(|e| e.to_string())?;
    let playlists = client.my_playlists().await.map_err(|e| e.to_string())?;
    let mut items = vec![PickerPlaylist {
        source: TransferSource::SpotifyLiked,
        label: "Liked Songs".to_owned(),
        total: 0,
    }];
    items.extend(playlists.into_iter().map(|p| PickerPlaylist {
        source: TransferSource::SpotifyPlaylist { id: p.id },
        label: p.name,
        total: p.total,
    }));
    Ok(items)
}

async fn run_one_job(job_id: String, spec: JobSpec, emit: EventSink) {
    // Fresh config: picks up the cookie/market as they are *now*, not at spawn time.
    let cfg = Config::load();
    let mut ctx = match build_ctx(&spec, &cfg).await {
        Ok(ctx) => ctx,
        Err(error) => {
            emit(TransferEvent::JobFailed {
                job_id,
                error,
                resumable: false,
            });
            return;
        }
    };
    // Throttle the per-track beats to ~1/s for the status line.
    let mut last_beat: Option<Instant> = None;
    let emit_progress = Arc::clone(&emit);
    let mut progress = move |p: TransferProgress| {
        let due = last_beat.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due || p.done == p.total {
            last_beat = Some(Instant::now());
            emit_progress(TransferEvent::Progress(p));
        }
    };
    match run_job(job_id.clone(), spec, None, &mut ctx, &mut progress).await {
        Ok(report) => emit(TransferEvent::JobDone(Box::new(report))),
        Err(e) => emit(TransferEvent::JobFailed {
            job_id,
            error: format!("{:#}", e.error),
            resumable: e.resumable,
        }),
    }
}

async fn build_ctx(spec: &JobSpec, cfg: &Config) -> Result<JobCtx, String> {
    let needs_spotify = matches!(
        spec.source,
        TransferSource::SpotifyPlaylist { .. } | TransferSource::SpotifyLiked
    ) || matches!(spec.dest, TransferDest::SpotifyNewPlaylist { .. });
    // LocalPlaylist writes locally but still *matches* against YouTube Music.
    let needs_ytm = matches!(spec.source, TransferSource::YtmPlaylist { .. })
        || matches!(
            spec.dest,
            TransferDest::YtmNewPlaylist { .. }
                | TransferDest::YtmExistingPlaylist { .. }
                | TransferDest::YtmLikes
                | TransferDest::LocalPlaylist { .. }
        );
    let spotify = if needs_spotify {
        Some(
            SpotifyClient::from_saved(cfg.spotify.client_id.as_deref())
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let ytm = if needs_ytm {
        match cfg.effective_cookie() {
            Some(cookie) => Some(
                crate::api::ytmusic::YtMusicApi::from_cookie(&cookie)
                    .await
                    .map_err(|e| {
                        crate::util::sanitize::sanitize_error_text(format!(
                            "YouTube Music auth failed: {e:#}"
                        ))
                    })?,
            ),
            // No cookie is fine when YTM is only needed to *search* for matches (a LocalPlaylist
            // dest) — anonymous search falls back to yt-dlp, so a Spotify→Library import still
            // works. Reading a YTM playlist or writing to the account (new/existing playlist,
            // likes) genuinely needs the cookie, so those still fail with a clear message.
            None => {
                let account_op = matches!(spec.source, TransferSource::YtmPlaylist { .. })
                    || matches!(
                        spec.dest,
                        TransferDest::YtmNewPlaylist { .. }
                            | TransferDest::YtmExistingPlaylist { .. }
                            | TransferDest::YtmLikes
                    );
                if account_op {
                    return Err(
                        "this needs a YouTube Music cookie — set one in Settings › General"
                            .to_owned(),
                    );
                }
                Some(crate::api::ytmusic::YtMusicApi::Anonymous)
            }
        }
    } else {
        None
    };
    Ok(JobCtx {
        ytm,
        spotify,
        search_config: cfg.effective_search(),
        market: cfg.spotify.market.clone(),
    })
}
