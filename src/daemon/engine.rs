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
    ArtworkRef, InstanceMode, QueueItemSnapshot, REMOTE_MAX_GEMINI_KEY_BYTES,
    REMOTE_MAX_QUERY_BYTES, REMOTE_MAX_TRACK_IDS, RemoteCommand, RemoteResponse,
    RemoteSettingChange, SettingsSnapshot, StatusSnapshot, ToggleState,
};
use crate::search_source::SearchConfig;
use crate::session::{LastMode, SessionCache};
use crate::signals::{self, Signals};
use crate::station::StationStore;
use crate::streaming::{StreamingConfig, StreamingMode};
use crate::tools::PlaybackFailureClass;
use crate::util::sanitize;

mod delivery;
mod gui_search;
mod persistence_gate;
mod streaming;
mod transport;

pub use delivery::EngineError;
use delivery::{record_player_delivery, require_player_delivery};
pub(super) use gui_search::RequesterKey;
use gui_search::{GuiSearchAdmission, GuiSearchIndex};
use transport::TransportRecovery;

// Autoplay/streaming policy + volume bounds are single-sourced with the TUI App in
// `crate::playback_policy`, so a threshold can't drift between the two playback owners.
#[cfg(test)]
use crate::playback_policy::{AUTOPLAY_MAX_FAILURES, AUTOPLAY_THRESHOLD, STREAMING_POOL_COUNT};
use crate::playback_policy::{MAX_CONSECUTIVE_PLAY_ERRORS, VOLUME_MAX, VOLUME_STEP};
#[cfg(test)]
use crate::streaming::CandidateSource;

const SESSION_EVENTS_CAP: usize = 20;

pub struct EngineOptions {
    pub resume: bool,
}

/// The persisted state the engine runs on. [`DaemonEngine::start`] fills it from disk;
/// the parity harness fills it with defaults so engine construction stays hermetic.
pub(crate) struct EngineState {
    pub config: Config,
    pub station: StationStore,
    pub library: Library,
    pub signals: Signals,
}

#[derive(Debug)]
pub enum EngineEffect {
    StreamingFallback {
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
        config: SearchConfig,
    },
    StreamingPreflight {
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
    /// Run a GUI-session search off-loop (`RemoteCommand::RunSearch`); the answer
    /// returns as [`ApiEvent::GuiSearchCompleted`] and is pushed on the `search` topic.
    GuiSearch {
        requester: RequesterKey,
        ticket: u64,
        query: String,
        source: crate::search_source::SearchSource,
        config: SearchConfig,
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
    player_emit: Arc<dyn Fn(PlayerEvent) + Send + Sync>,
    queue: Queue,
    playback: DaemonPlayback,
    config: Config,
    library: Library,
    signals: Signals,
    station: StationStore,
    loaded_video_id: Option<String>,
    /// A dead transport's current-track identity and pause bit. The next explicit load
    /// consumes it without duplicating history/signals and restores the pause state.
    transport_recovery: Option<TransportRecovery>,
    /// Monotonic identity for scheduled transport retries. Stale retry events must never
    /// restart a newer player lifetime.
    transport_recovery_generation: u64,
    /// One-shot crash-loop gate. Only a normal (non-recovery) load rearms it; merely
    /// recreating mpv or receiving late telemetry from the dead actor does not.
    transport_auto_recovery_armed: bool,
    /// Deterministic player starts for transport-recovery tests. Production always takes
    /// the real `player::spawn` path.
    #[cfg(test)]
    test_player_starts: VecDeque<PlayerRuntime>,
    streaming: bool,
    streaming_pending: bool,
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
    consecutive_play_errors: u8,
    /// yt-dlp self-heal bookkeeping (mirrors the TUI's `YtdlpHeal`): the in-flight
    /// healed track, the per-track one-shot guard, and the update-check cooldown.
    heal_pending: Option<String>,
    heal_attempted: HashSet<String>,
    heal_last_check: Option<Instant>,
    last_mode: LastMode,
    inactive_normal_queue: Option<QueueSnapshot>,
    inactive_radio_queue: Option<QueueSnapshot>,
    inactive_local_queue: Option<QueueSnapshot>,
    session_events: VecDeque<DaemonSessionEvent>,
    /// The media-session artwork cache's resolved file for a track, keyed by
    /// `video_id`; surfaced in [`Self::media_snapshot`] while the keys match.
    media_art: Option<crate::media::artwork::MediaArtworkReady>,
    /// Per-session/page rows addressable by `play_tracks`/`enqueue_tracks`, hard-bounded by the
    /// remote session cap so reloads and reconnects cannot grow owner memory indefinitely.
    gui_search_index: GuiSearchIndex,
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
    TransportRecovery,
    IdleReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonOutcome {
    FullPlay,
    Skip,
    QuickSkip,
}

#[derive(Debug, Clone)]
struct DaemonSessionEvent {
    artist_key: String,
    outcome: DaemonOutcome,
    completion: f32,
}

impl DaemonEngine {
    pub async fn start<F>(options: EngineOptions, emit: F) -> Result<Self, EngineError>
    where
        F: Fn(PlayerEvent) + Send + Sync + 'static,
    {
        let (config, startup) =
            crate::persist::load_verified_startup_state().map_err(EngineError::from)?;
        let crate::persist::StartupStoreSet {
            library,
            session_cache,
            signals,
            station,
            ..
        } = startup;
        let state = EngineState {
            config,
            station,
            library,
            signals,
        };
        crate::persist::ensure_startup_recovery_coherent().map_err(EngineError::from)?;
        // Orphan reaping can unlink lifeline records and kill child processes. It is safe only
        // after all recovery-backed stores have established one coherent startup frontier.
        if let Some(dir) = data_dir() {
            player::lifetime::reap_orphans(&dir);
        }
        let mut engine = Self::with_state(state, Arc::new(emit));

        // Resolve which yt-dlp/mpv this process runs (managed vs system vs override)
        // before the first `ensure_player` — the mpv spawn pins ytdl_hook to it.
        crate::tools::init(&engine.config.tools).await;
        // Keep the managed copy fresh for long daemon runs. No-op emit: the daemon
        // has no status line and `check_and_update` already logs its outcomes.
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

    /// Construct the engine from explicit state — the single init path [`start`] wraps
    /// with disk loads, and the App↔Daemon parity harness constructs hermetically
    /// (docs/gui/10 §4; the engine must be buildable without touching `ProjectDirs`).
    pub(crate) fn with_state(
        state: EngineState,
        player_emit: Arc<dyn Fn(PlayerEvent) + Send + Sync>,
    ) -> Self {
        let EngineState {
            mut config,
            station,
            library,
            signals,
        } = state;
        if let Some(profile) = &station.active {
            config.streaming.mode = profile.explore.to_mode();
        }

        let mut queue = Queue::default();
        queue.repeat = config.effective_repeat();
        queue.set_shuffle(config.effective_shuffle());

        Self {
            maintainer: crate::util::background_task::BackgroundTask::disabled("yt-dlp maintainer"),
            player: None,
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
            signals,
            station,
            loaded_video_id: None,
            transport_recovery: None,
            transport_recovery_generation: 0,
            transport_auto_recovery_armed: true,
            #[cfg(test)]
            test_player_starts: VecDeque::new(),
            streaming_pending: false,
            last_extend: None,
            consecutive_streaming_failures: 0,
            last_error: None,
            remote_persistence_write_failed: false,
            remote_persistence_error: None,
            remote_persistence_command_active: false,
            remote_persistence_read_only: false,
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
            gui_search_index: GuiSearchIndex::default(),
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

    pub fn api_cookie(&self) -> Option<String> {
        self.config.effective_cookie()
    }

    /// Stop the daemon-owned long-lived tasks before persistence/scrobble teardown.
    pub async fn shutdown_background(&mut self) {
        self.maintainer.shutdown().await;
    }

    pub fn initial_effects(&mut self) -> Vec<EngineEffect> {
        self.maybe_autoplay_extend()
    }

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
        if let Some(response) = self.preflight_remote_persistence(&command) {
            return (response, false, Vec::new());
        }
        let mut effects = Vec::new();
        let shutdown = matches!(command, RemoteCommand::Quit);
        let response = match command {
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
                // Music-mode invariant (mirrors the App reducer for parity): refuse turning
                // repeat on while autoplay streaming is on. Off→All is the only enabling step.
                if self.queue.repeat.cycle_blocked_by_streaming(self.streaming) {
                    RemoteResponse::status(self.status())
                } else {
                    self.queue.cycle_repeat();
                    self.config.repeat = self.queue.repeat;
                    self.save_config("daemon repeat setting");
                    self.save_session();
                    RemoteResponse::status(self.status())
                }
            }
            RemoteCommand::QueuePlay { position } => {
                let response = self.queue_play(position).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::QueueRemove { position } => {
                let response = self.queue_remove(position).await;
                effects.extend(self.maybe_autoplay_extend());
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
                self.config = Config::default();
                self.save_config("daemon settings reset");
                RemoteResponse::ok("settings reset".to_string())
            }
        };
        self.finish_remote_persistence(response, shutdown, effects)
    }

    /// Route one GUI `apply { group.field = value }` onto the live config. Fields that
    /// already have a [`RemoteSettingChange`] lane reuse it (live player/effect hooks
    /// included); the rest write config directly. Every accepted change is followed by
    /// a `settings_snapshot` push (the publisher diffs post-turn).
    fn apply_gui_setting(
        &mut self,
        change: crate::remote::proto::GuiSettingChange,
    ) -> (RemoteResponse, Vec<EngineEffect>) {
        use crate::remote::proto::RemoteSettingChange as S;
        let crate::remote::proto::GuiSettingChange {
            group,
            field,
            value,
        } = change;

        let as_bool = || value.as_bool();
        let as_u16 = || value.as_u64().and_then(|v| u16::try_from(v).ok());
        let as_str = || value.as_str().map(str::to_string);
        let as_optional_str = || match &value {
            Value::Null => Some(None),
            Value::String(s) => Some((!s.trim().is_empty()).then(|| s.trim().to_string())),
            _ => None,
        };
        let bad = || (RemoteResponse::err("bad_value"), Vec::new());
        let ok = |this: &Self| (RemoteResponse::status(this.status()), Vec::new());

        match (group.as_str(), field.as_str()) {
            ("playback", "speed_tenths") => match as_u16() {
                Some(tenths) => self.set_setting(S::Speed { tenths }),
                None => bad(),
            },
            ("playback", "seek_seconds") => match as_u16() {
                Some(seconds) => self.set_setting(S::SeekSeconds { seconds }),
                None => bad(),
            },
            ("playback", "gapless") => match as_bool() {
                Some(value) => self.set_setting(S::Gapless { value }),
                None => bad(),
            },
            ("playback", "enqueue_next") => match as_bool() {
                Some(v) => {
                    self.config.enqueue_next = Some(v);
                    self.save_config("daemon enqueue-next setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "autoplay_on_start") => match as_bool() {
                Some(v) => {
                    self.config.autoplay_on_start = Some(v);
                    self.save_config("daemon autoplay-on-start setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "mouse_wheel_volume") => match as_bool() {
                Some(v) => {
                    self.config.mouse_wheel_volume = Some(v);
                    self.save_config("daemon wheel-volume setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "media_controls") => match as_bool() {
                Some(v) => {
                    // The OS session itself is created at daemon start; the toggle
                    // takes full effect on the next launch (same as the TUI).
                    self.config.media_controls = Some(v);
                    self.save_config("daemon media-controls setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "volume") => match value.as_i64() {
                Some(v) => (self.set_volume(v), Vec::new()),
                None => bad(),
            },
            ("playback", "shuffle") => match as_bool() {
                Some(v) => {
                    if self.queue.shuffle != v {
                        self.queue.toggle_shuffle();
                        self.config.shuffle = Some(self.queue.shuffle);
                        self.save_config("daemon shuffle setting");
                        self.save_session();
                    }
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "repeat") => {
                match serde_json::from_value::<crate::queue::Repeat>(value.clone()) {
                    // Music-mode invariant: can't enable repeat while autoplay streaming is on.
                    Ok(repeat) if repeat.set_blocked_by_streaming(self.streaming) => ok(self),
                    Ok(repeat) => {
                        self.queue.repeat = repeat;
                        self.config.repeat = repeat;
                        self.save_config("daemon repeat setting");
                        self.save_session();
                        ok(self)
                    }
                    Err(_) => bad(),
                }
            }
            ("audio", "backend") => match as_str().as_deref() {
                Some("mpv") => {
                    self.config.audio.backend = crate::config::AudioBackend::Mpv;
                    self.save_config("daemon audio backend setting");
                    ok(self)
                }
                _ => bad(),
            },
            ("audio", "mpv_output") => match as_optional_str() {
                Some(value) => {
                    self.config.audio.mpv.output = value;
                    self.save_config("daemon mpv output setting");
                    ok(self)
                }
                None => bad(),
            },
            ("audio", "mpv_device") => match as_optional_str() {
                Some(value) => {
                    self.config.audio.mpv.device = value;
                    self.save_config("daemon mpv device setting");
                    ok(self)
                }
                None => bad(),
            },
            ("audio", "mpv_cache_forward") => match as_str() {
                Some(value) => {
                    self.config.audio.mpv.cache_forward = crate::settings::blank_to_none(&value)
                        .unwrap_or_else(|| crate::config::MPV_CACHE_FORWARD_DEFAULT.to_owned());
                    self.save_config("daemon mpv forward-cache setting");
                    ok(self)
                }
                None => bad(),
            },
            ("audio", "mpv_cache_back") => match as_str() {
                Some(value) => {
                    self.config.audio.mpv.cache_back = crate::settings::blank_to_none(&value)
                        .unwrap_or_else(|| crate::config::MPV_CACHE_BACK_DEFAULT.to_owned());
                    self.save_config("daemon mpv back-cache setting");
                    ok(self)
                }
                None => bad(),
            },
            ("eq", "preset") => match as_str()
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
            {
                Some(preset) => {
                    let previous_preset = self.config.eq_preset;
                    let previous_bands = self.config.eq_bands;
                    self.config.eq_preset = preset;
                    self.config.eq_bands = None; // preset gains take over
                    if let Err(error) = self.apply_audio_filter() {
                        self.config.eq_preset = previous_preset;
                        self.config.eq_bands = previous_bands;
                        return (self.reject_player_command(error), Vec::new());
                    }
                    self.save_config("daemon eq preset");
                    ok(self)
                }
                None => bad(),
            },
            ("eq", "bands") => match serde_json::from_value::<[f64; 10]>(value.clone()) {
                Ok(bands) => {
                    let previous_preset = self.config.eq_preset;
                    let previous_bands = self.config.eq_bands;
                    self.config.eq_bands = Some(bands);
                    self.config.eq_preset = crate::eq::EqPreset::Custom;
                    if let Err(error) = self.apply_audio_filter() {
                        self.config.eq_preset = previous_preset;
                        self.config.eq_bands = previous_bands;
                        return (self.reject_player_command(error), Vec::new());
                    }
                    self.save_config("daemon eq bands");
                    ok(self)
                }
                Err(_) => bad(),
            },
            ("eq", "normalize") => match as_bool() {
                Some(value) => self.set_setting(S::Normalize { value }),
                None => bad(),
            },
            ("streaming", "ai_enabled") => match as_bool() {
                Some(value) => self.set_setting(S::AiEnabled { value }),
                None => bad(),
            },
            ("streaming", "autoplay") => match as_bool() {
                Some(value) => self.set_setting(S::AutoplayStreaming { value }),
                None => bad(),
            },
            ("streaming", "mode") => match serde_json::from_value(value.clone()) {
                Ok(value) => self.set_setting(S::StreamingMode { value }),
                Err(_) => bad(),
            },
            ("streaming", "gemini_model") => {
                let parsed = as_str().and_then(|s| {
                    crate::ai::GeminiModel::CYCLE
                        .into_iter()
                        .find(|m| m.api_id() == s)
                        .or_else(|| {
                            serde_json::from_value(serde_json::Value::String(s.clone())).ok()
                        })
                });
                match parsed {
                    Some(model) => {
                        self.config.gemini_model = model;
                        self.save_config("daemon gemini model");
                        ok(self)
                    }
                    None => bad(),
                }
            }
            ("search", "default_source") => match serde_json::from_value(value.clone()) {
                Ok(source) => {
                    self.config.search.source = source;
                    self.save_config("daemon search source");
                    ok(self)
                }
                Err(_) => bad(),
            },
            (
                "search",
                flag @ ("soundcloud_enabled"
                | "audius_enabled"
                | "jamendo_enabled"
                | "internet_archive_enabled"
                | "radio_browser_enabled"),
            ) => match as_bool() {
                Some(v) => {
                    match flag {
                        "soundcloud_enabled" => self.config.search.soundcloud = v,
                        "audius_enabled" => self.config.search.audius = v,
                        "jamendo_enabled" => self.config.search.jamendo = v,
                        "internet_archive_enabled" => self.config.search.internet_archive = v,
                        _ => self.config.search.radio_browser = v,
                    }
                    self.save_config("daemon search catalogs");
                    ok(self)
                }
                None => bad(),
            },
            ("search", "audius_app_name") => match as_optional_str() {
                Some(value) => {
                    self.config.search.audius_app_name = value;
                    self.save_config("daemon audius app name");
                    ok(self)
                }
                None => bad(),
            },
            ("search", "jamendo_client_id") => match as_optional_str() {
                Some(value) => {
                    self.config.search.jamendo_client_id = value;
                    self.save_config("daemon jamendo client id");
                    ok(self)
                }
                None => bad(),
            },
            ("ui", "language") => match as_str().as_deref() {
                Some("en") => {
                    self.config.language = crate::i18n::Language::English;
                    self.save_config("daemon language");
                    ok(self)
                }
                Some("ko") => {
                    self.config.language = crate::i18n::Language::Korean;
                    self.save_config("daemon language");
                    ok(self)
                }
                _ => bad(),
            },
            ("ui", "mouse") => match as_bool() {
                Some(v) => {
                    self.config.mouse = Some(v);
                    self.save_config("daemon mouse setting");
                    ok(self)
                }
                None => bad(),
            },
            ("ui", "album_art") => match as_bool() {
                Some(v) => {
                    self.config.album_art = Some(v);
                    self.save_config("daemon album art setting");
                    ok(self)
                }
                None => bad(),
            },
            ("ui", "romanized_titles") => match as_bool() {
                Some(v) => {
                    self.config.romanized_titles = Some(v);
                    self.save_config("daemon romanized titles setting");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "download_dir") => match as_optional_str() {
                Some(value) => {
                    self.config.download_dir = value.map(std::path::PathBuf::from);
                    self.save_config("daemon download dir");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "cookies_file") => match as_optional_str() {
                Some(value) => {
                    self.config.cookies_file = value.map(std::path::PathBuf::from);
                    self.save_config("daemon cookies file");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "download_concurrency") => match value.as_u64() {
                Some(v @ 1..=16) => {
                    self.config.download_concurrency = Some(v as usize);
                    self.save_config("daemon download concurrency");
                    ok(self)
                }
                _ => bad(),
            },
            ("animations", field) => match self.apply_animation_field(field, &value) {
                true => {
                    self.save_config("daemon animations setting");
                    ok(self)
                }
                false => bad(),
            },
            ("theme", "preset") => match as_str() {
                Some(name) => {
                    // Clone-then-normalize: a fresh ThemeConfig cache (Clone resets it)
                    // so the direct preset write can't leave a stale resolved palette.
                    let mut theme = self.config.theme.clone();
                    theme.preset = name;
                    self.config.theme = theme.normalized();
                    self.save_config("daemon theme preset");
                    ok(self)
                }
                None => bad(),
            },
            ("theme", "retro") => match as_bool() {
                Some(v) => {
                    self.config.retro_mode = v;
                    self.save_config("daemon retro mode");
                    ok(self)
                }
                None => bad(),
            },
            ("theme", role_id) => {
                let role = crate::theme::ThemeRole::ALL
                    .into_iter()
                    .find(|role| role.id() == role_id);
                match (role, as_str()) {
                    (Some(role), Some(hex)) => {
                        let mut theme = self.config.theme.clone();
                        match theme.set_override(role, &hex) {
                            Ok(()) => {
                                self.config.theme = theme;
                                self.save_config("daemon theme override");
                                ok(self)
                            }
                            Err(_) => bad(),
                        }
                    }
                    _ => (RemoteResponse::err("unknown_setting"), Vec::new()),
                }
            }
            _ => (RemoteResponse::err("unknown_setting"), Vec::new()),
        }
    }

    /// Set one [`AnimationsConfig`] field by its wire name; `false` = unknown field or
    /// wrong value type.
    fn apply_animation_field(&mut self, field: &str, value: &serde_json::Value) -> bool {
        let anim = &mut self.config.animations;
        if field == "fps" {
            let Some(fps) = value.as_u64().and_then(|v| u16::try_from(v).ok()) else {
                return false;
            };
            anim.fps = fps.clamp(crate::config::FPS_MIN, crate::config::FPS_MAX);
            return true;
        }
        let Some(v) = value.as_bool() else {
            return false;
        };
        let slot = match field {
            "master" => &mut anim.master,
            "pause_unfocused" => &mut anim.pause_unfocused,
            "title" => &mut anim.title,
            "heart" => &mut anim.heart,
            "seekbar" => &mut anim.seekbar,
            "spinner" => &mut anim.spinner,
            "eq_bars" => &mut anim.eq_bars,
            "controls" => &mut anim.controls,
            "border" => &mut anim.border,
            "track_intro" => &mut anim.track_intro,
            "lyrics" => &mut anim.lyrics,
            "toast" => &mut anim.toast,
            "volume_flash" => &mut anim.volume_flash,
            "like_burst" => &mut anim.like_burst,
            "seek_flash" => &mut anim.seek_flash,
            "selection" => &mut anim.selection,
            "stagger" => &mut anim.stagger,
            "caret" => &mut anim.caret,
            "tabs" => &mut anim.tabs,
            "popup_fade" => &mut anim.popup_fade,
            "activity" => &mut anim.activity,
            "about_fx" => &mut anim.about_fx,
            "visualizer" => &mut anim.visualizer,
            "rain" => &mut anim.rain,
            "donut" => &mut anim.donut,
            "starfield" => &mut anim.starfield,
            "bounce" => &mut anim.bounce,
            _ => return false,
        };
        *slot = v;
        true
    }

    /// Re-send the current audio filter chain (EQ + normalize) to the live player.
    fn apply_audio_filter(&self) -> Result<(), EngineError> {
        let af = self.current_audio_filter();
        self.send_player_command_if_active("set_audio_filter", PlayerCmd::SetAudioFilter(af))
    }

    pub(crate) fn gui_search_is_current(&self, requester: &RequesterKey, ticket: u64) -> bool {
        self.gui_search_index.is_current(requester, ticket)
    }

    pub(crate) fn complete_gui_search(
        &mut self,
        requester: &RequesterKey,
        ticket: u64,
        groups: &[crate::api::GuiSearchGroup],
    ) -> bool {
        self.gui_search_index.complete(requester, ticket, groups)
    }

    #[cfg(test)]
    fn index_gui_search(
        &mut self,
        requester: &RequesterKey,
        groups: &[crate::api::GuiSearchGroup],
    ) {
        assert_eq!(
            self.gui_search_index.begin(
                requester,
                0,
                "test-index",
                crate::search_source::SearchSource::All,
            ),
            GuiSearchAdmission::Start
        );
        assert!(self.gui_search_index.complete(requester, 0, groups));
    }

    /// Resolve a GUI-addressed `video_id` to a playable [`Song`]: the last search's rows
    /// first, then the library (favorites/history), then a bare row mpv resolves at load time
    /// (covers e.g. AI suggestion chips that never went through search). Returns `None` for an
    /// id that is neither known nor a plausible YouTube id, so a bogus/oversized id from a
    /// buggy client or script can't enter the queue as a permanently-unplayable row.
    fn resolve_video_id(&self, requester: Option<&RequesterKey>, video_id: &str) -> Option<Song> {
        if let Some(song) = requester.and_then(|key| self.gui_search_index.resolve(key, video_id)) {
            return Some(song);
        }
        if let Some(song) = self
            .library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .find(|s| s.video_id == video_id)
        {
            return Some(song.clone());
        }
        crate::api::is_youtube_video_id(video_id).then(|| Song::remote(video_id, video_id, "", ""))
    }

    async fn play_tracks(
        &mut self,
        requester: Option<&RequesterKey>,
        video_ids: Vec<String>,
    ) -> RemoteResponse {
        let songs = match self.resolve_video_ids_exact(requester, &video_ids) {
            Ok(songs) => songs,
            Err(reason) => return RemoteResponse::err(reason),
        };
        if !self.queue.has_capacity_for(songs.len()) {
            return RemoteResponse::err("queue_full");
        }
        let previous = self.queue.snapshot();
        let expected = songs.len();
        let added = self.queue.play_now_many(songs);
        debug_assert_eq!(added, expected, "queue capacity was preflighted");
        self.load_current_or_restore_queue(previous)
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn enqueue_tracks(
        &mut self,
        requester: Option<&RequesterKey>,
        video_ids: Vec<String>,
    ) -> RemoteResponse {
        if video_ids.is_empty() {
            return RemoteResponse::err("empty_selection");
        }
        let songs = match self.resolve_video_ids_exact(requester, &video_ids) {
            Ok(songs) => songs,
            Err(reason) => return RemoteResponse::err(reason),
        };
        if !self.queue.has_capacity_for(songs.len()) {
            return RemoteResponse::err("queue_full");
        }
        let previous = self.queue.snapshot();
        let old_len = self.queue.len();
        let was_idle = self.loaded_video_id.is_none();
        let expected = songs.len();
        let added = if self.config.effective_enqueue_next() && !was_idle {
            self.queue.insert_next_many(songs)
        } else {
            self.queue.extend(songs)
        };
        debug_assert_eq!(added, expected, "queue capacity was preflighted");
        if was_idle {
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            return self
                .load_current_or_restore_queue(previous)
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
        }
        self.save_session();
        RemoteResponse::status(self.status())
    }

    fn resolve_video_ids_exact(
        &self,
        requester: Option<&RequesterKey>,
        video_ids: &[String],
    ) -> Result<Vec<Song>, &'static str> {
        if video_ids.is_empty() {
            return Err("empty_selection");
        }
        if video_ids.len() > REMOTE_MAX_TRACK_IDS {
            return Err("too_many_tracks");
        }
        video_ids
            .iter()
            .map(|id| self.resolve_video_id(requester, id).ok_or("stale_results"))
            .collect()
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
            // Intercepted by the host loop (index + `search` topic push) before it
            // reaches the engine; defensive no-op if it ever lands here.
            ApiEvent::GuiSearchCompleted { .. } => Vec::new(),
            ApiEvent::StreamingResults {
                seed_video_id,
                candidates,
            } => {
                self.streaming_pending = false;
                if self.streaming && self.queue.contains_video_id(&seed_video_id) {
                    let picks = self.plan_local_streaming(&seed_video_id, candidates);
                    self.extend_sanitized_streaming(&seed_video_id, picks, &[])
                        .await
                } else {
                    Vec::new()
                }
            }
            ApiEvent::StreamingPreflighted {
                seed_video_id,
                songs,
            } => {
                self.streaming_pending = false;
                if self.streaming && self.queue.contains_video_id(&seed_video_id) {
                    self.extend_queue_from_streaming(songs).await
                } else {
                    Vec::new()
                }
            }
            ApiEvent::StreamingError {
                seed_video_id,
                error,
            } => {
                self.streaming_pending = false;
                if self.streaming && self.queue.contains_video_id(&seed_video_id) {
                    self.note_streaming_failure(format!("autoplay streaming failed: {error}"));
                }
                Vec::new()
            }
            // Playlist search/import is a TUI-only flow; the daemon never issues those
            // commands, so their answers are inert here.
            ApiEvent::ModeResolved { .. }
            | ApiEvent::SearchResults { .. }
            | ApiEvent::SearchError { .. }
            | ApiEvent::PlaylistTracks { .. }
            | ApiEvent::PlaylistTracksError { .. } => Vec::new(),
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
        StatusSnapshot {
            title: current.map(|song| crate::api::sanitize_title(&song.title)),
            artist: current.map(|song| crate::api::sanitize_artist(&song.artist)),
            paused: current.is_none() || self.playback.paused,
            volume: self.playback.volume,
            position,
            total,
            streaming: self.streaming,
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
        }
    }

    /// Whether the OS media session should be live (the same config toggle the TUI uses).
    pub fn media_controls_enabled(&self) -> bool {
        self.config.effective_media_controls()
    }

    pub fn scrobble_settings(&self) -> crate::scrobble::ScrobbleSettings {
        self.config.scrobble_settings()
    }

    pub fn set_media_art(&mut self, ready: crate::media::artwork::MediaArtworkReady) {
        self.media_art = Some(ready);
    }

    /// Apply one OS media-session command. Returns `(shutdown, effects)`; commands the
    /// current state can't honor are ignored quietly (their buttons were reported
    /// disabled). Mirrors [`crate::app`]'s `apply_media` for the headless engine.
    pub async fn handle_media(
        &mut self,
        cmd: crate::media::MediaCommand,
    ) -> (bool, Vec<EngineEffect>) {
        use crate::media::MediaCommand;
        tracing::debug!(?cmd, "daemon media command");
        let mut effects = Vec::new();
        match cmd {
            MediaCommand::Play => {
                if self.queue.current().is_some() && (self.playback.paused || self.needs_load()) {
                    let _ = self.toggle_pause().await;
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Pause => {
                if !self.playback.paused && !self.needs_load() {
                    let _ = self.toggle_pause().await;
                }
            }
            MediaCommand::Toggle => {
                if self.queue.current().is_some() {
                    let _ = self.toggle_pause().await;
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Stop => {
                if self.queue.current().is_some() || self.loaded_video_id.is_some() {
                    self.stop_playback();
                    self.save_session();
                }
            }
            MediaCommand::Next => {
                if self.queue.peek_next().is_some() {
                    let outgoing = self.prepare_outgoing(false);
                    let response = self.next_track().await;
                    if response.ok
                        && let Some(outgoing) = outgoing
                    {
                        self.commit_outgoing(outgoing);
                    }
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Previous => {
                if self.queue.current().is_some() {
                    let _ = self.prev_track().await;
                }
            }
            MediaCommand::SeekBy(seconds) => {
                if self.media_can_seek() && seconds.is_finite() {
                    let _ = self.seek(seconds);
                }
            }
            MediaCommand::SeekTo(pos) => {
                // `pos.is_finite()` rejects NaN/±inf (a NaN also fails `>= 0.0`, but inf would
                // slip past it); mirrors the App reducer's non-finite guard for parity.
                if self.media_can_seek() && pos.is_finite() && pos >= 0.0 {
                    // Out-of-range SetPosition is ignored per the MPRIS spec.
                    if let Some(d) = self.playback.duration
                        && pos > d + 0.5
                    {
                        return (false, effects);
                    }
                    let _ = self.seek_to(pos);
                }
            }
            MediaCommand::SetShuffle(on) => {
                if !self.current_is_radio_stream() && self.queue.shuffle != on {
                    self.queue.set_shuffle(on);
                    self.config.shuffle = Some(on);
                    self.save_config("daemon shuffle setting");
                    self.save_session();
                }
            }
            MediaCommand::SetRepeat(mode) => {
                // Live-radio parity with the TUI: these UI slots are reinterpreted as live-sync
                // controls, so OS widgets must not mutate shuffle/repeat while a station plays.
                // Music-mode invariant: an OS widget can't enable repeat while streaming is on.
                if !self.current_is_radio_stream()
                    && self.queue.repeat != mode
                    && !mode.set_blocked_by_streaming(self.streaming)
                {
                    self.queue.repeat = mode;
                    self.config.repeat = mode;
                    self.save_config("daemon repeat setting");
                    self.save_session();
                }
            }
            MediaCommand::SetVolume(v) => {
                // Shared 0..1→percent map with the TUI; a non-finite write is ignored.
                if let Some(volume) = crate::playback_policy::volume_percent_from_unit(v)
                    && volume != self.playback.volume
                {
                    let _ = self.adjust_volume(volume - self.playback.volume);
                }
            }
            MediaCommand::SetRate(rate) => {
                if rate == 0.0 {
                    return Box::pin(self.handle_media(MediaCommand::Pause)).await;
                }
                let speed = clamp_speed(rate);
                if (speed - self.playback.speed).abs() > f64::EPSILON {
                    let delivery = self.send_player_command_if_active(
                        "set_speed",
                        PlayerCmd::SetProperty {
                            name: "speed".to_owned(),
                            value: Value::from(speed),
                        },
                    );
                    if let Err(error) = delivery {
                        self.last_error = Some(error.to_string());
                    } else {
                        self.playback.speed = speed;
                    }
                }
            }
            MediaCommand::Like => self.media_set_rating(true),
            MediaCommand::Dislike => self.media_set_rating(false),
            MediaCommand::OpenUri(uri) => {
                if let Some(id) = crate::media::parse_youtube_video_id(&uri) {
                    let song = self
                        .library
                        .favorites
                        .iter()
                        .chain(self.library.history.iter())
                        .find(|s| s.youtube_id() == Some(id.as_str()))
                        .cloned()
                        .unwrap_or_else(|| {
                            Song::remote(id.clone(), format!("YouTube {id}"), "", "")
                        });
                    let previous = self.queue.snapshot();
                    if self.queue.play_now(song) {
                        if let Err(e) = self.load_current_or_restore_queue(previous).await {
                            self.last_error = Some(e.to_string());
                            self.stop_playback();
                        }
                        effects.extend(self.maybe_autoplay_extend());
                    }
                }
            }
            MediaCommand::Quit => {
                self.stop_playback();
                self.suppress_transport_recovery_for_shutdown();
                self.save_session();
                return (true, effects);
            }
        }
        (false, effects)
    }

    fn needs_load(&self) -> bool {
        self.loaded_video_id.as_deref() != self.queue.current().map(|song| song.video_id.as_str())
    }

    fn media_can_seek(&self) -> bool {
        self.loaded_video_id.is_some()
            && self
                .queue
                .current()
                .is_some_and(|song| !song.is_radio_station())
    }

    fn current_is_radio_stream(&self) -> bool {
        self.queue
            .current()
            .is_some_and(|song| song.is_radio_station())
    }

    /// Like/dislike from the OS surface: same favorite/dislike bookkeeping the TUI's
    /// rating cycle performs, persisted immediately (the daemon has no Cmd loop).
    fn media_set_rating(&mut self, like: bool) {
        let Some(song) = self.queue.current().cloned() else {
            return;
        };
        if song.is_radio_station() {
            if like {
                self.library.toggle_favorite(&song);
                self.save_library("daemon radio favorite");
            }
            return;
        }
        let artist_key = signals::normalize_artist(&song.artist);
        let now = signals::unix_now();
        let liked = self.library.is_favorite(&song.video_id);
        let disliked = self.signals.is_disliked(&song.video_id);
        if like {
            if liked {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
            } else {
                if disliked {
                    self.signals
                        .toggle_dislike(&song.video_id, &artist_key, now);
                }
                let now_fav = self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, now_fav, now);
            }
        } else if disliked {
            self.signals
                .toggle_dislike(&song.video_id, &artist_key, now);
        } else {
            if liked {
                self.library.toggle_favorite(&song);
                self.signals
                    .record_like(&song.video_id, &artist_key, false, now);
            }
            self.signals
                .toggle_dislike(&song.video_id, &artist_key, now);
        }
        self.save_library("daemon media rating library");
        self.save_signals("daemon media rating signals");
    }

    /// The v8 publisher's read view of this owner (the daemon analog of
    /// `App::core_view`; docs/gui/02 §14). Interpolates elapsed to "now" from the same
    /// anchor the OS media session uses. EQ reflects config (the daemon's live EQ apply
    /// lands at S4/B3); the daemon has no ICY now-playing surface yet.
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
            streaming: self.streaming,
            radio_mode: self.last_mode == LastMode::Radio,
            stream_now_playing: None,
            owner_mode: crate::remote::proto::InstanceMode::Daemon,
            eq_preset: self.config.eq_preset.label().to_string(),
            eq_bands: self.config.effective_eq_bands(),
            eq_normalize: self.config.effective_normalize(),
            config: &self.config,
            // Same current-track gate as status()/media_snapshot: stale art from the
            // previous track never rides a push.
            artwork: cur.and_then(|song| {
                self.media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| ArtworkRef {
                        key: art.key.clone(),
                        path: Some(art.path.to_string_lossy().into_owned()),
                        mime: None,
                    })
            }),
        }
    }

    /// Build the OS media-session snapshot from engine state (the daemon analog of
    /// the TUI's `App::media_snapshot`).
    pub fn media_snapshot(&self) -> crate::media::MediaSnapshot {
        use crate::media::{MediaCaps, MediaPlaybackStatus, MediaSnapshot, MediaTrack};
        let current = self.queue.current();
        let track = current.map(|song| {
            let is_live = song.is_radio_station();
            let duration = if is_live {
                None
            } else {
                self.playback.duration.filter(|d| *d > 0.0).or_else(|| {
                    crate::streaming::candidate::parse_duration_secs(&song.duration).map(f64::from)
                })
            };
            let youtube_id = song.youtube_id().map(str::to_owned);
            let art_query = match (&song.local_path, &youtube_id) {
                (Some(path), _) => Some(crate::media::artwork::ArtQuery::LocalFile(path.clone())),
                (None, Some(id)) if !is_live => {
                    Some(crate::media::artwork::ArtQuery::Youtube { id: id.clone() })
                }
                _ => None,
            };
            MediaTrack {
                key: song.video_id.clone(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                album: if is_live { None } else { song.album.clone() },
                duration,
                is_live,
                url: youtube_id
                    .as_deref()
                    .map(|id| format!("https://music.youtube.com/watch?v={id}")),
                art_remote_url: youtube_id
                    .as_deref()
                    .filter(|_| !is_live)
                    .map(crate::media::artwork::remote_thumbnail_url),
                art_file: self
                    .media_art
                    .as_ref()
                    .filter(|art| art.key == song.video_id)
                    .map(|art| art.path.clone()),
                art_query,
                liked: self.library.is_favorite(&song.video_id),
                disliked: self.signals.is_disliked(&song.video_id),
            }
        });
        let status = if track.is_none() {
            MediaPlaybackStatus::Stopped
        } else if self.playback.paused || self.loaded_video_id.is_none() {
            MediaPlaybackStatus::Paused
        } else {
            MediaPlaybackStatus::Playing
        };
        let caps = MediaCaps {
            can_next: self.queue.peek_next().is_some(),
            can_previous: track.is_some(),
            can_play: track.is_some(),
            can_pause: track.is_some(),
            can_seek: self.media_can_seek() && track.as_ref().is_some_and(|t| t.duration.is_some()),
        };
        MediaSnapshot {
            track,
            status,
            position: self.playback.time_pos.unwrap_or(0.0),
            captured_at: self.playback.time_pos_at.unwrap_or_else(Instant::now),
            rate: self.playback.speed,
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
            volume: (self.playback.volume as f64 / 100.0).clamp(0.0, 1.0),
            caps,
            position_epoch: self.playback.position_epoch,
        }
    }

    fn restore_session_cache(&mut self, cache: SessionCache) {
        self.last_mode = cache.last_mode;
        self.inactive_normal_queue = cache.normal_queue.clone();
        self.inactive_radio_queue = cache.radio_queue.clone();
        self.inactive_local_queue = cache.local_queue.clone();

        if let Some(snapshot) = cache.active_queue().cloned() {
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
        self.load_current_or_restore_queue(previous)
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
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
            return self
                .load_current_or_restore_queue(previous)
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
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
            self.send_active_player_command("seek_absolute", PlayerCmd::SeekAbsolute(target))
        {
            return self.reject_player_command(error);
        }
        self.note_seek(target);
        RemoteResponse::status(self.status())
    }

    /// Record a position discontinuity at `pos` (seek applied / track restarted).
    fn note_seek(&mut self, pos: f64) {
        self.playback.time_pos = Some(pos);
        self.playback.time_pos_at = Some(Instant::now());
        self.bump_position_epoch(PositionEpochReason::Seek);
    }

    fn set_streaming(&mut self, state: ToggleState) -> (RemoteResponse, Vec<EngineEffect>) {
        let mut on = state.resolve(self.streaming);
        // Music-mode invariant (mirrors the App reducer for parity): never enable autoplay while
        // repeat is on. Clamping to `false` keeps the response identical to a normal "off".
        if on && self.queue.repeat.is_on() {
            on = false;
        }
        self.streaming = on;
        self.config.autoplay_streaming = Some(self.streaming);
        if self.streaming {
            self.consecutive_streaming_failures = 0;
        } else {
            self.streaming_pending = false;
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
                self.config.search.streaming_source = search.normalized_streaming_source(value);
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
            self.reset_idle_playback();
            self.loaded_video_id = None;
        }
        effects.extend(self.maybe_autoplay_extend());
        effects
    }

    async fn handle_playback_error(&mut self, error: String) -> Vec<EngineEffect> {
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
        if let Some(player) = self.player.take() {
            record_player_delivery("stop", player.handle.send(PlayerCmd::Stop));
        }
        self.reset_idle_playback();
        self.loaded_video_id = None;
        self.transport_recovery = None;
        self.transport_auto_recovery_armed = true;
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
        let (handle, guard) = player::spawn(
            move |event| (emit)(event),
            data_dir(),
            self.config
                .cookies_file_for_external_tools(data_dir().as_deref()),
            self.config.effective_gapless(),
            self.config.audio.runtime(),
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

    fn current_audio_filter(&self) -> String {
        eq::build_af_string(
            &self.config.effective_eq_bands(),
            self.config.effective_normalize(),
        )
        .unwrap_or_default()
    }

    fn record_session_event(&mut self, artist_key: &str, outcome: DaemonOutcome, completion: f32) {
        self.session_events.push_back(DaemonSessionEvent {
            artist_key: artist_key.to_owned(),
            outcome,
            completion,
        });
        while self.session_events.len() > SESSION_EVENTS_CAP {
            self.session_events.pop_front();
        }
    }

    fn session_cache_snapshot(&self) -> SessionCache {
        let mut cache = SessionCache::from_last_mode(self.last_mode);
        match self.last_mode {
            LastMode::Normal => {
                cache.normal_queue = Some(self.queue.snapshot());
                cache.radio_queue = self.inactive_radio_queue.clone();
                cache.local_queue = self.inactive_local_queue.clone();
            }
            LastMode::Radio => {
                cache.radio_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.clone();
                cache.local_queue = self.inactive_local_queue.clone();
            }
            LastMode::Local => {
                cache.local_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.clone();
                cache.radio_queue = self.inactive_radio_queue.clone();
            }
        }
        cache
    }
}

fn data_dir() -> Option<PathBuf> {
    crate::paths::data_dir()
}

#[cfg(test)]
mod delivery_tests;
#[cfg(test)]
mod gui_search_tests;
#[cfg(test)]
mod persistence_gate_tests;
#[cfg(test)]
mod transport_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use serde_json::json;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("title-{id}"), "artist".to_owned(), "3:00")
    }

    fn radio_station(id: &str) -> Song {
        let mut song = Song::remote(id, format!("station-{id}"), "", "");
        song.playable = Some(crate::api::PlayableRef::RadioStream {
            url: format!("https://radio.example/{id}.mp3"),
        });
        song
    }

    pub(super) fn engine_with_queue(ids: &[&str]) -> DaemonEngine {
        let mut queue = Queue::default();
        queue.set(ids.iter().map(|id| song(id)).collect(), 0);
        DaemonEngine {
            maintainer: crate::util::background_task::BackgroundTask::disabled("yt-dlp maintainer"),
            player: None,
            player_emit: Arc::new(|_| {}),
            queue,
            playback: DaemonPlayback {
                paused: true,
                volume: 50,
                time_pos: None,
                time_pos_at: None,
                position_epoch: 0,
                duration: None,
                speed: 1.0,
            },
            config: Config::default(),
            library: Library::default(),
            signals: Signals::default(),
            station: StationStore::default(),
            loaded_video_id: None,
            transport_recovery: None,
            transport_recovery_generation: 0,
            transport_auto_recovery_armed: true,
            test_player_starts: VecDeque::new(),
            streaming: false,
            streaming_pending: false,
            last_extend: None,
            consecutive_streaming_failures: 0,
            last_error: None,
            remote_persistence_write_failed: false,
            remote_persistence_error: None,
            remote_persistence_command_active: false,
            remote_persistence_read_only: false,
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
            gui_search_index: GuiSearchIndex::default(),
        }
    }

    #[tokio::test]
    async fn dropping_engine_aborts_maintainer_instead_of_detaching() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let mut engine = engine_with_queue(&[]);
        engine.maintainer = crate::util::background_task::BackgroundTask::spawn(
            "test daemon maintainer",
            async move {
                struct MarkDrop(Option<tokio::sync::oneshot::Sender<()>>);
                impl Drop for MarkDrop {
                    fn drop(&mut self) {
                        if let Some(tx) = self.0.take() {
                            let _ = tx.send(());
                        }
                    }
                }
                let _mark = MarkDrop(Some(dropped_tx));
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            },
        );
        started_rx.await.unwrap();

        drop(engine);

        tokio::time::timeout(Duration::from_millis(100), dropped_rx)
            .await
            .expect("engine drop must cancel maintainer")
            .unwrap();
    }

    pub(super) fn install_accepting_player(
        engine: &mut DaemonEngine,
    ) -> tokio::sync::mpsc::Receiver<PlayerCmd> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        engine.player = Some(PlayerRuntime {
            handle: PlayerHandle::test_handle(tx),
            _guard: None,
        });
        rx
    }

    fn gui_change(
        group: &str,
        field: &str,
        value: serde_json::Value,
    ) -> crate::remote::proto::GuiSettingChange {
        crate::remote::proto::GuiSettingChange {
            group: group.to_owned(),
            field: field.to_owned(),
            value,
        }
    }

    fn apply_gui_ok(
        engine: &mut DaemonEngine,
        group: &str,
        field: &str,
        value: serde_json::Value,
    ) -> Vec<EngineEffect> {
        let (response, effects) = engine.apply_gui_setting(gui_change(group, field, value));
        assert!(response.ok, "{group}.{field} should be accepted");
        effects
    }

    #[test]
    fn status_artwork_only_matches_current_track() {
        let mut engine = engine_with_queue(&["seed"]);
        // Art for a *different* track is not surfaced (mirrors the media snapshot gate).
        engine.set_media_art(crate::media::artwork::MediaArtworkReady {
            key: "other".to_owned(),
            path: std::path::PathBuf::from("/tmp/other.jpg"),
        });
        assert!(engine.status().artwork.is_none());

        engine.set_media_art(crate::media::artwork::MediaArtworkReady {
            key: "seed".to_owned(),
            path: std::path::PathBuf::from("/tmp/seed.jpg"),
        });
        let art = engine.status().artwork.expect("artwork");
        assert_eq!(art.key, "seed");
        assert_eq!(art.path.as_deref(), Some("/tmp/seed.jpg"));
    }

    #[test]
    fn gui_apply_routes_settings_to_live_daemon_state() {
        let mut engine = engine_with_queue(&["seed"]);

        apply_gui_ok(&mut engine, "playback", "speed_tenths", json!(25));
        apply_gui_ok(&mut engine, "playback", "seek_seconds", json!(99));
        apply_gui_ok(&mut engine, "playback", "gapless", json!(true));
        apply_gui_ok(&mut engine, "playback", "enqueue_next", json!(true));
        apply_gui_ok(&mut engine, "playback", "autoplay_on_start", json!(true));
        apply_gui_ok(&mut engine, "playback", "mouse_wheel_volume", json!(true));
        apply_gui_ok(&mut engine, "playback", "media_controls", json!(false));
        apply_gui_ok(&mut engine, "playback", "volume", json!(123));
        apply_gui_ok(&mut engine, "playback", "shuffle", json!(true));
        apply_gui_ok(
            &mut engine,
            "playback",
            "repeat",
            serde_json::to_value(crate::queue::Repeat::Off).unwrap(),
        );

        assert_eq!(engine.playback.speed, crate::config::SPEED_MAX);
        assert_eq!(
            engine.config.seek_seconds,
            Some(crate::config::SEEK_SECONDS_MAX)
        );
        assert_eq!(engine.config.gapless, Some(true));
        assert_eq!(engine.config.enqueue_next, Some(true));
        assert_eq!(engine.config.autoplay_on_start, Some(true));
        assert_eq!(engine.config.mouse_wheel_volume, Some(true));
        assert_eq!(engine.config.media_controls, Some(false));
        assert!(!super::super::daemon_media_enabled(&engine, true));
        apply_gui_ok(&mut engine, "playback", "media_controls", json!(true));
        assert!(super::super::daemon_media_enabled(&engine, true));
        assert!(!super::super::daemon_media_enabled(&engine, false));
        assert_eq!(engine.playback.volume, VOLUME_MAX);
        assert!(engine.queue.shuffle);

        apply_gui_ok(&mut engine, "eq", "preset", json!("rock"));
        apply_gui_ok(
            &mut engine,
            "eq",
            "bands",
            json!([0.0, 1.0, 2.0, 3.0, 4.0, 5.0, -1.0, -2.0, -3.0, -4.0]),
        );
        apply_gui_ok(&mut engine, "eq", "normalize", json!(true));
        assert_eq!(engine.config.eq_preset, crate::eq::EqPreset::Custom);
        assert_eq!(engine.config.eq_bands.unwrap()[5], 5.0);
        assert!(engine.current_audio_filter().contains("dynaudnorm"));

        let effects = apply_gui_ok(&mut engine, "streaming", "autoplay", json!(true));
        assert!(engine.streaming);
        assert!(matches!(
            effects.as_slice(),
            [EngineEffect::StreamingFallback { seed_video_id, .. }] if seed_video_id == "seed"
        ));
        apply_gui_ok(
            &mut engine,
            "streaming",
            "mode",
            serde_json::to_value(crate::streaming::StreamingMode::Discovery).unwrap(),
        );
        apply_gui_ok(
            &mut engine,
            "streaming",
            "gemini_model",
            json!("gemini-2.5-flash"),
        );
        apply_gui_ok(&mut engine, "streaming", "ai_enabled", json!(false));
        assert_eq!(
            engine.config.streaming.mode,
            crate::streaming::StreamingMode::Discovery
        );
        assert_eq!(engine.config.ai_enabled, Some(false));

        apply_gui_ok(
            &mut engine,
            "search",
            "default_source",
            serde_json::to_value(crate::search_source::SearchSource::All).unwrap(),
        );
        apply_gui_ok(&mut engine, "search", "soundcloud_enabled", json!(false));
        apply_gui_ok(&mut engine, "search", "audius_enabled", json!(false));
        apply_gui_ok(&mut engine, "search", "jamendo_enabled", json!(false));
        apply_gui_ok(
            &mut engine,
            "search",
            "internet_archive_enabled",
            json!(false),
        );
        apply_gui_ok(&mut engine, "search", "radio_browser_enabled", json!(false));
        apply_gui_ok(
            &mut engine,
            "search",
            "audius_app_name",
            json!("  daemon app  "),
        );
        apply_gui_ok(
            &mut engine,
            "search",
            "jamendo_client_id",
            serde_json::Value::Null,
        );
        assert_eq!(
            engine.config.search.audius_app_name.as_deref(),
            Some("daemon app")
        );
        assert_eq!(engine.config.search.jamendo_client_id, None);

        apply_gui_ok(&mut engine, "ui", "language", json!("ko"));
        apply_gui_ok(&mut engine, "ui", "mouse", json!(true));
        apply_gui_ok(&mut engine, "ui", "album_art", json!(true));
        apply_gui_ok(&mut engine, "ui", "romanized_titles", json!(true));
        assert_eq!(engine.config.language, crate::i18n::Language::Korean);
        assert_eq!(engine.config.mouse, Some(true));
        assert_eq!(engine.config.album_art, Some(true));
        assert_eq!(engine.config.romanized_titles, Some(true));

        apply_gui_ok(
            &mut engine,
            "storage",
            "download_dir",
            json!("/tmp/ytm-downloads"),
        );
        apply_gui_ok(
            &mut engine,
            "storage",
            "cookies_file",
            serde_json::Value::Null,
        );
        apply_gui_ok(&mut engine, "storage", "download_concurrency", json!(16));
        assert_eq!(
            engine.config.download_dir.as_deref(),
            Some(std::path::Path::new("/tmp/ytm-downloads"))
        );
        assert_eq!(engine.config.cookies_file, None);
        assert_eq!(engine.config.download_concurrency, Some(16));

        apply_gui_ok(&mut engine, "animations", "fps", json!(999));
        apply_gui_ok(&mut engine, "animations", "master", json!(true));
        apply_gui_ok(&mut engine, "animations", "bounce", json!(true));
        assert_eq!(engine.config.animations.fps, crate::config::FPS_MAX);
        assert!(engine.config.animations.master);
        assert!(engine.config.animations.bounce);

        apply_gui_ok(&mut engine, "theme", "preset", json!("light"));
        apply_gui_ok(&mut engine, "theme", "retro", json!(true));
        apply_gui_ok(&mut engine, "theme", "accent", json!("#112233"));
        assert_eq!(engine.config.theme.preset, "light");
        assert!(engine.config.retro_mode);
        assert_eq!(
            engine
                .config
                .theme
                .effective_hex(crate::theme::ThemeRole::Accent),
            "#112233"
        );
    }

    #[test]
    fn gui_apply_rejects_bad_values_and_unknown_fields() {
        let mut engine = engine_with_queue(&["seed"]);

        for (group, field, value, reason) in [
            ("playback", "speed_tenths", json!("fast"), "bad_value"),
            ("eq", "preset", json!("not-a-preset"), "bad_value"),
            ("streaming", "mode", json!("invalid"), "bad_value"),
            ("search", "audius_app_name", json!(42), "bad_value"),
            ("ui", "language", json!("fr"), "bad_value"),
            ("storage", "download_concurrency", json!(0), "bad_value"),
            ("animations", "nope", json!(true), "bad_value"),
            ("theme", "accent", json!("not-hex"), "bad_value"),
            ("theme", "not_a_role", json!("#ffffff"), "unknown_setting"),
            ("nope", "field", json!(true), "unknown_setting"),
        ] {
            let (response, effects) = engine.apply_gui_setting(gui_change(group, field, value));
            assert!(!response.ok, "{group}.{field} should be rejected");
            assert_eq!(response.reason.as_deref(), Some(reason));
            assert!(effects.is_empty());
        }
    }

    #[test]
    fn gui_search_index_resolution_prefers_visible_rows_then_library_then_safe_fallback() {
        let mut engine = engine_with_queue(&[]);
        let requester = RequesterKey::new(1, Some("page-a".to_owned()));
        let searched = Song::from_source(
            crate::search_source::SearchSource::Jamendo,
            "jam-1",
            "Jam title",
            "Jam artist",
            "2:00",
            crate::api::PlayableRef::DirectUrl {
                source: crate::search_source::SearchSource::Jamendo,
                url: "https://cdn.example/audio.mp3".to_owned(),
            },
        );
        engine.index_gui_search(
            &requester,
            &[crate::api::GuiSearchGroup {
                source: crate::search_source::SearchSource::Jamendo,
                songs: vec![searched.clone()],
                error: None,
            }],
        );
        let searched_row_id = crate::api::gui_search_row_id(&searched);
        assert_eq!(
            engine
                .resolve_video_id(Some(&requester), &searched_row_id)
                .unwrap()
                .watch_url(),
            "https://cdn.example/audio.mp3"
        );

        engine.library.favorites.push(song("dQw4w9WgXcQ"));
        assert_eq!(
            engine
                .resolve_video_id(Some(&requester), "dQw4w9WgXcQ")
                .unwrap()
                .title,
            "title-dQw4w9WgXcQ"
        );
        let fallback = engine
            .resolve_video_id(Some(&requester), "TAfHyXrULiM")
            .unwrap();
        assert_eq!(fallback.title, "TAfHyXrULiM");
        assert!(
            engine
                .resolve_video_id(Some(&requester), "bad/not/video")
                .is_none()
        );
    }

    #[tokio::test]
    async fn player_events_normalize_transport_state_without_player_runtime() {
        let mut engine = engine_with_queue(&["seed"]);

        assert!(
            engine
                .handle_player_event(PlayerEvent::TimePos(f64::NAN))
                .await
                .is_empty()
        );
        assert_eq!(engine.playback.time_pos, Some(0.0));
        engine
            .handle_player_event(PlayerEvent::Duration(Some(f64::INFINITY)))
            .await;
        assert_eq!(engine.playback.duration, Some(0.0));
        engine.handle_player_event(PlayerEvent::Paused(false)).await;
        assert!(!engine.playback.paused);
        assert!(engine.playback.time_pos_at.is_some());
        engine
            .handle_player_event(PlayerEvent::Volume(f64::INFINITY))
            .await;
        assert_eq!(engine.playback.volume, 50);
        engine.handle_player_event(PlayerEvent::Volume(12.4)).await;
        assert_eq!(engine.playback.volume, 12);
        engine
            .handle_player_event(PlayerEvent::Metadata(serde_json::Value::Null))
            .await;
        engine
            .handle_player_event(PlayerEvent::CacheTime(None))
            .await;
        engine
            .handle_player_event(PlayerEvent::AudioCodec(Some("aac".to_owned())))
            .await;
        engine
            .handle_player_event(PlayerEvent::FileFormat(Some("mp4".to_owned())))
            .await;
    }

    #[tokio::test]
    async fn media_commands_and_snapshot_mutate_only_supported_headless_state() {
        let mut engine = engine_with_queue(&["seed", "next"]);
        let _player_rx = install_accepting_player(&mut engine);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.playback.paused = false;
        engine.playback.time_pos = Some(10.0);
        engine.playback.time_pos_at = Some(Instant::now());
        engine.playback.duration = Some(100.0);
        engine.set_media_art(crate::media::artwork::MediaArtworkReady {
            key: "seed".to_owned(),
            path: std::path::PathBuf::from("/tmp/seed.jpg"),
        });
        engine.library.toggle_favorite(&song("seed"));

        let snapshot = engine.media_snapshot();
        assert_eq!(snapshot.status, crate::media::MediaPlaybackStatus::Playing);
        assert!(snapshot.caps.can_next);
        assert!(snapshot.caps.can_seek);
        let track = snapshot.track.unwrap();
        assert_eq!(track.key, "seed");
        assert_eq!(track.duration, Some(100.0));
        assert!(track.liked);
        assert_eq!(
            track.art_file.as_deref(),
            Some(std::path::Path::new("/tmp/seed.jpg"))
        );

        let (_, effects) = engine
            .handle_media(crate::media::MediaCommand::SeekBy(5.0))
            .await;
        assert!(effects.is_empty());
        assert_eq!(engine.playback.time_pos, Some(15.0));
        let epoch_after_seek = engine.playback.position_epoch;

        let (_, effects) = engine
            .handle_media(crate::media::MediaCommand::SeekTo(150.0))
            .await;
        assert!(effects.is_empty());
        assert_eq!(engine.playback.position_epoch, epoch_after_seek);
        assert_eq!(engine.playback.time_pos, Some(15.0));

        let (_, effects) = engine
            .handle_media(crate::media::MediaCommand::SetVolume(0.37))
            .await;
        assert!(effects.is_empty());
        assert_eq!(engine.playback.volume, 37);

        let (_, effects) = engine
            .handle_media(crate::media::MediaCommand::SetRate(1.75))
            .await;
        assert!(effects.is_empty());
        assert_eq!(engine.playback.speed, 1.8);

        let (_, effects) = engine
            .handle_media(crate::media::MediaCommand::SetShuffle(true))
            .await;
        assert!(effects.is_empty());
        assert!(engine.queue.shuffle);

        let (_, effects) = engine
            .handle_media(crate::media::MediaCommand::SetRepeat(
                crate::queue::Repeat::All,
            ))
            .await;
        assert!(effects.is_empty());
        assert_eq!(engine.queue.repeat, crate::queue::Repeat::All);

        let (shutdown, effects) = engine.handle_media(crate::media::MediaCommand::Stop).await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert!(engine.loaded_video_id.is_none());
        assert_eq!(
            engine.media_snapshot().status,
            crate::media::MediaPlaybackStatus::Paused
        );
    }

    #[test]
    fn status_core_view_and_media_snapshot_share_current_track_projection() {
        let mut engine = engine_with_queue(&["seed", "next"]);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.playback.paused = false;
        engine.playback.volume = 73;
        engine.playback.time_pos = Some(4.0);
        engine.playback.time_pos_at = Some(Instant::now() - Duration::from_millis(5));
        engine.playback.duration = Some(123.0);
        engine.playback.speed = 1.5;
        for _ in 0..7 {
            engine.bump_position_epoch(PositionEpochReason::Seek);
        }
        engine.streaming = true;
        engine.queue.set_shuffle(true);
        engine.queue.repeat = crate::queue::Repeat::All;
        engine.set_media_art(crate::media::artwork::MediaArtworkReady {
            key: "seed".to_owned(),
            path: std::path::PathBuf::from("/tmp/daemon-seed.jpg"),
        });
        engine.library.toggle_favorite(&song("seed"));
        engine.signals.toggle_dislike(
            "next",
            &signals::normalize_artist("artist"),
            signals::unix_now(),
        );

        let status = engine.status();
        assert_eq!(status.title.as_deref(), Some("title-seed"));
        assert_eq!(status.artist.as_deref(), Some("artist"));
        assert!(!status.paused);
        assert_eq!(status.volume, 73);
        assert_eq!(status.position, 1);
        assert_eq!(status.total, 2);
        assert!(status.streaming);
        assert!(status.shuffle);
        assert_eq!(status.repeat, crate::queue::Repeat::All);
        assert_eq!(status.duration_ms, Some(123_000));
        assert!(status.elapsed_ms.unwrap() >= 4_000);
        assert_eq!(
            status.artwork.as_ref().map(|art| art.key.as_str()),
            Some("seed")
        );
        assert_eq!(status.queue.len(), 2);
        assert!(status.queue[0].current);

        let core = engine.core_view();
        assert_eq!(core.volume, 73);
        assert_eq!(core.speed_tenths, 15);
        assert_eq!(core.duration_ms, Some(123_000));
        assert_eq!(core.position_epoch, 7);
        assert!(core.streaming);
        assert_eq!(core.owner_mode, InstanceMode::Daemon);
        assert_eq!(
            core.artwork.as_ref().map(|art| art.key.as_str()),
            Some("seed")
        );

        let media = engine.media_snapshot();
        assert_eq!(media.status, crate::media::MediaPlaybackStatus::Playing);
        assert!(media.shuffle);
        assert_eq!(media.repeat, crate::queue::Repeat::All);
        assert!((media.volume - 0.73).abs() < f64::EPSILON);
        assert!(media.caps.can_next);
        assert!(media.caps.can_previous);
        assert!(media.caps.can_seek);
        let track = media.track.expect("current media track");
        assert_eq!(track.key, "seed");
        assert_eq!(track.duration, Some(123.0));
        assert!(track.liked);
        assert!(!track.disliked);
        assert_eq!(
            track.url.as_deref(),
            Some("https://music.youtube.com/watch?v=seed")
        );
        assert!(track.art_remote_url.is_some());
        assert!(matches!(
            track.art_query,
            Some(crate::media::artwork::ArtQuery::Youtube { ref id }) if id == "seed"
        ));
    }

    #[test]
    fn media_snapshot_for_radio_stream_disables_track_specific_music_controls() {
        let mut engine = engine_with_queue(&[]);
        engine.queue.set(vec![radio_station("radio1")], 0);
        engine.loaded_video_id = Some("radio1".to_owned());
        engine.playback.paused = false;
        engine.playback.duration = Some(999.0);
        engine.set_media_art(crate::media::artwork::MediaArtworkReady {
            key: "radio1".to_owned(),
            path: std::path::PathBuf::from("/tmp/radio.jpg"),
        });

        let snapshot = engine.media_snapshot();

        assert_eq!(snapshot.status, crate::media::MediaPlaybackStatus::Playing);
        assert!(!snapshot.caps.can_next);
        assert!(snapshot.caps.can_previous);
        assert!(!snapshot.caps.can_seek);
        let track = snapshot.track.expect("radio track");
        assert_eq!(track.key, "radio1");
        assert!(track.is_live);
        assert_eq!(track.duration, None);
        assert_eq!(track.album, None);
        assert_eq!(
            track.url.as_deref(),
            Some("https://music.youtube.com/watch?v=radio1")
        );
        assert_eq!(track.art_remote_url, None);
        assert!(track.art_query.is_none());
        assert_eq!(
            track.art_file.as_deref(),
            Some(std::path::Path::new("/tmp/radio.jpg"))
        );
    }

    #[tokio::test]
    async fn remote_commands_cover_no_load_branches_and_gui_search_dispatch() {
        let mut engine = engine_with_queue(&[]);

        for command in [
            RemoteCommand::Next,
            RemoteCommand::Prev,
            RemoteCommand::TogglePause,
            RemoteCommand::SeekBack,
            RemoteCommand::SeekForward,
            RemoteCommand::QueuePlay { position: 1 },
            RemoteCommand::QueueRemove { position: 1 },
        ] {
            let (response, shutdown, effects) = engine.handle_remote(command).await;
            assert!(!response.ok);
            assert!(!shutdown);
            assert!(effects.is_empty());
        }

        let (response, shutdown, effects) = engine
            .handle_remote(RemoteCommand::RunSearch {
                ticket: 1,
                query: "   ".to_owned(),
                source: crate::search_source::SearchSource::Youtube,
            })
            .await;
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("empty_query"));
        assert!(!shutdown);
        assert!(effects.is_empty());

        let (response, _, effects) = engine
            .handle_remote(RemoteCommand::RunSearch {
                ticket: 2,
                query: "x".repeat(REMOTE_MAX_QUERY_BYTES + 1),
                source: crate::search_source::SearchSource::Youtube,
            })
            .await;
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("query_too_long"));
        assert!(effects.is_empty());

        let requester = RequesterKey::new(1, Some("engine-page".to_owned()));
        let (response, _, effects) = engine
            .handle_session_remote(
                RemoteCommand::RunSearch {
                    ticket: 3,
                    query: "  city pop  ".to_owned(),
                    source: crate::search_source::SearchSource::SoundCloud,
                },
                requester,
            )
            .await;
        assert!(response.ok);
        assert!(matches!(
            effects.as_slice(),
            [EngineEffect::GuiSearch {
                ticket: 3,
                query,
                source: crate::search_source::SearchSource::SoundCloud,
                ..
            }] if query == "city pop"
        ));

        let (response, _, effects) = engine
            .handle_remote(RemoteCommand::SetGeminiKey {
                key: "  key-123  ".to_owned(),
            })
            .await;
        assert!(response.ok);
        assert!(effects.is_empty());
        assert_eq!(engine.config.gemini_api_key.as_deref(), Some("key-123"));

        let (response, _, _) = engine
            .handle_remote(RemoteCommand::SetGeminiKey {
                key: "   ".to_owned(),
            })
            .await;
        assert!(response.ok);
        assert!(engine.config.gemini_api_key.is_none());

        engine.transport_recovery = Some(TransportRecovery {
            video_id: "queued-before-quit".to_owned(),
            paused: false,
            generation: 9,
            attempts: 0,
        });
        engine.transport_auto_recovery_armed = true;
        let (response, shutdown, effects) = engine.handle_remote(RemoteCommand::Quit).await;
        assert!(response.ok);
        assert!(shutdown);
        assert!(effects.is_empty());
        assert!(engine.loaded_video_id.is_none());
        assert!(engine.transport_recovery.is_none());
        assert!(!engine.transport_auto_recovery_armed);
    }

    #[tokio::test]
    async fn remote_repeat_and_streaming_guards_preserve_music_mode_invariant() {
        let mut engine = engine_with_queue(&["seed"]);
        engine.streaming = true;
        engine.queue.repeat = crate::queue::Repeat::Off;

        let (response, _, effects) = engine.handle_remote(RemoteCommand::CycleRepeat).await;

        assert!(response.ok);
        assert!(effects.is_empty());
        assert_eq!(engine.queue.repeat, crate::queue::Repeat::Off);

        engine.streaming = false;
        engine.queue.repeat = crate::queue::Repeat::All;
        let (response, _, effects) = engine
            .handle_remote(RemoteCommand::Streaming {
                state: ToggleState::On,
            })
            .await;

        assert!(response.ok);
        assert!(effects.is_empty());
        assert!(!engine.streaming);
        assert_eq!(engine.config.autoplay_streaming, Some(false));
    }

    #[tokio::test]
    async fn media_commands_ignore_invalid_or_disabled_operations() {
        let mut engine = engine_with_queue(&["seed"]);
        let _player_rx = install_accepting_player(&mut engine);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.playback.paused = false;
        engine.playback.time_pos = Some(5.0);
        engine.playback.duration = Some(60.0);

        for cmd in [
            crate::media::MediaCommand::SeekBy(f64::NAN),
            crate::media::MediaCommand::SeekTo(f64::NAN),
            crate::media::MediaCommand::SeekTo(-1.0),
            crate::media::MediaCommand::OpenUri("https://example.com/not-youtube".to_owned()),
        ] {
            let (shutdown, effects) = engine.handle_media(cmd).await;
            assert!(!shutdown);
            assert!(effects.is_empty());
        }
        assert_eq!(engine.playback.time_pos, Some(5.0));
        let epoch = engine.playback.position_epoch;

        let (shutdown, effects) = engine
            .handle_media(crate::media::MediaCommand::SetRate(0.0))
            .await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert!(engine.playback.paused);
        assert_eq!(engine.playback.position_epoch, epoch);

        engine.transport_recovery = Some(TransportRecovery {
            video_id: "queued-before-media-quit".to_owned(),
            paused: false,
            generation: 11,
            attempts: 0,
        });
        engine.transport_auto_recovery_armed = true;
        let (shutdown, effects) = engine.handle_media(crate::media::MediaCommand::Quit).await;
        assert!(shutdown);
        assert!(effects.is_empty());
        assert!(engine.loaded_video_id.is_none());
        assert!(engine.transport_recovery.is_none());
        assert!(!engine.transport_auto_recovery_armed);
    }

    #[tokio::test]
    async fn api_streaming_events_extend_clear_pending_and_trip_circuit_breaker() {
        let mut engine = engine_with_queue(&["seed"]);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.streaming = true;
        engine.streaming_pending = true;
        engine.consecutive_streaming_failures = 2;

        let additions = vec![song("fresh-a"), song("fresh-b")];
        let effects = engine
            .handle_api_event(ApiEvent::StreamingPreflighted {
                seed_video_id: "seed".to_owned(),
                songs: additions,
            })
            .await;
        assert!(effects.is_empty());
        assert!(!engine.streaming_pending);
        assert_eq!(engine.consecutive_streaming_failures, 0);
        assert!(
            engine
                .queue
                .ordered_iter()
                .any(|song| song.video_id == "fresh-a")
        );

        engine.streaming_pending = true;
        let effects = engine
            .handle_api_event(ApiEvent::StreamingResults {
                seed_video_id: "not-in-queue".to_owned(),
                candidates: vec![(song("ignored"), CandidateSource::YtdlpStreaming)],
            })
            .await;
        assert!(effects.is_empty());
        assert!(!engine.streaming_pending);
        assert!(
            !engine
                .queue
                .ordered_iter()
                .any(|song| song.video_id == "ignored")
        );

        for idx in 0..AUTOPLAY_MAX_FAILURES {
            engine.streaming = true;
            engine
                .handle_api_event(ApiEvent::StreamingError {
                    seed_video_id: "seed".to_owned(),
                    error: format!("failure-{idx}"),
                })
                .await;
        }
        assert!(!engine.streaming);
        assert_eq!(engine.config.autoplay_streaming, Some(false));
        assert!(
            engine
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("autoplay streaming failed")
        );

        for inert in [
            ApiEvent::TrackResolved {
                seq: 1,
                result: Ok(Vec::new()),
            },
            ApiEvent::SearchError {
                request_id: 1,
                source: crate::search_source::SearchSource::Youtube,
                error: "offline".to_owned(),
            },
            ApiEvent::PlaylistTracksError {
                title: "mix".to_owned(),
                error: "private".to_owned(),
            },
        ] {
            assert!(engine.handle_api_event(inert).await.is_empty());
        }
    }

    #[test]
    fn session_event_bias_caps_and_classifies_recent_skips() {
        let mut engine = engine_with_queue(&["seed"]);

        for idx in 0..(SESSION_EVENTS_CAP + 5) {
            let outcome = match idx % 3 {
                0 => DaemonOutcome::FullPlay,
                1 => DaemonOutcome::Skip,
                _ => DaemonOutcome::QuickSkip,
            };
            engine.record_session_event(
                &format!("artist-{idx}"),
                outcome,
                if matches!(outcome, DaemonOutcome::FullPlay) {
                    0.9
                } else {
                    0.1
                },
            );
        }

        assert_eq!(engine.session_events.len(), SESSION_EVENTS_CAP);
        assert_eq!(
            engine
                .session_events
                .front()
                .map(|event| event.artist_key.as_str()),
            Some("artist-5")
        );
        assert_eq!(engine.streaming_skip_streak(), 0);

        engine.record_session_event("skip-a", DaemonOutcome::QuickSkip, 0.0);
        engine.record_session_event("skip-b", DaemonOutcome::Skip, 0.2);
        assert_eq!(engine.streaming_skip_streak(), 2);

        let bias = engine.session_artist_bias();
        assert!(bias.get("skip-a").copied().unwrap_or_default() < 0.0);
        assert!(bias.get("skip-b").copied().unwrap_or_default() < 0.0);

        engine.playback.time_pos = Some(15.0);
        engine.playback.duration = Some(60.0);
        assert!((engine.playback_completion() - 0.25).abs() < f32::EPSILON);
        engine.playback.duration = None;
        assert!((engine.playback_completion() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn maybe_autoplay_extend_emits_real_streaming_request() {
        let mut engine = engine_with_queue(&["seed"]);
        engine.streaming = true;

        let effects = engine.maybe_autoplay_extend();

        assert_eq!(effects.len(), 1);
        match &effects[0] {
            EngineEffect::StreamingFallback {
                seed_video_id,
                limit,
                ..
            } => {
                assert_eq!(seed_video_id, "seed");
                assert_eq!(*limit, STREAMING_POOL_COUNT);
            }
            _ => panic!("expected streaming fallback"),
        }
        assert!(engine.streaming_pending);
    }

    #[tokio::test]
    async fn streaming_on_forces_request_even_when_queue_is_not_low() {
        let mut engine = engine_with_queue(&["seed", "a", "b", "c", "d", "e"]);
        engine.last_extend = Some(Instant::now());
        assert!(engine.queue.remaining() > AUTOPLAY_THRESHOLD);

        let (response, shutdown, effects) = engine
            .handle_remote(RemoteCommand::Streaming {
                state: ToggleState::On,
            })
            .await;

        assert!(response.ok);
        assert!(!shutdown);
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            EngineEffect::StreamingFallback { seed_video_id, .. } if seed_video_id == "seed"
        ));
    }

    #[tokio::test]
    async fn remote_semantic_caps_reject_abuse() {
        // Over-long search query (via Play) is rejected before the search fan-out.
        let mut engine = engine_with_queue(&["seed"]);
        let (resp, _, _) = engine
            .handle_remote(RemoteCommand::Play {
                query: "x".repeat(REMOTE_MAX_QUERY_BYTES + 1),
            })
            .await;
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("query_too_long"));

        // Over-long Gemini key is rejected and does not overwrite the stored key.
        let (resp, _, _) = engine
            .handle_remote(RemoteCommand::SetGeminiKey {
                key: "k".repeat(REMOTE_MAX_GEMINI_KEY_BYTES + 1),
            })
            .await;
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("key_too_long"));
        assert!(engine.config.gemini_api_key.is_none());

        // A request containing an unknown row is rejected as an indivisible stale selection.
        let (resp, _, _) = engine
            .handle_remote(RemoteCommand::EnqueueTracks {
                video_ids: vec!["not-a-valid-id".into(), "also/bad".into()],
            })
            .await;
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("stale_results"));
    }

    #[tokio::test]
    async fn remote_seek_to_is_clamped_when_duration_unknown() {
        let mut engine = engine_with_queue(&["seed"]);
        let _player_rx = install_accepting_player(&mut engine);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.playback.duration = None; // live / not-yet-probed
        let (resp, _, _) = engine
            .handle_remote(RemoteCommand::SeekTo { ms: u64::MAX })
            .await;
        assert!(resp.ok);
        // The absurd target is capped at the day ceiling, not passed through to mpv.
        assert_eq!(
            engine.playback.time_pos,
            Some(crate::playback_policy::MAX_SEEK_SECONDS)
        );
    }

    #[tokio::test]
    async fn streaming_on_forces_request_with_dj_gem_setting_off_too() {
        let mut engine = engine_with_queue(&["seed", "a", "b", "c", "d", "e"]);
        engine.config.ai_enabled = Some(false);
        assert!(engine.queue.remaining() > AUTOPLAY_THRESHOLD);

        let (response, shutdown, effects) = engine
            .handle_remote(RemoteCommand::Streaming {
                state: ToggleState::On,
            })
            .await;

        assert!(response.ok);
        assert!(!shutdown);
        assert!(matches!(
            effects.as_slice(),
            [EngineEffect::StreamingFallback { seed_video_id, .. }] if seed_video_id == "seed"
        ));
    }

    #[tokio::test]
    async fn media_shuffle_and_repeat_are_ignored_for_live_radio() {
        let mut engine = engine_with_queue(&[]);
        engine.queue.set(vec![radio_station("radio1")], 0);
        engine.loaded_video_id = Some("radio1".to_owned());

        let (shutdown, effects) = engine
            .handle_media(crate::media::MediaCommand::SetShuffle(true))
            .await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert!(!engine.queue.shuffle);
        assert_eq!(engine.config.shuffle, None);

        let (shutdown, effects) = engine
            .handle_media(crate::media::MediaCommand::SetRepeat(
                crate::queue::Repeat::All,
            ))
            .await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_eq!(engine.queue.repeat, crate::queue::Repeat::Off);
        assert_eq!(engine.config.repeat, crate::queue::Repeat::Off);
    }

    #[test]
    fn plan_local_streaming_filters_existing_queue_ids() {
        let mut engine = engine_with_queue(&["seed"]);
        let candidates = (0..12)
            .map(|i| {
                (
                    Song::remote(
                        format!("c{i}"),
                        format!("candidate {i}"),
                        format!("artist {i}"),
                        "3:00",
                    ),
                    CandidateSource::YtdlpStreaming,
                )
            })
            .collect();

        let picks = engine.plan_local_streaming("seed", candidates);

        assert!(!picks.is_empty());
        assert!(picks.iter().all(|song| song.video_id != "seed"));
    }

    #[test]
    fn session_snapshot_preserves_active_queue() {
        let mut engine = engine_with_queue(&["a", "b"]);
        engine.queue.next(false);

        let cache = engine.session_cache_snapshot();
        let snapshot = cache.normal_queue.expect("normal queue saved");

        assert_eq!(snapshot.cursor, 1);
        assert_eq!(snapshot.songs.len(), 2);
    }

    #[test]
    fn session_snapshot_preserves_local_mode_queue() {
        let mut engine = engine_with_queue(&["local-a", "local-b"]);
        engine.last_mode = LastMode::Local;
        engine.queue.next(false);
        engine.inactive_normal_queue = Some({
            let mut queue = Queue::default();
            queue.set(vec![song("normal")], 0);
            queue.snapshot()
        });
        engine.inactive_radio_queue = Some({
            let mut queue = Queue::default();
            queue.set(vec![radio_station("radio")], 0);
            queue.snapshot()
        });

        let cache = engine.session_cache_snapshot();

        assert_eq!(cache.last_mode, LastMode::Local);
        assert_eq!(cache.local_queue.as_ref().map(|s| s.cursor), Some(1));
        assert_eq!(cache.normal_queue.as_ref().map(|s| s.songs.len()), Some(1));
        assert_eq!(cache.radio_queue.as_ref().map(|s| s.songs.len()), Some(1));
    }

    // yt-dlp self-heal parity with the TUI reducer (src/app/tests.rs). Single-track
    // queues on the skip paths keep these hermetic: with no next track the engine
    // stops instead of calling `load_current` (which would spawn a real mpv).

    const EXTRACTION_ERR: &str = "mpv could not play this track (unrecognized file format)";

    #[tokio::test]
    async fn extraction_error_triggers_self_heal_effect() {
        let mut engine = engine_with_queue(&["a", "b"]);
        let effects = engine
            .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
            .await;
        assert!(
            matches!(&effects[..], [EngineEffect::YtdlpSelfHeal { video_id, .. }] if video_id == "a"),
            "runs an update check instead of skipping"
        );
        assert_eq!(
            engine.queue.current().map(|s| s.video_id.as_str()),
            Some("a"),
            "cursor stays on the failed track while the heal runs"
        );
        assert_eq!(engine.consecutive_play_errors, 0, "heal is not a strike");
    }

    #[tokio::test]
    async fn heal_without_update_falls_back_to_stop_on_single_track() {
        let mut engine = engine_with_queue(&["a"]);
        engine
            .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
            .await;
        let effects = engine.handle_heal_result("a".to_owned(), false).await;
        assert!(effects.is_empty());
        assert_eq!(
            engine.consecutive_play_errors, 1,
            "now it counts as a strike"
        );
        assert!(engine.last_error.is_some());
    }

    #[tokio::test]
    async fn heal_runs_once_per_track_then_plain_error_path() {
        let mut engine = engine_with_queue(&["a"]);
        engine
            .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
            .await;
        engine.handle_heal_result("a".to_owned(), false).await;
        // The same track failing again must not heal again (no retry loops).
        let effects = engine
            .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
            .await;
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, EngineEffect::YtdlpSelfHeal { .. })),
            "one heal per track per session"
        );
    }

    #[tokio::test]
    async fn stale_heal_result_is_dropped() {
        let mut engine = engine_with_queue(&["a", "b"]);
        engine
            .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
            .await;
        // Playback moved on (remote Next) while the check ran.
        engine.queue.next(false);
        let effects = engine.handle_heal_result("a".to_owned(), true).await;
        assert!(effects.is_empty(), "stale heal result is dropped");
        assert_eq!(
            engine.queue.current().map(|s| s.video_id.as_str()),
            Some("b")
        );
    }

    #[tokio::test]
    async fn non_extraction_error_skips_without_healing() {
        for error in [
            "mpv could not play this track (HTTP error 403 Forbidden)",
            "mpv could not play this track (HTTP Error 429: Too Many Requests)",
        ] {
            let mut engine = engine_with_queue(&["a"]);
            let effects = engine
                .handle_player_event(PlayerEvent::Error(error.to_owned()))
                .await;
            assert!(
                !effects
                    .iter()
                    .any(|e| matches!(e, EngineEffect::YtdlpSelfHeal { .. })),
                "HTTP rejection errors take the plain path: {error}"
            );
            assert_eq!(engine.consecutive_play_errors, 1);
            let last_error = engine.last_error.as_deref().unwrap_or_default();
            assert!(last_error.contains("YouTube rejected the stream"));
            assert!(last_error.contains("ytt doctor --verbose"));
            assert!(last_error.contains("JS runtime"));
        }
    }
}
