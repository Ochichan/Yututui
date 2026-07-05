use std::collections::{HashMap, HashSet, VecDeque};
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
    ArtworkRef, InstanceMode, QueueItemSnapshot, RemoteCommand, RemoteResponse,
    RemoteSettingChange, SettingsSnapshot, StatusSnapshot, ToggleState,
};
use crate::search_source::SearchConfig;
use crate::session::{LastMode, SessionCache};
use crate::signals::{self, Signals};
use crate::station::StationStore;
use crate::streaming::{self, CandidateSource, Cooc, StationState, StreamingConfig, StreamingMode};
use crate::util::sanitize;

const VOLUME_STEP: i64 = 5;
const VOLUME_MAX: i64 = 100;
const AUTOPLAY_THRESHOLD: usize = 3;
const STREAMING_POOL_COUNT: usize = 40;
const STREAMING_FALLBACK_COUNT: usize = 8;
const STREAMING_RECENT_ARTISTS: usize = 12;
const AUTOPLAY_COOLDOWN: Duration = Duration::from_secs(60);
const AUTOPLAY_MAX_FAILURES: u8 = 3;
const MAX_CONSECUTIVE_PLAY_ERRORS: u8 = 3;
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
pub enum EngineError {
    Player(String),
}

impl EngineError {
    fn reason(&self) -> &'static str {
        match self {
            EngineError::Player(_) => "mpv_unavailable",
        }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Player(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for EngineError {}

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
        ticket: u64,
        query: String,
        source: crate::search_source::SearchSource,
        config: SearchConfig,
    },
}

pub struct DaemonEngine {
    player: Option<PlayerRuntime>,
    player_emit: Arc<dyn Fn(PlayerEvent) + Send + Sync>,
    queue: Queue,
    playback: DaemonPlayback,
    config: Config,
    library: Library,
    signals: Signals,
    station: StationStore,
    loaded_video_id: Option<String>,
    streaming: bool,
    streaming_pending: bool,
    last_extend: Option<Instant>,
    consecutive_streaming_failures: u8,
    last_error: Option<String>,
    consecutive_play_errors: u8,
    /// yt-dlp self-heal bookkeeping (mirrors the TUI's `YtdlpHeal`): the in-flight
    /// healed track, the per-track one-shot guard, and the update-check cooldown.
    heal_pending: Option<String>,
    heal_attempted: HashSet<String>,
    heal_last_check: Option<Instant>,
    last_mode: LastMode,
    inactive_normal_queue: Option<QueueSnapshot>,
    inactive_radio_queue: Option<QueueSnapshot>,
    session_events: VecDeque<DaemonSessionEvent>,
    /// The media-session artwork cache's resolved file for a track, keyed by
    /// `video_id`; surfaced in [`Self::media_snapshot`] while the keys match.
    media_art: Option<crate::media::artwork::MediaArtworkReady>,
    /// Rows the GUI can address by bare `video_id` (`play_tracks`/`enqueue_tracks`):
    /// the songs of the most recently completed GUI search. Replaced wholesale per
    /// search — the GUI only ever acts on the results it currently shows.
    gui_search_index: std::collections::HashMap<String, Song>,
}

struct PlayerRuntime {
    handle: PlayerHandle,
    _guard: player::Mpv,
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
        if let Some(dir) = data_dir() {
            player::lifetime::reap_orphans(&dir);
        }
        let mut engine = Self::with_state(
            EngineState {
                config: Config::load(),
                station: StationStore::load(),
                library: Library::load(),
                signals: Signals::load(),
            },
            Arc::new(emit),
        );

        // Resolve which yt-dlp/mpv this process runs (managed vs system vs override)
        // before the first `ensure_player` — the mpv spawn pins ytdl_hook to it.
        crate::tools::init(&engine.config.tools).await;
        // Keep the managed copy fresh for long daemon runs. No-op emit: the daemon
        // has no status line and `check_and_update` already logs its outcomes.
        crate::tools::ytdlp::spawn_maintainer(engine.config.tools.clone(), |_| {});

        if options.resume {
            engine.restore_last_session();
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
            streaming: config.effective_autoplay_streaming()
                && config.effective_repeat() == crate::queue::Repeat::Off,
            config,
            library,
            signals,
            station,
            loaded_video_id: None,
            streaming_pending: false,
            last_extend: None,
            consecutive_streaming_failures: 0,
            last_error: None,
            consecutive_play_errors: 0,
            heal_pending: None,
            heal_attempted: HashSet::new(),
            heal_last_check: None,
            last_mode: LastMode::Normal,
            inactive_normal_queue: None,
            inactive_radio_queue: None,
            session_events: VecDeque::new(),
            media_art: None,
            gui_search_index: std::collections::HashMap::new(),
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

    pub fn initial_effects(&mut self) -> Vec<EngineEffect> {
        self.maybe_autoplay_extend()
    }

    pub async fn handle_remote(
        &mut self,
        command: RemoteCommand,
    ) -> (RemoteResponse, bool, Vec<EngineEffect>) {
        self.last_error = None;
        let mut effects = Vec::new();
        let shutdown = matches!(command, RemoteCommand::Quit);
        let response = match command {
            RemoteCommand::Status => RemoteResponse::status(self.status()),
            RemoteCommand::Quit => {
                self.stop_playback();
                self.save_session();
                RemoteResponse::ok("stopping daemon".to_string())
            }
            RemoteCommand::Next => {
                self.record_outgoing(false);
                let response = self.next_track().await;
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
                if self.queue.repeat == crate::queue::Repeat::Off && self.streaming {
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
                effects.extend(self.force_autoplay_extend());
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
                } else {
                    // Off-loop: the api actor answers with GuiSearchCompleted, which the
                    // host loop indexes here and pushes on the `search` topic.
                    effects.push(EngineEffect::GuiSearch {
                        ticket,
                        query,
                        source,
                        config: self.config.effective_search(),
                    });
                    RemoteResponse::ok("searching".to_string())
                }
            }
            RemoteCommand::PlayTracks { video_ids } => {
                let response = self.play_tracks(video_ids).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::EnqueueTracks { video_ids } => {
                let response = self.enqueue_tracks(video_ids).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Apply { change } => {
                let (response, setting_effects) = self.apply_gui_setting(change);
                effects.extend(setting_effects);
                response
            }
            RemoteCommand::SetGeminiKey { key } => {
                let key = key.trim();
                self.config.gemini_api_key = (!key.is_empty()).then(|| key.to_string());
                self.save_config("daemon gemini key");
                RemoteResponse::ok("gemini key updated".to_string())
            }
            RemoteCommand::ResetAllSettings => {
                // Danger zone (GUI double-confirms). Keep playback rolling; the fresh
                // defaults apply live where cheap and at next launch elsewhere.
                self.config = Config::default();
                self.save_config("daemon settings reset");
                RemoteResponse::ok("settings reset".to_string())
            }
        };
        (response, shutdown, effects)
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
                    Ok(repeat) if repeat.is_on() && self.streaming => ok(self),
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
            ("eq", "preset") => match as_str()
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
            {
                Some(preset) => {
                    self.config.eq_preset = preset;
                    self.config.eq_bands = None; // preset gains take over
                    self.apply_audio_filter();
                    self.save_config("daemon eq preset");
                    ok(self)
                }
                None => bad(),
            },
            ("eq", "bands") => match serde_json::from_value::<[f64; 10]>(value.clone()) {
                Ok(bands) => {
                    self.config.eq_bands = Some(bands);
                    self.config.eq_preset = crate::eq::EqPreset::Custom;
                    self.apply_audio_filter();
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
            ("search", "audius_app_name") => match as_str() {
                Some(s) => {
                    self.config.search.audius_app_name =
                        (!s.trim().is_empty()).then(|| s.trim().to_string());
                    self.save_config("daemon audius app name");
                    ok(self)
                }
                None => bad(),
            },
            ("search", "jamendo_client_id") => match as_str() {
                Some(s) => {
                    self.config.search.jamendo_client_id =
                        (!s.trim().is_empty()).then(|| s.trim().to_string());
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
            ("storage", "download_dir") => match as_str() {
                Some(s) => {
                    self.config.download_dir =
                        (!s.trim().is_empty()).then(|| std::path::PathBuf::from(s.trim()));
                    self.save_config("daemon download dir");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "cookies_file") => match as_str() {
                Some(s) => {
                    self.config.cookies_file =
                        (!s.trim().is_empty()).then(|| std::path::PathBuf::from(s.trim()));
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
    fn apply_audio_filter(&mut self) {
        let af = self.current_audio_filter();
        if let Some(player) = &self.player {
            player.handle.send(PlayerCmd::SetAudioFilter(af));
        }
    }

    /// Record a completed GUI search so `play_tracks`/`enqueue_tracks` can address its
    /// rows by bare `video_id`. Wholesale replace: the GUI acts on what it shows.
    pub fn index_gui_search(&mut self, groups: &[crate::api::GuiSearchGroup]) {
        self.gui_search_index.clear();
        for group in groups {
            for song in &group.songs {
                self.gui_search_index
                    .insert(song.video_id.clone(), song.clone());
            }
        }
    }

    /// Resolve a GUI-addressed `video_id` to a playable [`Song`]: the last search's
    /// rows first, then the library (favorites/history), then a bare row mpv resolves
    /// at load time (covers e.g. AI suggestion chips that never went through search).
    fn resolve_video_id(&self, video_id: &str) -> Song {
        if let Some(song) = self.gui_search_index.get(video_id) {
            return song.clone();
        }
        if let Some(song) = self
            .library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .find(|s| s.video_id == video_id)
        {
            return song.clone();
        }
        Song::remote(video_id, video_id, "", "")
    }

    async fn play_tracks(&mut self, video_ids: Vec<String>) -> RemoteResponse {
        let mut songs = video_ids.iter().map(|id| self.resolve_video_id(id));
        let Some(first) = songs.next() else {
            return RemoteResponse::err("empty_selection");
        };
        let rest: Vec<Song> = songs.collect();
        if !self.queue.play_now(first) {
            return RemoteResponse::err("queue_full");
        }
        if !rest.is_empty() {
            self.queue.insert_next_many(rest);
        }
        self.save_session();
        self.load_current()
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn enqueue_tracks(&mut self, video_ids: Vec<String>) -> RemoteResponse {
        if video_ids.is_empty() {
            return RemoteResponse::err("empty_selection");
        }
        let songs: Vec<Song> = video_ids
            .iter()
            .map(|id| self.resolve_video_id(id))
            .collect();
        let old_len = self.queue.len();
        let was_idle = self.loaded_video_id.is_none();
        let added = if self.config.effective_enqueue_next() && !was_idle {
            self.queue.insert_next_many(songs)
        } else {
            self.queue.extend(songs)
        };
        if added == 0 {
            return RemoteResponse::err("queue_full");
        }
        self.save_session();
        if was_idle {
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            return self
                .load_current()
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
        }
        RemoteResponse::status(self.status())
    }

    pub async fn handle_player_event(&mut self, event: PlayerEvent) -> Vec<EngineEffect> {
        match event {
            PlayerEvent::TimePos(t) => {
                self.playback.time_pos = Some(t);
                self.playback.time_pos_at = Some(Instant::now());
                if t > 0.0 {
                    self.consecutive_play_errors = 0;
                }
                Vec::new()
            }
            PlayerEvent::Duration(d) => {
                self.playback.duration = Some(d);
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
                self.playback.volume = volume.round() as i64;
                Vec::new()
            }
            PlayerEvent::Metadata(_) => Vec::new(),
            // The headless engine has no live-sync surface; timeshift state is the TUI
            // reducer's concern (`Msg::PlayerCacheTime`).
            PlayerEvent::CacheTime(_) => Vec::new(),
            // Recording is a TUI-only feature; the headless engine ignores container hints.
            PlayerEvent::AudioCodec(_) | PlayerEvent::FileFormat(_) => Vec::new(),
            PlayerEvent::Eof => {
                self.record_outgoing(true);
                self.advance_after_end().await
            }
            PlayerEvent::Error(error) => self.handle_playback_error(error).await,
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
            title: current.map(|song| song.title.clone()),
            artist: current.map(|song| song.artist.clone()),
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
                    title: song.title.clone(),
                    artist: song.artist.clone(),
                    duration: song.duration.clone(),
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
                    self.record_outgoing(false);
                    let _ = self.next_track().await;
                    effects.extend(self.maybe_autoplay_extend());
                }
            }
            MediaCommand::Previous => {
                if self.queue.current().is_some() {
                    let _ = self.prev_track().await;
                }
            }
            MediaCommand::SeekBy(seconds) => {
                if self.media_can_seek() {
                    let _ = self.seek(seconds);
                }
            }
            MediaCommand::SeekTo(pos) => {
                if self.media_can_seek() && pos >= 0.0 {
                    // Out-of-range SetPosition is ignored per the MPRIS spec.
                    if let Some(d) = self.playback.duration
                        && pos > d + 0.5
                    {
                        return (false, effects);
                    }
                    self.note_seek(pos);
                    if let Some(player) = &self.player {
                        player.handle.send(PlayerCmd::SeekAbsolute(pos));
                    }
                }
            }
            MediaCommand::SetShuffle(on) => {
                if self.queue.shuffle != on {
                    self.queue.set_shuffle(on);
                    self.config.shuffle = Some(on);
                    self.save_config("daemon shuffle setting");
                    self.save_session();
                }
            }
            MediaCommand::SetRepeat(mode) => {
                // Music-mode invariant: an OS widget can't enable repeat while streaming is on.
                if self.queue.repeat != mode && !(mode.is_on() && self.streaming) {
                    self.queue.repeat = mode;
                    self.config.repeat = mode;
                    self.save_config("daemon repeat setting");
                    self.save_session();
                }
            }
            MediaCommand::SetVolume(v) => {
                let volume = (v.clamp(0.0, 1.0) * 100.0).round() as i64;
                if volume != self.playback.volume {
                    let _ = self.adjust_volume(volume - self.playback.volume);
                }
            }
            MediaCommand::SetRate(rate) => {
                if rate == 0.0 {
                    return Box::pin(self.handle_media(MediaCommand::Pause)).await;
                }
                let speed = clamp_speed(rate);
                if (speed - self.playback.speed).abs() > f64::EPSILON {
                    self.playback.speed = speed;
                    if let Some(player) = &self.player {
                        player.handle.send(PlayerCmd::SetProperty {
                            name: "speed".to_owned(),
                            value: Value::from(speed),
                        });
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
                    if self.queue.play_now(song) {
                        if let Err(e) = self.load_current().await {
                            self.last_error = Some(e.to_string());
                            self.stop_playback();
                        }
                        effects.extend(self.maybe_autoplay_extend());
                    }
                }
            }
            MediaCommand::Quit => {
                self.stop_playback();
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

    /// Like/dislike from the OS surface: same favorite/dislike bookkeeping the TUI's
    /// rating cycle performs, persisted immediately (the daemon has no Cmd loop).
    fn media_set_rating(&mut self, like: bool) {
        let Some(song) = self.queue.current().cloned() else {
            return;
        };
        if song.is_radio_station() {
            if like {
                self.library.toggle_favorite(&song);
                if let Err(e) = self.library.save() {
                    tracing::warn!(error = %e, "failed to save daemon library");
                }
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
        if let Err(e) = self.library.save() {
            tracing::warn!(error = %e, "failed to save daemon library");
        }
        if let Err(e) = self.signals.save() {
            tracing::warn!(error = %e, "failed to save daemon signals");
        }
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

    fn restore_last_session(&mut self) {
        let cache = SessionCache::load();
        self.last_mode = cache.last_mode;
        self.inactive_normal_queue = cache.normal_queue.clone();
        self.inactive_radio_queue = cache.radio_queue.clone();

        if let Some(snapshot) = cache.active_queue().cloned() {
            self.queue.restore_snapshot(snapshot);
            self.reset_idle_playback();
            return;
        }

        let songs: Vec<Song> = match cache.last_mode {
            LastMode::Radio => self.library.radios.iter().cloned().collect(),
            LastMode::Normal => self.library.history.iter().cloned().collect(),
        };
        if !songs.is_empty() {
            self.queue.set(songs, 0);
            self.reset_idle_playback();
        }
    }

    async fn resume_session(&mut self) -> RemoteResponse {
        self.restore_last_session();
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
        if self.queue.next(false).is_some() {
            return self
                .load_current()
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
        self.queue.prev();
        self.load_current()
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn queue_play(&mut self, position: usize) -> RemoteResponse {
        if position >= self.queue.len() {
            return RemoteResponse::err("queue_index");
        }
        self.queue.goto(position);
        self.load_current()
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
        let current_changed = self.queue.remove_at(position).unwrap_or(false);
        self.save_session();

        if current_changed {
            if let Some(next_pos) = next_pos_after_removal {
                self.queue.goto(next_pos);
                return self
                    .load_current()
                    .await
                    .map(|_| RemoteResponse::status(self.status()))
                    .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
            }
            self.stop_playback();
        }

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
        self.playback.paused = !self.playback.paused;
        if let Some(player) = &self.player {
            player.handle.send(PlayerCmd::CyclePause);
        }
        RemoteResponse::status(self.status())
    }

    async fn search_and_play(&mut self, query: String) -> RemoteResponse {
        let song = match self.first_search_result(&query).await {
            Ok(Some(song)) => song,
            Ok(None) => return RemoteResponse::err("no_results"),
            Err(()) => return RemoteResponse::err("search_error"),
        };
        if !self.queue.play_now(song) {
            return RemoteResponse::err("queue_full");
        }
        self.load_current()
            .await
            .map(|_| RemoteResponse::status(self.status()))
            .unwrap_or_else(|e| RemoteResponse::err(e.reason()))
    }

    async fn search_and_enqueue(&mut self, query: String) -> RemoteResponse {
        let song = match self.first_search_result(&query).await {
            Ok(Some(song)) => song,
            Ok(None) => return RemoteResponse::err("no_results"),
            Err(()) => return RemoteResponse::err("search_error"),
        };
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
        self.save_session();
        if was_idle {
            self.queue
                .goto(old_len.min(self.queue.len().saturating_sub(1)));
            return self
                .load_current()
                .await
                .map(|_| RemoteResponse::status(self.status()))
                .unwrap_or_else(|e| RemoteResponse::err(e.reason()));
        }
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
                tracing::warn!(%query, %error, "daemon remote search failed");
                Err(())
            }
        }
    }

    fn adjust_volume(&mut self, delta: i64) -> RemoteResponse {
        self.set_volume(self.playback.volume + delta)
    }

    fn set_volume(&mut self, percent: i64) -> RemoteResponse {
        self.playback.volume = percent.clamp(0, VOLUME_MAX);
        if let Some(player) = &self.player {
            player
                .handle
                .send(PlayerCmd::SetVolume(self.playback.volume));
        }
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
        self.note_seek(target);
        if let Some(player) = &self.player {
            player.handle.send(PlayerCmd::SeekRelative(seconds));
        }
        RemoteResponse::status(self.status())
    }

    fn seek_to(&mut self, pos: f64) -> RemoteResponse {
        if self.loaded_video_id.is_none() {
            return RemoteResponse::err("nothing_playing");
        }
        let mut target = pos.max(0.0);
        if let Some(duration) = self.playback.duration {
            target = target.min(duration);
        }
        self.note_seek(target);
        if let Some(player) = &self.player {
            player.handle.send(PlayerCmd::SeekAbsolute(target));
        }
        RemoteResponse::status(self.status())
    }

    /// Record a position discontinuity at `pos` (seek applied / track restarted).
    fn note_seek(&mut self, pos: f64) {
        self.playback.time_pos = Some(pos);
        self.playback.time_pos_at = Some(Instant::now());
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
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
        if let Err(e) = self.config.save() {
            tracing::warn!(error = %e, "failed to save daemon autoplay streaming setting");
        }
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
                self.config.speed = Some(speed);
                self.playback.speed = speed;
                if let Some(player) = &self.player {
                    player.handle.send(PlayerCmd::SetProperty {
                        name: "speed".to_owned(),
                        value: Value::from(speed),
                    });
                }
                self.save_config("daemon speed setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::SeekSeconds { seconds } => {
                self.config.seek_seconds = Some(clamp_seek_seconds(f64::from(seconds)));
                self.save_config("daemon seek step setting");
                (RemoteResponse::status(self.status()), Vec::new())
            }
            RemoteSettingChange::Normalize { value } => {
                self.config.normalize = Some(value);
                let af = self.current_audio_filter();
                if let Some(player) = &self.player {
                    player.handle.send(PlayerCmd::SetAudioFilter(af));
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

    fn save_config(&self, context: &str) {
        if let Err(e) = self.config.save() {
            tracing::warn!(error = %e, "failed to save {context}");
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
        // Self-heal (mirrors the TUI reducer): an extraction-shaped failure on a
        // yt-dlp-resolved track is the stale-yt-dlp signature — update in the
        // background and retry this track once. Unlike the TUI (whose session mpv
        // keeps its spawn-time ytdl_path), the daemon can simply drop its player:
        // the respawn re-pins ytdl_hook to the fresh binary.
        if crate::tools::looks_like_extraction_failure(&error)
            && self.heal_pending.is_none()
            && let Some(song) = self.queue.current()
            && song.prefetch_target().is_some()
            && !self.heal_attempted.contains(&song.video_id)
            && self
                .heal_last_check
                .is_none_or(|at| at.elapsed() >= crate::tools::HEAL_COOLDOWN)
        {
            let video_id = song.video_id.clone();
            self.heal_attempted.insert(video_id.clone());
            self.heal_last_check = Some(Instant::now());
            self.heal_pending = Some(video_id.clone());
            self.last_error = Some(error);
            return vec![EngineEffect::YtdlpSelfHeal {
                video_id,
                tools: self.config.tools.clone(),
            }];
        }
        self.last_error = Some(error);
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
        self.handle_playback_error("stream resolution failed (yt-dlp already current)".to_owned())
            .await
    }

    async fn load_current(&mut self) -> Result<(), EngineError> {
        self.ensure_player().await?;
        self.load_current_loaded();
        Ok(())
    }

    fn load_current_loaded(&mut self) {
        let Some(song) = self.queue.current().cloned() else {
            self.stop_playback();
            return;
        };
        self.playback.paused = false;
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
        self.playback.duration = None;
        self.loaded_video_id = Some(song.video_id.clone());
        self.library.record_play(&song);
        if let Err(e) = self.library.save() {
            tracing::warn!(error = %e, "failed to save daemon library history");
        }
        self.save_session();
        if let Some(player) = &self.player {
            player.handle.send(PlayerCmd::Load(song.playback_target()));
        }
    }

    fn stop_playback(&mut self) {
        if let Some(player) = self.player.take() {
            player.handle.send(PlayerCmd::Stop);
        }
        self.reset_idle_playback();
        self.loaded_video_id = None;
    }

    fn reset_idle_playback(&mut self) {
        self.playback.paused = true;
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
        self.playback.duration = None;
    }

    async fn ensure_player(&mut self) -> Result<(), EngineError> {
        if self.player.is_some() {
            return Ok(());
        }
        let emit = Arc::clone(&self.player_emit);
        let (handle, guard) = player::spawn(
            move |event| (emit)(event),
            data_dir(),
            self.config.effective_cookies_file(),
            self.config.effective_gapless(),
        )
        .await
        .map_err(|e| EngineError::Player(format!("failed to start mpv: {e:#}")))?;

        handle.send(PlayerCmd::SetVolume(self.playback.volume));
        let speed = self.playback.speed;
        if (speed - 1.0).abs() > f64::EPSILON {
            handle.send(PlayerCmd::SetProperty {
                name: "speed".to_owned(),
                value: Value::from(speed),
            });
        }
        handle.send(PlayerCmd::SetAudioFilter(self.current_audio_filter()));
        self.player = Some(PlayerRuntime {
            handle,
            _guard: guard,
        });
        Ok(())
    }

    fn current_audio_filter(&self) -> String {
        eq::build_af_string(
            &self.config.effective_eq_bands(),
            self.config.effective_normalize(),
        )
        .unwrap_or_default()
    }

    fn maybe_autoplay_extend(&mut self) -> Vec<EngineEffect> {
        self.autoplay_extend(false)
    }

    fn force_autoplay_extend(&mut self) -> Vec<EngineEffect> {
        self.autoplay_extend(true)
    }

    fn autoplay_extend(&mut self, force: bool) -> Vec<EngineEffect> {
        if !self.streaming || self.streaming_pending {
            return Vec::new();
        }
        if !force && self.queue.remaining() > AUTOPLAY_THRESHOLD {
            return Vec::new();
        }
        if !force
            && self
                .last_extend
                .is_some_and(|t| t.elapsed() < AUTOPLAY_COOLDOWN)
        {
            return Vec::new();
        }
        let Some(cur) = self.queue.current() else {
            return Vec::new();
        };
        if cur.is_radio_station() {
            return Vec::new();
        }

        let seed = format!("{} — {}", cur.title, cur.artist);
        let seed_video_id = cur.video_id.clone();
        let exclude_ids = self.streaming_exclude_ids(&seed_video_id);
        self.last_extend = Some(Instant::now());
        self.streaming_pending = true;
        vec![EngineEffect::StreamingFallback {
            seed,
            seed_video_id,
            exclude_ids,
            limit: STREAMING_POOL_COUNT,
            mode: self.config.streaming.mode,
            config: self.config.effective_search(),
        }]
    }

    fn streaming_exclude_ids(&self, seed_video_id: &str) -> Vec<String> {
        let profile = self.config.streaming.mode.profile(&self.config.streaming);
        let mut ids: HashSet<String> = self
            .queue
            .ordered_iter()
            .filter(|song| !song.is_radio_station())
            .map(|song| song.video_id.clone())
            .collect();
        ids.insert(seed_video_id.to_owned());
        let favorite_ids: HashSet<&str> = self
            .library
            .favorites
            .iter()
            .filter(|song| !song.is_radio_station())
            .map(|s| s.video_id.as_str())
            .collect();
        for (idx, song) in self
            .library
            .history
            .iter()
            .filter(|song| !song.is_radio_station())
            .enumerate()
        {
            let inside_horizon = idx < profile.history_block_horizon;
            let protected_old_favorite =
                profile.allow_old_liked_repeats && favorite_ids.contains(song.video_id.as_str());
            if inside_horizon || !protected_old_favorite {
                ids.insert(song.video_id.clone());
            }
        }
        ids.into_iter().collect()
    }

    fn plan_local_streaming(
        &mut self,
        seed_video_id: &str,
        mut candidates: Vec<(Song, CandidateSource)>,
    ) -> Vec<Song> {
        let st = self.build_station_state(seed_video_id);
        let cooc = Cooc::build(self.signals.play_log(), &self.config.streaming.cooc);
        self.augment_streaming_candidates(seed_video_id, &mut candidates);
        let pool = streaming::pool_from_tagged(candidates);
        streaming::plan_local(
            pool,
            &st,
            &self.signals,
            &cooc,
            &self.config.streaming,
            STREAMING_FALLBACK_COUNT,
            signals::unix_now(),
        )
    }

    async fn extend_sanitized_streaming(
        &mut self,
        seed_video_id: &str,
        songs: Vec<Song>,
        fallback: &[Song],
    ) -> Vec<EngineEffect> {
        let sanitized = streaming::sanitize_final_picks(
            songs,
            fallback,
            self.config.streaming.mode,
            &self.config.streaming,
        );
        if !sanitized.is_empty()
            && streaming::final_preflight_needed(
                &sanitized,
                fallback,
                self.config.streaming.mode,
                &self.config.streaming,
            )
        {
            self.streaming_pending = true;
            return vec![EngineEffect::StreamingPreflight {
                seed_video_id: seed_video_id.to_owned(),
                picks: sanitized,
                fallback: fallback.to_vec(),
                mode: self.config.streaming.mode,
                config: self.config.streaming.clone(),
            }];
        }
        self.extend_queue_from_streaming(sanitized).await
    }

    async fn extend_queue_from_streaming(&mut self, songs: Vec<Song>) -> Vec<EngineEffect> {
        let added = self.queue.extend(songs);
        if added == 0 {
            self.note_streaming_failure("autoplay streaming found no new tracks".to_owned());
            return Vec::new();
        }
        self.consecutive_streaming_failures = 0;
        self.save_session();
        if self.loaded_video_id.is_none() && self.queue.remaining() > 0 {
            self.queue.next(false);
            if let Err(e) = self.load_current().await {
                self.last_error = Some(e.to_string());
                self.stop_playback();
            }
        }
        Vec::new()
    }

    fn note_streaming_failure(&mut self, status: String) {
        self.last_error = Some(status);
        if self.streaming {
            self.consecutive_streaming_failures =
                self.consecutive_streaming_failures.saturating_add(1);
            if self.consecutive_streaming_failures >= AUTOPLAY_MAX_FAILURES {
                self.streaming = false;
                self.streaming_pending = false;
                self.config.autoplay_streaming = Some(false);
                if let Err(e) = self.config.save() {
                    tracing::warn!(error = %e, "failed to save daemon streaming circuit-breaker");
                }
            }
        }
    }

    fn augment_streaming_candidates(
        &self,
        seed_video_id: &str,
        candidates: &mut Vec<(Song, CandidateSource)>,
    ) {
        let mode = self.config.streaming.mode;
        let profile = mode.profile(&self.config.streaming);
        let seed_artist = self.streaming_seed_artist_key(seed_video_id);
        let mut seen: HashSet<String> = candidates
            .iter()
            .map(|(song, _)| song.video_id.clone())
            .collect();
        seen.extend(
            self.queue
                .ordered_iter()
                .filter(|song| !song.is_radio_station())
                .map(|song| song.video_id.clone()),
        );
        seen.insert(seed_video_id.to_owned());

        let (liked_cap, history_cap) = match mode {
            StreamingMode::Focused => (14, 8),
            StreamingMode::Balanced => (10, 14),
            StreamingMode::Discovery => (6, 24),
        };

        let mut favorites: Vec<Song> = self
            .library
            .favorites
            .iter()
            .filter(|song| !song.is_radio_station())
            .cloned()
            .collect();
        favorites.sort_by(|a, b| {
            local_neighbor_score(b, &seed_artist, &self.signals).total_cmp(&local_neighbor_score(
                a,
                &seed_artist,
                &self.signals,
            ))
        });
        for song in favorites.into_iter().take(liked_cap) {
            if seen.insert(song.video_id.clone()) {
                candidates.push((song, CandidateSource::LikedNeighbor));
            }
        }

        let mut added_history = 0usize;
        for song in self
            .library
            .history
            .iter()
            .filter(|song| !song.is_radio_station())
            .skip(profile.history_block_horizon)
        {
            if seen.insert(song.video_id.clone()) {
                candidates.push((song.clone(), CandidateSource::HistoryCooc));
                added_history += 1;
                if added_history >= history_cap {
                    break;
                }
            }
        }
    }

    fn build_station_state(&self, seed_video_id: &str) -> StationState {
        let profile = self.config.streaming.mode.profile(&self.config.streaming);
        let mut recent_track_ids: Vec<String> = self
            .queue
            .ordered_iter()
            .filter(|song| !song.is_radio_station())
            .map(|song| song.video_id.clone())
            .collect();
        recent_track_ids.extend(
            self.library
                .history
                .iter()
                .filter(|s| !s.is_radio_station())
                .take(profile.history_block_horizon)
                .map(|s| s.video_id.clone()),
        );

        let mut recent_artist_keys: Vec<String> = self
            .library
            .history
            .iter()
            .filter(|s| !s.is_radio_station())
            .take(STREAMING_RECENT_ARTISTS)
            .map(|s| signals::normalize_artist(&s.artist))
            .collect();
        recent_artist_keys.reverse();
        if let Some(cur) = self.queue.current()
            && !cur.is_radio_station()
        {
            push_artist_key(&mut recent_artist_keys, &cur.artist);
        }
        for song in self
            .queue
            .ordered_iter()
            .skip(self.queue.cursor_pos().saturating_add(1))
            .filter(|song| !song.is_radio_station())
            .take(8)
        {
            push_artist_key(&mut recent_artist_keys, &song.artist);
        }

        let favorite_artist_keys: HashSet<String> = self
            .library
            .favorites
            .iter()
            .filter(|s| !s.is_radio_station())
            .map(|s| signals::normalize_artist(&s.artist))
            .collect();
        let skip_streak = self.streaming_skip_streak();
        let temporary_novelty_boost =
            if self.config.streaming.mode == StreamingMode::Focused && skip_streak >= 2 {
                0.12
            } else {
                0.0
            };
        let temporary_familiarity_boost =
            if self.config.streaming.mode == StreamingMode::Discovery && skip_streak >= 2 {
                0.20
            } else {
                0.0
            };

        StationState {
            mode: self.config.streaming.mode,
            seed_video_id: seed_video_id.to_owned(),
            seed_artist_key: self.streaming_seed_artist_key(seed_video_id),
            recent_track_ids,
            recent_artist_keys,
            banned_track_ids: HashSet::new(),
            banned_artist_keys: self.station.avoid_artist_keys().into_iter().collect(),
            favorite_artist_keys,
            session_artist_bias: self.session_artist_bias(),
            temporary_novelty_boost,
            temporary_familiarity_boost,
        }
    }

    fn streaming_seed_artist_key(&self, seed_video_id: &str) -> String {
        if let Some(cur) = self.queue.current()
            && cur.video_id == seed_video_id
            && !cur.is_radio_station()
        {
            return signals::normalize_artist(&cur.artist);
        }
        self.library
            .history
            .iter()
            .filter(|s| !s.is_radio_station())
            .find(|s| s.video_id == seed_video_id)
            .map(|s| signals::normalize_artist(&s.artist))
            .unwrap_or_default()
    }

    fn session_artist_bias(&self) -> HashMap<String, f32> {
        let mut out: HashMap<String, f32> = HashMap::new();
        for event in self.session_events.iter().rev().take(8) {
            let completion = event.completion.clamp(0.0, 1.0);
            let delta = match event.outcome {
                DaemonOutcome::FullPlay => 0.05 * completion.max(0.5),
                DaemonOutcome::Skip => -0.10 * (1.0 - completion).max(0.25),
                DaemonOutcome::QuickSkip => -0.20 * (1.0 - completion).max(0.5),
            };
            let entry = out.entry(event.artist_key.clone()).or_insert(0.0);
            *entry = (*entry + delta).clamp(-0.50, 0.35);
        }
        out
    }

    fn streaming_skip_streak(&self) -> usize {
        self.session_events
            .iter()
            .rev()
            .take_while(|e| matches!(e.outcome, DaemonOutcome::Skip | DaemonOutcome::QuickSkip))
            .count()
    }

    fn record_outgoing(&mut self, full: bool) {
        let Some(song) = self.queue.current().cloned() else {
            return;
        };
        if song.is_radio_station() {
            return;
        }
        let artist_key = signals::normalize_artist(&song.artist);
        let now = signals::unix_now();
        let (outcome, completion) = if full {
            self.signals
                .record_play(&song.video_id, &artist_key, 1.0, now);
            (DaemonOutcome::FullPlay, 1.0)
        } else {
            let completion = self.playback_completion();
            self.signals
                .record_skip(&song.video_id, &artist_key, completion, now, 0.6);
            let outcome = if completion < signals::STRONG_SKIP_FRAC {
                DaemonOutcome::QuickSkip
            } else {
                DaemonOutcome::Skip
            };
            (outcome, completion)
        };
        self.record_session_event(&artist_key, outcome, completion);
        if let Err(e) = self.signals.save() {
            tracing::warn!(error = %e, "failed to save daemon signals");
        }
    }

    fn playback_completion(&self) -> f32 {
        match (self.playback.time_pos, self.playback.duration) {
            (Some(t), Some(d)) if d > 0.0 => (t / d).clamp(0.0, 1.0) as f32,
            _ => 0.5,
        }
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
        let mut cache = SessionCache::from_radio_mode(self.last_mode == LastMode::Radio);
        match self.last_mode {
            LastMode::Normal => {
                cache.normal_queue = Some(self.queue.snapshot());
                cache.radio_queue = self.inactive_radio_queue.clone();
            }
            LastMode::Radio => {
                cache.radio_queue = Some(self.queue.snapshot());
                cache.normal_queue = self.inactive_normal_queue.clone();
            }
        }
        cache
    }

    fn save_session(&self) {
        if let Err(e) = self.session_cache_snapshot().save() {
            tracing::warn!(error = %e, "failed to save daemon session");
        }
    }
}

fn local_neighbor_score(song: &Song, seed_artist_key: &str, sig: &Signals) -> f32 {
    let artist_key = signals::normalize_artist(&song.artist);
    let seed_bonus = if artist_key == seed_artist_key {
        1.0
    } else {
        0.0
    };
    seed_bonus + sig.artist_weight(&artist_key)
}

fn push_artist_key(keys: &mut Vec<String>, artist: &str) {
    let key = signals::normalize_artist(artist);
    if !key.is_empty() {
        keys.push(key);
    }
}

fn data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|dirs| dirs.data_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("title-{id}"), "artist".to_owned(), "3:00")
    }

    fn engine_with_queue(ids: &[&str]) -> DaemonEngine {
        let mut queue = Queue::default();
        queue.set(ids.iter().map(|id| song(id)).collect(), 0);
        DaemonEngine {
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
            streaming: false,
            streaming_pending: false,
            last_extend: None,
            consecutive_streaming_failures: 0,
            last_error: None,
            consecutive_play_errors: 0,
            heal_pending: None,
            heal_attempted: HashSet::new(),
            heal_last_check: None,
            last_mode: LastMode::Normal,
            inactive_normal_queue: None,
            inactive_radio_queue: None,
            session_events: VecDeque::new(),
            media_art: None,
            gui_search_index: std::collections::HashMap::new(),
        }
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
        let mut engine = engine_with_queue(&["a"]);
        let effects = engine
            .handle_player_event(PlayerEvent::Error(
                "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
            ))
            .await;
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, EngineEffect::YtdlpSelfHeal { .. })),
            "network errors take the plain path"
        );
        assert_eq!(engine.consecutive_play_errors, 1);
    }
}
