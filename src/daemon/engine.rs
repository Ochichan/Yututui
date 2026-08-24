use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::ytmusic::YtMusicApi;
use crate::api::{ApiEvent, Song};
use crate::config::{Config, clamp_seek_seconds, clamp_speed};
use crate::eq;
use crate::library::Library;
use crate::player::{self, PlayerCmd, PlayerEvent, PlayerHandle};
use crate::queue::{Queue, QueueSnapshot};
use crate::remote::proto::{
    ArtworkRef, InstanceMode, LongFormSeekRuntimeSnapshot, QueueItemSnapshot,
    REMOTE_MAX_QUERY_BYTES, RemoteCommand, RemoteResponse, RemoteSettingChange, SettingsSnapshot,
    StatusSnapshot, ToggleState,
};
use crate::search_source::SearchConfig;
use crate::session::{LastMode, SessionCache};
#[cfg(test)]
use crate::signals;
use crate::signals::Signals;
use crate::station::StationStore;
use crate::streaming::{StreamingConfig, StreamingMode};
use crate::tools::PlaybackFailureClass;
use crate::util::sanitize;
use yututui_core::sleep_timer::{SLEEP_MAX_MINUTES, SleepStep, SleepTimer};

mod delivery;
mod media_session;
mod open_subsonic_bridge;
mod open_subsonic_runtime;
mod persistence_gate;
mod personal_export;
pub(super) mod personal_sync;
mod remote_dispatch;
mod streaming;
mod transport;
use self::streaming::PendingStreamingRequest;
#[cfg(test)]
use self::streaming::StreamingRequestStage;

#[path = "engine_session.rs"]
mod engine_session;

pub use delivery::EngineError;
use delivery::{record_player_delivery, require_player_delivery};
#[cfg(test)]
use engine_session::SESSION_EVENTS_CAP;
use engine_session::{DaemonOutcome, DaemonSessionEvent, data_dir};
#[cfg(test)]
pub(in crate::daemon) use persistence_gate::fail_store_saves_for_test;
#[cfg(test)]
use transport::TransportRecovery;
use transport::TransportRecoveryState;

// Autoplay/streaming policy + volume bounds are single-sourced with the TUI App in
// `crate::playback_policy`, so a threshold can't drift between the two playback owners.
#[cfg(test)]
use crate::playback_policy::{AUTOPLAY_MAX_FAILURES, AUTOPLAY_THRESHOLD, STREAMING_POOL_COUNT};
use crate::playback_policy::{
    MAX_CONSECUTIVE_PLAY_ERRORS, PlaybackModeAction, PlaybackModeState, VOLUME_MAX, VOLUME_STEP,
};
#[cfg(test)]
use crate::streaming::CandidateSource;

mod media_projection;

pub struct EngineOptions {
    pub resume: bool,
}

/// The persisted state the engine runs on. [`DaemonEngine::start`] fills it from disk;
/// the parity harness fills it with defaults so engine construction stays hermetic.
pub(crate) struct EngineState {
    pub config: Config,
    pub station: StationStore,
    pub library: Library,
    pub playlists: crate::playlists::Playlists,
    pub signals: Signals,
}

#[derive(Debug)]
pub enum EngineEffect {
    StreamingFallback {
        request_id: u64,
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
        config: SearchConfig,
    },
    StreamingPreflight {
        request_id: u64,
        seed_video_id: String,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: StreamingConfig,
    },
    /// Playback self-heal: run a yt-dlp update check off-loop (extraction-shaped
    /// failure on `video_id`). The serve loop answers with
    /// [`DaemonEngine::handle_heal_result`].
    YtdlpSelfHeal {
        video_id: String,
        tools: crate::config::ToolsConfig,
    },
    /// Re-enter the daemon owner loop after a bounded transport-recovery backoff.
    /// The generation makes an already-satisfied or superseded retry inert.
    TransportRecoveryRetry {
        generation: u64,
        retry_after: Duration,
    },
}

pub struct DaemonEngine {
    maintainer: crate::util::background_task::BackgroundTask,
    player: Option<PlayerRuntime>,
    open_subsonic: open_subsonic_runtime::OpenSubsonicOwner,
    open_subsonic_rating_identity: Option<String>,
    open_subsonic_pending_rating: Option<open_subsonic_bridge::PendingOpenSubsonicRatingProjection>,
    open_subsonic_playlist_identity: Option<String>,
    open_subsonic_pending_playlist:
        Option<open_subsonic_bridge::PendingOpenSubsonicPlaylistProjection>,
    open_subsonic_pending_scrobbles: VecDeque<open_subsonic_bridge::PendingOpenSubsonicScrobble>,
    player_emit: Arc<dyn Fn(PlayerEvent) + Send + Sync>,
    queue: Queue,
    playback: DaemonPlayback,
    config: Config,
    library: Library,
    playlists: crate::playlists::Playlists,
    playlists_rev: u64,
    library_invalidations: u64,
    signals: Signals,
    station: StationStore,
    personal_state: crate::personal_state::PersonalStateV2,
    personal_state_revision_guard: crate::sync::OwnerRevisionGuard,
    personal_state_device_id: Option<crate::personal_state::DeviceId>,
    personal_sync_in_progress: bool,
    #[cfg(test)]
    personal_state_paths: crate::personal_state::PersonalStatePaths,
    loaded_video_id: Option<String>,
    /// One lifecycle owns automatic-restart gating and any current-track replay payload.
    transport_recovery: TransportRecoveryState,
    transport_recovery_generation: u64,
    source_recovery: crate::player::recovery::RecoveryPlanner,
    source_logical_generation: u64,
    source_file_generation: u64,
    /// Test-only deterministic player starts; production always calls `player::spawn`.
    #[cfg(test)]
    test_player_starts: VecDeque<PlayerRuntime>,
    streaming: bool,
    /// Compatibility projection; only the streaming request helpers update this bit.
    streaming_pending: bool,
    streaming_request_seq: u64,
    pending_streaming_request: Option<PendingStreamingRequest>,
    last_extend: Option<Instant>,
    consecutive_streaming_failures: u8,
    last_error: Option<String>,
    /// Set when any durable write fails during the current remote command. The
    /// command's success-shaped response is replaced with `durability_unconfirmed` while
    /// preserving any player-visible state that was already applied.
    remote_persistence_write_failed: bool,
    /// Persistence-only diagnostic state. Healthy probes clear this without erasing an
    /// unrelated player/transport `last_error`.
    remote_persistence_error: Option<String>,
    remote_persistence_command_active: bool,
    remote_persistence_read_only: bool,
    /// Test-only standing choice; see `silence_remote_persistence_for_test`.
    #[cfg(test)]
    persistence_disabled_for_test: bool,
    consecutive_play_errors: u8,
    /// yt-dlp self-heal bookkeeping (mirrors the TUI's `YtdlpHeal`): the in-flight
    /// healed track, the per-track one-shot guard, and the update-check cooldown.
    heal_pending: Option<String>,
    heal_attempted: HashSet<String>,
    heal_last_check: Option<Instant>,
    last_mode: LastMode,
    inactive_normal_queue: Option<Arc<QueueSnapshot>>,
    inactive_radio_queue: Option<Arc<QueueSnapshot>>,
    inactive_local_queue: Option<Arc<QueueSnapshot>>,
    session_events: VecDeque<DaemonSessionEvent>,
    /// The media-session artwork cache's resolved file for a track, keyed by
    /// `video_id`; surfaced in [`Self::media_snapshot`] while the keys match.
    media_art: Option<crate::media::artwork::MediaArtworkReady>,
    /// Armed sleep timer; the shared core state machine keeps App and daemon fade/fire
    /// semantics identical. Ticked by the owner loop's 1 Hz sleep pump.
    sleep_timer: Option<yututui_core::sleep_timer::SleepTimer>,
}

struct PlayerRuntime {
    handle: PlayerHandle,
    _guard: Option<player::Mpv>,
}

#[derive(Debug, Clone, Copy)]
struct DaemonPlayback {
    paused: bool,
    volume: i64,
    time_pos: Option<f64>,
    /// When `time_pos` was last (re)based — the OS media session interpolates the live
    /// position from this anchor while playing (rebased on pause/resume/seek too).
    time_pos_at: Option<Instant>,
    /// Bumped on every position discontinuity (seek / track (re)start) so the media
    /// session re-announces the position; playback progress never bumps it.
    position_epoch: u64,
    duration: Option<f64>,
    /// Live playback speed (session-scoped, seeded from config; MPRIS `Rate` writes it).
    speed: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PositionEpochReason {
    Seek,
    TrackRestart,
    SourceRecovery,
    TransportRecovery,
    IdleReset,
}

impl DaemonEngine {
    pub async fn start<F, B>(
        options: EngineOptions,
        emit: F,
        bridge_emit: B,
        ready_emit: open_subsonic_runtime::OpenSubsonicReadySink,
    ) -> Result<Self, EngineError>
    where
        F: Fn(PlayerEvent) + Send + Sync + 'static,
        B: Fn(crate::open_subsonic::OpenSubsonicBridgeImport) + Send + Sync + 'static,
    {
        let (config, startup) =
            crate::persist::load_verified_startup_state().map_err(EngineError::from)?;
        let crate::persist::StartupStoreSet {
            personal_state,
            personal_state_device_id,
            library,
            playlists,
            session_cache,
            signals,
            station,
            ..
        } = startup;
        let state = EngineState {
            config,
            station,
            library,
            playlists,
            signals,
        };
        crate::persist::ensure_startup_recovery_coherent().map_err(EngineError::from)?;
        // Reap only after recovery stores establish one coherent startup frontier.
        if let Some(dir) = data_dir() {
            player::lifetime::reap_orphans(&dir);
        }
        let mut engine = Self::with_state(state, Arc::new(emit));
        engine.install_personal_state(personal_state);
        engine.personal_state_device_id = personal_state_device_id;
        // Optional server discovery must not hold local restore or player startup hostage.
        open_subsonic_runtime::initialize(&mut engine, Arc::new(bridge_emit), ready_emit);
        // External tool discovery remains required for ordinary YouTube/local playback.
        crate::tools::init(&engine.config.tools).await;
        // Keep the managed copy fresh; the daemon has no status-line emitter.
        engine.maintainer =
            crate::tools::ytdlp::spawn_maintainer(engine.config.tools.clone(), |_| {});
        if options.resume {
            engine.restore_session_cache(session_cache);
            if engine.queue.current().is_some() {
                engine.load_current().await?;
            }
        }
        Ok(engine)
    }

    /// Construct explicit state; [`start`] adds disk loads, while parity tests stay hermetic
    ///.
    pub(crate) fn with_state(
        state: EngineState,
        player_emit: Arc<dyn Fn(PlayerEvent) + Send + Sync>,
    ) -> Self {
        let EngineState {
            mut config,
            station,
            library,
            playlists,
            signals,
        } = state;
        let personal_state =
            crate::personal_state::legacy_state(&library, &playlists, &signals, &station)
                .unwrap_or_default();
        if let Some(profile) = &station.active {
            config.streaming.mode = profile.explore.to_mode();
        }

        let mut queue = Queue::default();
        queue.repeat = config.effective_repeat();
        queue.set_shuffle(config.effective_shuffle());

        Self {
            maintainer: crate::util::background_task::BackgroundTask::disabled("yt-dlp maintainer"),
            player: None,
            open_subsonic: Default::default(),
            open_subsonic_rating_identity: None,
            open_subsonic_pending_rating: None,
            open_subsonic_playlist_identity: None,
            open_subsonic_pending_playlist: None,
            open_subsonic_pending_scrobbles: VecDeque::new(),
            player_emit,
            queue,
            playback: DaemonPlayback {
                paused: true,
                volume: config.volume.clamp(0, VOLUME_MAX),
                time_pos: None,
                time_pos_at: None,
                position_epoch: 0,
                duration: None,
                speed: config.effective_speed(),
            },
            // Music-mode invariant: never start with both autoplay and repeat on (drop
            // streaming, keep the deliberate repeat) — matches the App's `apply_config`.
            streaming: crate::playback_policy::streaming_enabled_with_repeat(
                config.effective_autoplay_streaming(),
                config.effective_repeat(),
            ),
            config,
            library,
            playlists,
            playlists_rev: 0,
            library_invalidations: 0,
            signals,
            station,
            personal_state_revision_guard: Default::default(),
            personal_state,
            personal_state_device_id: None,
            personal_sync_in_progress: false,
            #[cfg(test)]
            personal_state_paths: tests::personal_state_paths(),
            loaded_video_id: None,
            transport_recovery: TransportRecoveryState::Armed,
            transport_recovery_generation: 0,
            source_recovery: crate::player::recovery::RecoveryPlanner::default(),
            source_logical_generation: 0,
            source_file_generation: 0,
            #[cfg(test)]
            test_player_starts: VecDeque::new(),
            streaming_pending: false,
            streaming_request_seq: 0,
            pending_streaming_request: None,
            last_extend: None,
            consecutive_streaming_failures: 0,
            last_error: None,
            remote_persistence_write_failed: false,
            remote_persistence_error: None,
            remote_persistence_command_active: false,
            remote_persistence_read_only: false,
            #[cfg(test)]
            persistence_disabled_for_test: false,
            consecutive_play_errors: 0,
            heal_pending: None,
            heal_attempted: HashSet::new(),
            heal_last_check: None,
            last_mode: LastMode::Normal,
            inactive_normal_queue: None,
            inactive_radio_queue: None,
            inactive_local_queue: None,
            session_events: VecDeque::new(),
            media_art: None,
            sleep_timer: None,
        }
    }

    /// Test-only queue seeding through the real snapshot-restore path (the same
    /// mechanism `restore_last_session` uses), so parity tests never reach for mpv
    /// or the on-disk session cache. The RNG seed keeps shuffle deterministic across
    /// the two owners under the shared parity script.
    #[cfg(test)]
    pub(crate) fn restore_queue_snapshot(
        &mut self,
        snapshot: crate::queue::QueueSnapshot,
        rng_seed: u64,
    ) {
        self.queue.restore_snapshot(snapshot);
        self.queue.seed_rng(rng_seed);
    }

    /// Test-only mode seeding through the same persisted enum the daemon restores at startup.
    #[cfg(test)]
    pub(crate) fn restore_last_mode_for_test(&mut self, mode: LastMode) {
        if self.last_mode != mode {
            self.cancel_pending_streaming_request();
        }
        self.last_mode = mode;
    }

    #[cfg(test)]
    pub(crate) fn install_seek_parity_player(
        &mut self,
        video_id: &str,
        position: f64,
        duration: f64,
    ) -> tokio::sync::mpsc::Receiver<PlayerCmd> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        self.player = Some(PlayerRuntime {
            handle: PlayerHandle::test_handle(tx),
            _guard: None,
        });
        self.loaded_video_id = Some(video_id.to_owned());
        self.playback.time_pos = Some(position);
        self.playback.duration = Some(duration);
        rx
    }

    #[cfg(test)]
    pub(crate) fn queue_transport_recovery_parity_player(
        &mut self,
    ) -> tokio::sync::mpsc::Receiver<PlayerCmd> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        self.test_player_starts.push_back(PlayerRuntime {
            handle: PlayerHandle::test_handle(tx),
            _guard: None,
        });
        rx
    }

    #[cfg(test)]
    pub(crate) fn seek_parity_projection(&self) -> (Option<f64>, u64) {
        (self.playback.time_pos, self.playback.position_epoch)
    }

    pub fn api_cookie(&self) -> Option<String> {
        self.config.effective_cookie()
    }

    /// Stop the daemon-owned long-lived tasks before persistence/scrobble teardown.
    pub async fn shutdown_background(&mut self) {
        self.open_subsonic.shutdown().await;
        self.maintainer.shutdown().await;
    }

    pub fn initial_effects(&mut self) -> Vec<EngineEffect> {
        self.maybe_autoplay_extend()
    }

    pub async fn handle_player_event(&mut self, event: PlayerEvent) -> Vec<EngineEffect> {
        if let Some(event_generation) = event.file_generation() {
            let current_generation = self
                .player
                .as_ref()
                .map(|player| player.handle.current_file_generation());
            if !self
                .player
                .as_ref()
                .is_some_and(|player| player.handle.event_is_current(&event))
            {
                tracing::debug!(
                    event_generation,
                    ?current_generation,
                    "ignored stale daemon audio terminal event"
                );
                return Vec::new();
            }
        }
        match event.into_unscoped() {
            PlayerEvent::TimePos(t) => {
                // Normalize at the mpv trust boundary (parity with the TUI reducer): a
                // NaN/inf/negative time-pos must not reach the position clock or media session.
                let t = crate::playback_policy::norm_position(t);
                self.playback.time_pos = Some(t);
                self.playback.time_pos_at = Some(Instant::now());
                if t > 0.0 {
                    self.consecutive_play_errors = 0;
                }
                Vec::new()
            }
            PlayerEvent::Duration(d) => {
                // Mirror of the TUI reducer (app/mod.rs `PlayerMsg::Duration`): `None`
                // clears the stored length instead of preserving a stale one.
                self.playback.duration = d.map(crate::playback_policy::norm_duration);
                Vec::new()
            }
            PlayerEvent::Paused(paused) => {
                if self.playback.paused != paused {
                    // Rebase the position clock on pause/resume so a long pause never
                    // reads as elapsed progress to the OS media session.
                    self.playback.time_pos_at = Some(Instant::now());
                }
                self.playback.paused = paused;
                Vec::new()
            }
            PlayerEvent::Volume(volume) => {
                // Ignore a non-finite report rather than muting / storing a garbage level.
                if let Some(volume) = crate::playback_policy::norm_volume_event(volume) {
                    self.playback.volume = volume;
                }
                Vec::new()
            }
            PlayerEvent::Metadata(_) => Vec::new(),
            // The headless engine has no live-sync surface; timeshift state is the TUI
            // reducer's concern (`PlayerMsg::CacheTime`).
            PlayerEvent::CacheTime(_) => Vec::new(),
            // Recording is a TUI-only feature; the headless engine ignores container hints.
            PlayerEvent::AudioCodec(_) | PlayerEvent::FileFormat(_) => Vec::new(),
            // Chapters drive the TUI seekbar only; the headless engine never renders them.
            PlayerEvent::Chapters(_) => Vec::new(),
            // Audio-output discovery and picker acknowledgements belong to the interactive TUI.
            PlayerEvent::AudioDeviceList(_)
            | PlayerEvent::AudioDeviceRefreshFailed(_)
            | PlayerEvent::AudioDeviceChanged(_)
            | PlayerEvent::CurrentAudioOutput(_)
            | PlayerEvent::AudioDeviceSelectionResult { .. } => Vec::new(),
            PlayerEvent::Eof => {
                self.record_outgoing(true);
                self.advance_after_end().await
            }
            PlayerEvent::Error(error) => self.handle_playback_error(error).await,
            PlayerEvent::TransportClosed(reason) => {
                let Some(generation) = self.handle_transport_closed(reason) else {
                    return Vec::new();
                };
                self.attempt_transport_recovery(generation).await
            }
            PlayerEvent::CacheEmergency {
                file_generation,
                position_secs,
                paused: _,
                reason,
            } => {
                let replacement = self
                    .player
                    .as_ref()
                    .map(|player| player.handle.current_file_generation())
                    != Some(file_generation);
                let generation = if replacement {
                    self.handle_cache_replacement_emergency(reason)
                } else {
                    // Cache actions outrank the actor command backlog, so this snapshot can be
                    // older than a seek/pause the daemon has already admitted. For the same file
                    // generation, preserve the authoritative owner projection and use the actor
                    // position only before the owner has observed one.
                    self.handle_cache_emergency(
                        self.playback.time_pos.unwrap_or(position_secs),
                        self.playback.paused,
                        reason,
                    )
                };
                let Some(generation) = generation else {
                    return Vec::new();
                };
                self.attempt_transport_recovery(generation).await
            }
            PlayerEvent::CacheReplacementEmergency { reason } => {
                let Some(generation) = self.handle_cache_replacement_emergency(reason) else {
                    return Vec::new();
                };
                self.attempt_transport_recovery(generation).await
            }
            PlayerEvent::FileScoped { .. } => {
                unreachable!("daemon player event was unscoped before reduction")
            }
        }
    }

    pub async fn handle_api_event(&mut self, event: ApiEvent) -> Vec<EngineEffect> {
        match event {
            // Track resolution belongs to the TUI's "what's playing" overlay; the
            // headless engine never issues one.
            ApiEvent::TrackResolved { .. } => Vec::new(),
            event @ (ApiEvent::StreamingResults { .. }
            | ApiEvent::StreamingPreflighted { .. }
            | ApiEvent::StreamingError { .. }) => self.handle_streaming_api_event(event).await,
            // Playlist search/import is a TUI-only flow; the daemon never issues those
            // commands, so their answers are inert here.
            ApiEvent::ModeResolved { .. }
            | ApiEvent::SearchResults { .. }
            | ApiEvent::SearchError { .. }
            | ApiEvent::PlaylistTracks { .. }
            | ApiEvent::PlaylistTracksError { .. }
            | ApiEvent::ArtistPage { .. }
            | ApiEvent::ArtistPageError { .. } => Vec::new(),
        }
    }

    pub fn status(&self) -> StatusSnapshot {
        let (position, total) = if self.queue.is_empty() {
            (0, 0)
        } else {
            self.queue.position()
        };
        let current = self.queue.current();
        let mut settings = SettingsSnapshot::from_config(&self.config, false);
        settings.autoplay_streaming = self.streaming;
        settings.long_form_seek = self.player.as_ref().map(|player| {
            let runtime = player.handle.long_form_seek_runtime_status();
            LongFormSeekRuntimeSnapshot {
                effective: crate::remote::publish::long_form_seek_effective(
                    runtime.status.effective,
                ),
                reason: crate::remote::publish::long_form_seek_reason(runtime.status.reason),
                last_failure: runtime
                    .last_failure
                    .map(crate::remote::publish::long_form_seek_reason),
                last_cleanup_ms: runtime.last_cleanup_ms,
            }
        });
        StatusSnapshot {
            title: current.map(|song| crate::api::sanitize_title(&song.title)),
            artist: current.map(|song| crate::api::sanitize_artist(&song.artist)),
            paused: current.is_none() || self.playback.paused,
            volume: self.playback.volume,
            position,
            total,
            streaming: self.streaming_active(),
            owner_mode: InstanceMode::Daemon,
            settings,
            queue: self
                .queue
                .ordered_iter()
                .enumerate()
                .map(|(index, song)| QueueItemSnapshot {
                    title: crate::api::sanitize_title(&song.title),
                    artist: crate::api::sanitize_artist(&song.artist),
                    duration: crate::api::sanitize_duration(&song.duration),
                    current: index == self.queue.cursor_pos(),
                })
                .collect(),
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
            elapsed_ms: current.and(self.playback.time_pos).map(|pos| {
                // Interpolated to "now", mirroring the OS media-session clock, so the
                // mini player's progress bar is fresh at every poll.
                let mut pos = pos;
                if !self.playback.paused
                    && let Some(at) = self.playback.time_pos_at
                {
                    pos += at.elapsed().as_secs_f64() * self.playback.speed;
                }
                if let Some(duration) = self.playback.duration {
                    pos = pos.min(duration);
                }
                (pos.max(0.0) * 1000.0) as u64
            }),
            duration_ms: current
                .and(self.playback.duration)
                .map(|duration| (duration.max(0.0) * 1000.0) as u64),
            is_live: current.is_some_and(|song| song.is_radio_station()),
            queue_rev: Some(self.queue.rev()),
            track_id: current.map(|song| crate::api::sanitize_provider_id(&song.video_id)),
            position_epoch: self.playback.position_epoch,
            // Same current-track gate as the media snapshot below: stale art from the
            // previous track never rides a status reply.
            artwork: current.and_then(|song| {
                self.media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| ArtworkRef {
                        key: art.key.clone(),
                        path: Some(art.path.to_string_lossy().into_owned()),
                        mime: None,
                    })
            }),
            personal_sync: Some(self.personal_sync_status()),
            sleep_remaining_secs: self
                .sleep_timer
                .and_then(|timer| timer.remaining_secs(Instant::now())),
        }
    }

    /// Whether the OS media session should be live (the same config toggle the TUI uses).
    pub fn media_controls_enabled(&self) -> bool {
        self.config.effective_media_controls()
    }

    pub fn scrobble_settings(&self) -> crate::scrobble::ScrobbleSettings {
        self.config.scrobble_settings()
    }

    /// The v8 publisher's read view of this owner (the daemon analog of
    /// `App::core_view`). Interpolates elapsed to "now" from the same
    /// anchor the OS media session uses. EQ reflects config; the daemon has no ICY
    /// now-playing surface yet.
    #[cfg(test)]
    pub(crate) fn signals(&self) -> &Signals {
        &self.signals
    }

    pub(crate) fn core_view(&self) -> crate::remote::publish::CoreView<'_> {
        let cur = self.queue.current();
        crate::remote::publish::CoreView {
            queue: &self.queue,
            paused: self.playback.paused,
            volume: self.playback.volume,
            speed_tenths: (self.playback.speed * 10.0).round() as u16,
            elapsed_ms: cur.and(self.playback.time_pos).map(|mut pos| {
                if !self.playback.paused
                    && let Some(at) = self.playback.time_pos_at
                {
                    pos += at.elapsed().as_secs_f64() * self.playback.speed;
                }
                if let Some(duration) = self.playback.duration {
                    pos = pos.min(duration);
                }
                (pos.max(0.0) * 1000.0) as u64
            }),
            duration_ms: cur
                .and(self.playback.duration)
                .map(|duration| (duration.max(0.0) * 1000.0) as u64),
            position_epoch: self.playback.position_epoch,
            streaming: self.streaming_active(),
            radio_mode: self.last_mode == LastMode::Radio,
            stream_now_playing: None,
            owner_mode: crate::remote::proto::InstanceMode::Daemon,
            eq_preset: self.config.eq_preset.label(),
            eq_bands: self.config.effective_eq_bands(),
            eq_normalize: self.config.effective_normalize(),
            config: &self.config,
            long_form_seek_status: self
                .player
                .as_ref()
                .map(|player| player.handle.long_form_seek_status()),
            library: &self.library,
            signals: &self.signals,
            // Same current-track gate as status()/media_snapshot: stale art from the
            // previous track never rides a push.
            artwork: cur.and_then(|song| {
                self.media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| crate::remote::publish::CoreArtwork {
                        key: &art.key,
                        path: Some(art.path.as_path()),
                        mime: None,
                    })
            }),
        }
    }

    fn restore_session_cache(&mut self, cache: SessionCache) {
        self.cancel_pending_streaming_request();
        self.last_mode = cache.last_mode;
        let active_queue = cache.active_queue().cloned();
        self.inactive_normal_queue = cache.normal_queue.map(Arc::new);
        self.inactive_radio_queue = cache.radio_queue.map(Arc::new);
        self.inactive_local_queue = cache.local_queue.map(Arc::new);

        if let Some(snapshot) = active_queue {
            self.queue.restore_snapshot(snapshot);
            self.reset_idle_playback();
            return;
        }

        let songs: Vec<Song> = match cache.last_mode {
            LastMode::Radio => self.library.radios.iter().cloned().collect(),
            LastMode::Normal => self.library.history.iter().cloned().collect(),
            LastMode::Local => Vec::new(),
        };
        if !songs.is_empty() {
            self.queue.set(songs, 0);
            self.reset_idle_playback();
        }
    }

    async fn resume_session(&mut self) -> RemoteResponse {
        let cache = SessionCache::load();
        if let Err(error) = persistence_gate::current_recovery_status() {
            return self.reject_remote_recovery(error);
        }
        self.restore_session_cache(cache);
        if self.queue.current().is_none() {
            return RemoteResponse::err("session_empty");
        }
        self.load_current()
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn next_track(&mut self) -> RemoteResponse {
        if self.queue.is_empty() {
            return RemoteResponse::err("queue_empty");
        }
        let previous = self.queue.snapshot();
        if self.queue.next(false).is_some() {
            return self
                .load_current_or_restore_queue(previous)
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
        }
        self.stop_playback();
        RemoteResponse::err("queue_end")
    }

    async fn prev_track(&mut self) -> RemoteResponse {
        if self.queue.is_empty() {
            return RemoteResponse::err("queue_empty");
        }
        let previous = self.queue.snapshot();
        self.queue.prev();
        self.load_current_or_restore_queue(previous)
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn queue_play(&mut self, position: usize) -> RemoteResponse {
        if position >= self.queue.len() {
            return RemoteResponse::err("queue_index");
        }
        let previous = self.queue.snapshot();
        self.queue.goto(position);
        self.load_current_or_restore_queue(previous)
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn queue_remove(&mut self, position: usize) -> RemoteResponse {
        let len_before = self.queue.len();
        if position >= len_before {
            return RemoteResponse::err("queue_index");
        }

        let current_pos = self.queue.cursor_pos();
        let removed_current = position == current_pos;
        let next_pos_after_removal = if removed_current && len_before > 1 {
            if position + 1 < len_before {
                Some(position)
            } else if self.queue.repeat == crate::queue::Repeat::All {
                Some(0)
            } else {
                None
            }
        } else {
            None
        };
        let previous = self.queue.snapshot();
        let current_changed = self.queue.remove_at(position).unwrap_or(false);

        if current_changed {
            if let Some(next_pos) = next_pos_after_removal {
                self.queue.goto(next_pos);
                return self
                    .load_current_or_restore_queue(previous)
                    .await
                    .map(|_| RemoteResponse::status(self.status()))
                    .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
            }
            self.stop_playback();
        }

        self.save_session();
        RemoteResponse::status(self.status())
    }

    async fn toggle_pause(&mut self) -> RemoteResponse {
        if self.queue.is_empty() {
            return RemoteResponse::err("queue_empty");
        }
        if self.loaded_video_id.as_deref()
            != self.queue.current().map(|song| song.video_id.as_str())
        {
            return self
                .load_current()
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
        }
        if let Err(error) = self.send_active_player_command("cycle_pause", PlayerCmd::CyclePause) {
            return self.reject_player_command(error);
        }
        self.source_recovery.supersede_transport();
        self.playback.paused = !self.playback.paused;
        RemoteResponse::status(self.status())
    }

    async fn search_and_play(&mut self, query: String) -> RemoteResponse {
        if query.trim().len() > REMOTE_MAX_QUERY_BYTES {
            return RemoteResponse::err("query_too_long");
        }
        let song = match self.first_search_result(&query).await {
            Ok(Some(song)) => song,
            Ok(None) => return RemoteResponse::err("no_results"),
            Err(()) => return RemoteResponse::err("search_error"),
        };
        let previous = self.queue.snapshot();
        if !self.queue.play_now(song) {
            return RemoteResponse::err("queue_full");
        }
        match self.load_current_or_restore_queue(previous).await {
            Ok(()) => RemoteResponse::status(self.status()),
            Err(error) => RemoteResponse::err(error.reason()),
        }
    }

    async fn search_and_enqueue(&mut self, query: String) -> RemoteResponse {
        if query.trim().len() > REMOTE_MAX_QUERY_BYTES {
            return RemoteResponse::err("query_too_long");
        }
        let song = match self.first_search_result(&query).await {
            Ok(Some(song)) => song,
            Ok(None) => return RemoteResponse::err("no_results"),
            Err(()) => return RemoteResponse::err("search_error"),
        };
        let previous = self.queue.snapshot();
        let old_len = self.queue.len();
        let was_idle = self.loaded_video_id.is_none();
        let added = if self.config.effective_enqueue_next() && !was_idle {
            self.queue.insert_next_many(vec![song])
        } else {
            self.queue.extend(vec![song])
        };
        if added == 0 {
            return RemoteResponse::err("queue_full");
        }
        if was_idle {
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            return match self.load_current_or_restore_queue(previous).await {
                Ok(()) => RemoteResponse::status(self.status()),
                Err(error) => RemoteResponse::err(error.reason()),
            };
        }
        self.save_session();
        RemoteResponse::status(self.status())
    }

    async fn first_search_result(&mut self, query: &str) -> Result<Option<Song>, ()> {
        let config = self.config.effective_search();
        let source = config.normalized_source(config.source);
        let api = match self.config.effective_cookie() {
            Some(cookie) => match YtMusicApi::from_cookie(&cookie).await {
                Ok(api) => api,
                Err(e) => {
                    let error = sanitize::sanitize_error_text(format!("{e:#}"));
                    tracing::warn!(%error, "daemon search cookie auth failed; using anonymous search");
                    YtMusicApi::Anonymous
                }
            },
            None => YtMusicApi::Anonymous,
        };
        match api.search_songs(query, source, &config).await {
            Ok(songs) => Ok(songs.into_iter().next()),
            Err(e) => {
                let error = sanitize::sanitize_error_text(format!("{e:#}"));
                self.last_error = Some(format!("search failed: {error}"));
                let query_log = crate::util::query::query_log_preview(query);
                tracing::warn!(
                    query_bytes = query_log.bytes,
                    query_chars = query_log.chars,
                    query_preview = %query_log.preview,
                    query_truncated = query_log.truncated,
                    %error,
                    "daemon remote search failed"
                );
                Err(())
            }
        }
    }

    fn adjust_volume(&mut self, delta: i64) -> RemoteResponse {
        self.set_volume(self.playback.volume + delta)
    }

    fn set_volume(&mut self, percent: i64) -> RemoteResponse {
        let volume = percent.clamp(0, VOLUME_MAX);
        if let Err(error) =
            self.send_player_command_if_active("set_volume", PlayerCmd::SetVolume(volume))
        {
            return self.reject_player_command(error);
        }
        self.playback.volume = volume;
        RemoteResponse::status(self.status())
    }

    /// Arm the sleep timer through the shared core state machine (the App's
    /// [`crate::app::App::arm_sleep_timer`] uses the same policy, so fade/fire never drift).
    fn arm_sleep(&mut self, minutes: u32) -> RemoteResponse {
        let minutes = minutes.clamp(1, SLEEP_MAX_MINUTES);
        let fade_secs = self.config.sleep_timer.effective_fade_secs();
        self.sleep_timer = Some(SleepTimer::armed(Instant::now(), minutes, fade_secs));
        RemoteResponse::status(self.status())
    }

    /// Cancel the timer; a fade in progress restores its pre-fade volume.
    fn cancel_sleep(&mut self) -> RemoteResponse {
        if let Some(timer) = self.sleep_timer
            && timer.fading
            && let Some(pre) = timer.pre_fade_volume
            && self.playback.volume != pre
        {
            let _ = self.send_player_command_if_active("sleep_restore", PlayerCmd::SetVolume(pre));
            self.playback.volume = pre;
        }
        self.sleep_timer = None;
        RemoteResponse::status(self.status())
    }

    /// Whether the owner loop should arm the 1 Hz sleep pump.
    pub(crate) fn sleep_timer_active(&self) -> bool {
        self.sleep_timer.is_some()
    }

    /// One owner-loop tick while a timer is armed: advance the fade or fire at the
    /// deadline. Returns `true` when player-visible state changed (so the loop can
    /// republish the watch feed).
    pub(crate) fn sleep_tick(&mut self) -> bool {
        let Some(mut timer) = self.sleep_timer else {
            return false;
        };
        let now = Instant::now();
        match timer.advance(now, self.playback.volume) {
            SleepStep::Idle => {
                self.sleep_timer = Some(timer);
                false
            }
            SleepStep::Volume(volume) => {
                self.sleep_timer = Some(timer);
                let _ =
                    self.send_player_command_if_active("sleep_fade", PlayerCmd::SetVolume(volume));
                self.playback.volume = volume;
                true
            }
            SleepStep::Fired => {
                let restore = timer.pre_fade_volume;
                self.sleep_timer = None;
                if let Some(pre) = restore {
                    let _ = self
                        .send_player_command_if_active("sleep_restore", PlayerCmd::SetVolume(pre));
                    self.playback.volume = pre;
                }
                if !self.playback.paused && !self.queue.is_empty() {
                    let _ = self.send_player_command_if_active("sleep_fire", PlayerCmd::CyclePause);
                    self.playback.paused = true;
                }
                true
            }
        }
    }

    fn seek(&mut self, seconds: f64) -> RemoteResponse {
        if self.loaded_video_id.is_none() {
            return RemoteResponse::err("nothing_playing");
        }
        // Optimistic position + epoch bump so the OS media session re-announces the
        // discontinuity immediately; mpv confirms via its next time-pos report.
        let mut target = (self.playback.time_pos.unwrap_or(0.0) + seconds).max(0.0);
        if let Some(d) = self.playback.duration {
            target = target.min(d);
        }
        if let Err(error) =
            self.send_active_player_command("seek_relative", PlayerCmd::SeekRelative(seconds))
        {
            return self.reject_player_command(error);
        }
        self.note_seek(target);
        RemoteResponse::status(self.status())
    }

    fn seek_to(&mut self, pos: f64) -> RemoteResponse {
        if self.loaded_video_id.is_none() {
            return RemoteResponse::err("nothing_playing");
        }
        let target = crate::playback_policy::clamp_seek_target(pos, self.playback.duration);
        if let Err(error) =
            self.send_active_player_command("seek_absolute", PlayerCmd::interactive_seek(target))
        {
            return self.reject_player_command(error);
        }
        self.note_seek(target);
        RemoteResponse::status(self.status())
    }

    /// Record a position discontinuity at `pos` (seek applied / track restarted).
    fn note_seek(&mut self, pos: f64) {
        self.source_recovery.supersede_transport();
        self.playback.time_pos = Some(pos);
        self.playback.time_pos_at = Some(Instant::now());
        self.bump_position_epoch(PositionEpochReason::Seek);
    }

    fn set_streaming(&mut self, state: ToggleState) -> (RemoteResponse, Vec<EngineEffect>) {
        if self.last_mode == LastMode::Local {
            return (
                RemoteResponse::err("streaming_unavailable_in_local_mode"),
                Vec::new(),
            );
        }
        let on = state.resolve(self.streaming);
        let transition = PlaybackModeState::new(self.queue.repeat, self.streaming)
            .transition(PlaybackModeAction::SetStreaming(on));
        let Ok(transition) = transition else {
            return (
                RemoteResponse::err("incompatible_playback_modes"),
                Vec::new(),
            );
        };
        self.streaming = transition.state.autoplay_streaming;
        self.config.autoplay_streaming = Some(self.streaming);
        if self.streaming {
            self.consecutive_streaming_failures = 0;
        } else {
            self.cancel_pending_streaming_request();
        }
        self.save_config("daemon autoplay streaming setting");
        let effects = if self.streaming {
            self.force_autoplay_extend()
        } else {
            Vec::new()
        };
        (RemoteResponse::status(self.status()), effects)
    }

    fn set_setting(&mut self, change: RemoteSettingChange) -> (RemoteResponse, Vec<EngineEffect>) {
        match change {
            RemoteSettingChange::AutoplayStreaming { value } => self.set_streaming(if value {
                ToggleState::On
            } else {
                ToggleState::Off
            }),
            RemoteSettingChange::StreamingMode { value } => {
                if self.config.streaming.mode != value {
                    self.cancel_pending_streaming_request();
                }
                self.config.streaming.mode = value;
                self.save_config("daemon streaming mode setting");
                let effects = if self.streaming {
                    self.force_autoplay_extend()
                } else {
                    Vec::new()
                };
                (RemoteResponse::status(self.status()), effects)
            }
            RemoteSettingChange::StreamingSource { value } => {
                let search = self.config.effective_search();
                let value = search.normalized_streaming_source(value);
                if self.config.effective_search().streaming_source != value {
                    self.cancel_pending_streaming_request();
                }
                self.config.search.streaming_source = value;
                self.save_config("daemon streaming source setting");
                let effects = if self.streaming {
                    self.force_autoplay_extend()
                } else {
                    Vec::new()
                };
                (RemoteResponse::status(self.status()), effects)
            }
            RemoteSettingChange::Speed { tenths } => {
                let speed = clamp_speed(f64::from(tenths) / 10.0);
                let delivery = self.send_player_command_if_active(
                    "set_speed",
                    PlayerCmd::SetProperty {
                        name: "speed".to_owned(),
                        value: Value::from(speed),
                    },
                );
                if let Err(error) = delivery {
                    return (self.reject_player_command(error), Vec::new());
                }
                self.config.speed = Some(speed);
                self.playback.speed = speed;
                self.save_config("daemon speed setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::SeekSeconds { seconds } => {
                self.config.seek_seconds = Some(clamp_seek_seconds(f64::from(seconds)));
                self.save_config("daemon seek step setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::Normalize { value } => {
                let previous = self.config.normalize;
                self.config.normalize = Some(value);
                if let Err(error) = self.apply_audio_filter() {
                    self.config.normalize = previous;
                    return (self.reject_player_command(error), Vec::new());
                }
                self.save_config("daemon normalize setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::Gapless { value } => {
                self.config.gapless = Some(value);
                self.save_config("daemon gapless setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::AiEnabled { value } => {
                self.config.ai_enabled = Some(value);
                self.save_config("daemon DJ Gem setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::RadioMode { .. } => {
                (RemoteResponse::err("radio_mode_unavailable"), Vec::new())
            }
        }
    }

    async fn advance_after_end(&mut self) -> Vec<EngineEffect> {
        let mut effects = Vec::new();
        if self.queue.next(true).is_some() {
            if let Err(e) = self.load_current().await {
                self.last_error = Some(e.to_string());
                self.stop_playback();
            }
        } else {
            // mpv may retain an EOF'd media (and its unlinked disk-cache allocation) under
            // keep-open. Use the same owner close boundary as an explicit queue-end Stop while
            // leaving the queue cursor/metadata intact for the idle projection.
            self.stop_playback();
        }
        effects.extend(self.maybe_autoplay_extend());
        effects
    }

    async fn handle_playback_error(&mut self, error: String) -> Vec<EngineEffect> {
        if self.try_source_recovery(&error) {
            return Vec::new();
        }
        let failure_class = crate::tools::classify_playback_failure(&error);
        // Self-heal (mirrors the TUI reducer): an extraction-shaped failure on a
        // yt-dlp-resolved track is the stale-yt-dlp signature — update in the
        // background and retry this track once. Unlike the TUI (whose session mpv
        // keeps its spawn-time ytdl_path), the daemon can simply drop its player:
        // the respawn re-pins ytdl_hook to the fresh binary.
        if failure_class == PlaybackFailureClass::Extraction
            && self.heal_pending.is_none()
            && let Some(song) = self.queue.current()
            && song.prefetch_target().is_some()
            && !self.heal_attempted.contains(&song.video_id)
            && self
                .heal_last_check
                .is_none_or(|at| at.elapsed() >= crate::tools::HEAL_COOLDOWN)
        {
            let video_id = song.video_id.clone();
            // Bound the per-track guard set (see the App reducer): a long-lived daemon resets
            // it after enough distinct healed tracks rather than growing for the process life.
            if self.heal_attempted.len() >= crate::playback_policy::HEAL_ATTEMPTED_MAX {
                self.heal_attempted.clear();
            }
            self.heal_attempted.insert(video_id.clone());
            self.heal_last_check = Some(Instant::now());
            self.heal_pending = Some(video_id.clone());
            self.last_error = Some(crate::tools::playback_failure_actionable_error(
                failure_class,
                &error,
            ));
            return vec![EngineEffect::YtdlpSelfHeal {
                video_id,
                tools: self.config.tools.clone(),
            }];
        }
        self.last_error = Some(crate::tools::playback_failure_actionable_error(
            failure_class,
            &error,
        ));
        self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
        if self.consecutive_play_errors <= MAX_CONSECUTIVE_PLAY_ERRORS
            && self.queue.peek_next().is_some()
        {
            self.queue.next(false);
            if let Err(e) = self.load_current().await {
                self.last_error = Some(e.to_string());
                self.stop_playback();
            }
            self.maybe_autoplay_extend()
        } else {
            self.stop_playback();
            Vec::new()
        }
    }

    /// Finish a self-heal round: a new binary landed -> respawn mpv (fresh ytdl_hook
    /// pin) and retry the same track once; otherwise fall back to the plain skip
    /// path (the per-track `heal_attempted` guard keeps it from looping).
    pub async fn handle_heal_result(
        &mut self,
        video_id: String,
        updated: bool,
    ) -> Vec<EngineEffect> {
        if self.heal_pending.as_deref() != Some(video_id.as_str()) {
            return Vec::new(); // stale: playback moved on
        }
        self.heal_pending = None;
        let still_current = self.queue.current().is_some_and(|s| s.video_id == video_id);
        if !still_current {
            return Vec::new();
        }
        if updated {
            self.player = None; // next ensure_player re-pins ytdl_hook
            if let Err(e) = self.load_current().await {
                self.last_error = Some(e.to_string());
                self.stop_playback();
            }
            return Vec::new();
        }
        self.handle_playback_error(
            "mpv could not play this track (unrecognized file format; yt-dlp already current)"
                .to_owned(),
        )
        .await
    }

    async fn load_current(&mut self) -> Result<(), EngineError> {
        self.ensure_player().await?;
        self.load_current_loaded()
    }

    fn stop_playback(&mut self) {
        self.source_recovery.supersede_transport();
        if let Some(player) = self.player.take() {
            record_player_delivery("stop", player.handle.send(PlayerCmd::Stop));
        }
        self.reset_idle_playback();
        self.loaded_video_id = None;
        self.transport_recovery.rearm_after_normal_load_or_stop();
    }

    fn reset_idle_playback(&mut self) {
        self.playback.paused = true;
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::IdleReset);
        self.playback.duration = None;
    }

    // INVARIANT(PLAY-EPOCH-001): every daemon position discontinuity bumps through this helper.
    fn bump_position_epoch(&mut self, _reason: PositionEpochReason) {
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
    }

    async fn ensure_player(&mut self) -> Result<(), EngineError> {
        if self.player.is_some() {
            return Ok(());
        }

        #[cfg(test)]
        if let Some(player) = self.test_player_starts.pop_front() {
            return self.configure_player_runtime(player);
        }

        let emit = Arc::clone(&self.player_emit);
        let (handle, guard) = player::spawn_with_route_provider(
            move |event| (emit)(event),
            data_dir(),
            self.config
                .cookies_file_for_external_tools(data_dir().as_deref()),
            self.config.effective_gapless(),
            self.config.audio.runtime(),
            self.open_subsonic.route_provider(),
        )
        .await
        .map_err(|e| EngineError::Player(format!("failed to start mpv: {e:#}")))?;

        self.configure_player_runtime(PlayerRuntime {
            handle,
            _guard: Some(guard),
        })
    }

    fn configure_player_runtime(&mut self, player: PlayerRuntime) -> Result<(), EngineError> {
        require_player_delivery(
            "volume",
            player
                .handle
                .send(PlayerCmd::SetVolume(self.playback.volume)),
        )?;
        let speed = self.playback.speed;
        if (speed - 1.0).abs() > f64::EPSILON {
            require_player_delivery(
                "speed",
                player.handle.send(PlayerCmd::SetProperty {
                    name: "speed".to_owned(),
                    value: Value::from(speed),
                }),
            )?;
        }
        require_player_delivery(
            "audio_filter",
            player
                .handle
                .send(PlayerCmd::SetAudioFilter(self.current_audio_filter())),
        )?;
        self.player = Some(player);
        Ok(())
    }

    pub(super) fn current_audio_filter(&self) -> String {
        eq::build_af_string(
            &self.config.effective_eq_bands(),
            self.config.effective_normalize(),
        )
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod delivery_tests;
#[cfg(test)]
mod local_mode_tests;
#[cfg(test)]
mod persistence_gate_tests;
#[cfg(test)]
mod streaming_request_tests;
#[cfg(test)]
pub(in crate::daemon) mod tests;
#[cfg(test)]
mod transport_tests;
