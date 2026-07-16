//! Persistent configuration: cross-platform paths, atomic save, and a one-time import
//! of the old `~/.youtube-music-cli/config.json`.
//!
//! Auth: either an inline `cookie` (raw `Cookie:` header) or a `cookies_file` pointing
//! at a Netscape `cookies.txt`. If no file is configured, `~/Music/yututui/cookies.txt`
//! (the platform music folder) is tried. The header form feeds ytmapi-rs; the file is
//! also handed to mpv/yt-dlp so they own stream resolution (PO tokens, throttling).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

use crate::ai::GeminiModel;
use crate::eq::{self, EqPreset};
use crate::i18n::{DjGemLanguage, Language};
use crate::queue::Repeat;
use crate::search_source::SearchConfig;
use crate::streaming::StreamingConfig;
use crate::theme::ThemeConfig;

mod animation;
mod audio;
mod recovery;
mod spotify;
mod storage;
mod visual;
pub use animation::{AnimationsConfig, FPS_DEFAULT, FPS_MAX, FPS_MIN};
pub use audio::{
    AudioBackend, AudioConfig, AudioRuntimeConfig, LongFormSeekOptimization,
    MPV_CACHE_BACK_DEFAULT, MPV_CACHE_BACK_LEGACY_DEFAULT, MPV_CACHE_DEFAULTS_REVISION,
    MPV_CACHE_FORWARD_DEFAULT, MPV_CACHE_FORWARD_LEGACY_DEFAULT, MpvAudioConfig,
    MpvAudioRuntimeConfig,
};
pub use spotify::SpotifyImportMode;
pub use storage::{
    default_cookies_file, default_download_dir, default_recording_dir, peek_saved_language,
};
pub use visual::{AlbumArtQuality, PlayerBarPosition, VideoOverlay};

pub(crate) use storage::config_path;
#[cfg(test)]
use storage::{config_for_missing_profile, import_old_from, ytm_dir_under_audio_dir};
use storage::{
    external_cookies_warning, import_external_cookies_file, normalize_user_dir,
    parse_netscape_cookies, validate_external_cookies_file,
};

/// Clamp range for playback speed (matches the `>`/`<` controls and the settings slider).
pub const SPEED_MIN: f64 = 0.5;
pub const SPEED_MAX: f64 = 2.0;

/// Version of the interactive Beginner Mode walkthrough shipped by this build. Bump this only
/// when the ordered steps or their completion contracts change; copy-only edits must not make a
/// user repeat the tour.
pub const BEGINNER_TUTORIAL_VERSION: u16 = 3;

/// Persisted Beginner Mode walkthrough cursor. The step stays a string deliberately: a newer
/// build may write a step this build does not know, and retaining that value lets the app decline
/// to run an incompatible future tour without destroying its progress on the next settings save.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BeginnerTutorialProgress {
    pub content_version: u16,
    pub next_step: String,
}

impl BeginnerTutorialProgress {
    /// The reset/initial cursor: the first ordered step of the current tour version.
    pub fn start() -> Self {
        Self::default()
    }
}

impl Default for BeginnerTutorialProgress {
    fn default() -> Self {
        Self {
            content_version: BEGINNER_TUTORIAL_VERSION,
            next_step: "language".to_owned(),
        }
    }
}

/// Upper bound on `config.json` when loading. The config holds a handful of settings and EQ
/// bands — nothing legitimate approaches this — so a larger file (corrupt or hostile) is
/// treated like an unreadable one and rebuilt rather than read wholesale into memory.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Cap on the cookies file (`cookies.txt`) read for the `Cookie:` auth header. Real cookie
/// jars are a few KB; this only rejects a pathological/hostile path before it's read wholesale
/// into memory, and the read also refuses to follow a symlink.
const MAX_COOKIE_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const EXTERNAL_COOKIES_COPY: &str = "cookies.external.txt";

/// Clamp range for the seek step (seconds) used by the seek-back/-forward keys, exposed as
/// a slider on the Playback settings tab. Default is 10s.
pub const SEEK_SECONDS_MIN: f64 = 1.0;
pub const SEEK_SECONDS_MAX: f64 = 60.0;
pub const SEEK_SECONDS_DEFAULT: f64 = 10.0;

/// Clamp/round a playback-speed multiplier to one decimal within the supported range. The
/// single source of this rule — both the TUI (`settings`, re-exported) and the headless
/// daemon (`daemon::engine`) apply it, so a bound change here can't drift between them.
pub fn clamp_speed(s: f64) -> f64 {
    // A non-finite rate (a stray NaN/inf from an MPRIS `Rate` write) must not poison playback
    // speed — `NaN.clamp(..)` stays NaN — so normalize it to 1.0 before rounding/clamping.
    let s = crate::util::finite_or(s, 1.0);
    ((s * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX)
}

/// Clamp/round a seek step to whole seconds within the supported range.
pub fn clamp_seek_seconds(s: f64) -> f64 {
    // Mirror clamp_speed: coalesce a non-finite value before rounding/clamping, since
    // `NaN.round().clamp(..)` stays NaN and would poison the seek step.
    crate::util::finite_or(s, SEEK_SECONDS_DEFAULT)
        .round()
        .clamp(SEEK_SECONDS_MIN, SEEK_SECONDS_MAX)
}

/// Concurrent `yt-dlp`/ffmpeg downloads. Keep the default conservative because each download
/// can spawn multiple external processes and dominate CPU/RAM outside this Rust process.
pub const DOWNLOAD_CONCURRENCY_MIN: usize = 1;
pub const DOWNLOAD_CONCURRENCY_MAX: usize = 3;
pub const DOWNLOAD_CONCURRENCY_DEFAULT: usize = 2;

/// Radio recording (a Shortwave-style feature) bounds. Durations are seconds; the settings
/// slider shows the max in minutes. Defaults match Shortwave (30s min, 15min max, 10 kept).
pub const RECORDING_MIN_SECONDS_MIN: u32 = 5;
pub const RECORDING_MIN_SECONDS_MAX: u32 = 600;
pub const RECORDING_MIN_SECONDS_DEFAULT: u32 = 30;
pub const RECORDING_MAX_SECONDS_MIN: u32 = 60;
pub const RECORDING_MAX_SECONDS_MAX: u32 = 3600;
pub const RECORDING_MAX_SECONDS_DEFAULT: u32 = 900;
pub const RECORDING_PAST_TRACKS_MIN: usize = 1;
pub const RECORDING_PAST_TRACKS_MAX: usize = 50;
pub const RECORDING_PAST_TRACKS_DEFAULT: usize = 10;

/// Scrobbling accounts (the **Accounts** settings tab) plus shared behavior, grouped under
/// one JSON key (`"scrobble"`). Every field is `#[serde(default)]` so older config files
/// forward-migrate cleanly.
// No `Debug`: holds the Last.fm session key and the ListenBrainz token (see `Config`).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrobbleConfig {
    pub lastfm: LastfmConfig,
    pub listenbrainz: ListenBrainzConfig,
    /// Also scrobble local files (when they carry real title + artist metadata). `None` → on.
    pub local_files: Option<bool>,
}

/// Last.fm account + behavior. The app ships embedded API credentials
/// (see [`crate::scrobble::lastfm`]); `api_key`/`api_secret` override them when set.
// No `Debug`: `session_key` and `api_secret` are secrets.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LastfmConfig {
    /// `None` → on whenever a session key is present (mirrors the `ai_enabled` idiom), so
    /// connecting is enough; this exists to switch scrobbling off without disconnecting.
    pub enabled: Option<bool>,
    /// The infinite-lifetime web-service session key from `auth.getSession`.
    pub session_key: Option<String>,
    /// Display-only: whose account the session key belongs to.
    pub username: Option<String>,
    /// Mirror in-app like/unlike to Last.fm `track.love`/`track.unlove`. `None` → on.
    pub love_sync: Option<bool>,
    /// Override the embedded application API key (for self-built binaries).
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
}

impl LastfmConfig {
    /// Connected *and* not switched off — the "should we scrobble here" gate.
    pub fn is_active(&self) -> bool {
        self.session_key.as_deref().is_some_and(|k| !k.is_empty()) && self.enabled.unwrap_or(true)
    }
}

/// ListenBrainz account. Token auth only — no browser flow (the user copies their token
/// from listenbrainz.org/settings). `api_url` supports self-hosted instances.
// No `Debug`: `token` is a secret.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ListenBrainzConfig {
    /// `None` → on whenever a token is present (same idiom as [`LastfmConfig::enabled`]).
    pub enabled: Option<bool>,
    pub token: Option<String>,
    /// `None` → `https://api.listenbrainz.org`.
    pub api_url: Option<String>,
}

impl ListenBrainzConfig {
    pub fn is_active(&self) -> bool {
        self.token.as_deref().is_some_and(|t| !t.is_empty()) && self.enabled.unwrap_or(true)
    }
}

/// Spotify Web API access (the transfer feature). Development-mode apps are limited to an
/// allowlist of 25 users, so every user registers their **own** app at
/// developer.spotify.com and pastes the Client ID here — there is no embedded one. Tokens
/// live in a separate `spotify_token.json` (the client rotates refresh tokens outside the
/// settings screen; splitting the files avoids write races with config saves).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotifyConfig {
    /// The user's own app Client ID (PKCE — no secret exists).
    pub client_id: Option<String>,
    /// Loopback-redirect port; the app's registered redirect URI must be exactly
    /// `http://127.0.0.1:<port>/callback`. `None` → [`SPOTIFY_REDIRECT_PORT_DEFAULT`].
    pub redirect_port: Option<u16>,
    /// Optional market (ISO country code) for Spotify track search during export.
    pub market: Option<String>,
    /// How the TUI import flow handles ambiguous matches when creating local Library playlists.
    pub import_mode: SpotifyImportMode,
}

pub const SPOTIFY_REDIRECT_PORT_DEFAULT: u16 = 9271;

/// External-tool management (the managed yt-dlp and binary-path overrides), grouped
/// under one JSON key (`"tools"`), every field `#[serde(default)]` so older config
/// files forward-migrate cleanly. See [`crate::tools`] for the selection policy this
/// feeds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ToolsConfig {
    /// Let the app download/update its own yt-dlp standalone binary (the fix for
    /// distro-frozen yt-dlp breaking playback). `None` → on. Users who refuse
    /// networked executable downloads set `false` and own keeping yt-dlp current.
    pub ytdlp_managed: Option<bool>,
    /// Release channel for the managed binary. `None` → nightly (upstream's own
    /// recommendation for YouTube users — extractor fixes land there within a day).
    pub ytdlp_channel: Option<crate::tools::YtdlpChannel>,
    /// Absolute path to a specific yt-dlp to use **unconditionally** (no version
    /// compare). The `YTM_YTDLP` env var overrides even this.
    pub ytdlp_path: Option<PathBuf>,
    /// Absolute path to the mpv binary. `YTM_MPV` env var wins; `None` → `mpv` on
    /// PATH. For old distros where a newer mpv lives outside PATH (flatpak, etc.).
    pub mpv_path: Option<PathBuf>,
}

impl ToolsConfig {
    /// Whether the managed yt-dlp is enabled (default on).
    pub fn managed_enabled(&self) -> bool {
        self.ytdlp_managed.unwrap_or(true)
    }

    /// The managed binary's release channel (default nightly).
    pub fn channel(&self) -> crate::tools::YtdlpChannel {
        self.ytdlp_channel.unwrap_or_default()
    }

    /// The unconditional yt-dlp override: `YTM_YTDLP` env var, else `ytdlp_path`.
    pub fn ytdlp_override(&self) -> Option<PathBuf> {
        if let Ok(env) = std::env::var("YTM_YTDLP")
            && !env.trim().is_empty()
        {
            return Some(PathBuf::from(env.trim()));
        }
        self.ytdlp_path.clone()
    }

    /// The unconditional mpv override: `YTM_MPV` env var, else `mpv_path`.
    pub fn mpv_override(&self) -> Option<PathBuf> {
        if let Ok(env) = std::env::var("YTM_MPV")
            && !env.trim().is_empty()
        {
            return Some(PathBuf::from(env.trim()));
        }
        self.mpv_path.clone()
    }

    /// The mpv program to spawn: `YTM_MPV` env var, else `mpv_path`, else `"mpv"`.
    pub fn mpv_program(&self) -> String {
        match self.mpv_override() {
            Some(p) => p.to_string_lossy().into_owned(),
            None => "mpv".to_owned(),
        }
    }
}

// No `Debug`: this holds secrets (`cookie` raw header, `gemini_api_key`, the scrobbling
// session key/token), so a stray `{:?}` must not be able to leak them — same reason
// `GeminiClient` omits it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Show explanatory control labels and, on a writable launch, the interactive beginner tour.
    /// The ordinary/default value is false so pre-feature, recovered, and programmatic configs do
    /// not unexpectedly enter onboarding. [`Self::fresh_install`] is the sole opt-in constructor.
    pub beginner_mode: bool,
    /// Versioned cursor for the last incomplete beginner-tour step.
    pub beginner_tutorial: BeginnerTutorialProgress,
    /// Raw `Cookie:` header for music.youtube.com (takes precedence over the file).
    pub cookie: Option<String>,
    /// Path to a Netscape `cookies.txt` exported from the browser.
    /// `None` -> `<user music dir>/yututui/cookies.txt`.
    pub cookies_file: Option<PathBuf>,
    /// Startup volume, 0-100.
    pub volume: i64,
    /// Where downloads are saved. `None` -> `<user music dir>/yututui`.
    pub download_dir: Option<PathBuf>,
    /// Local Deck scan roots. The download directory is included by default; explicit roots are
    /// additional user music folders.
    pub local: LocalConfig,
    /// Most simultaneous downloads. `None` -> [`DOWNLOAD_CONCURRENCY_DEFAULT`].
    pub download_concurrency: Option<usize>,
    /// Capture the mouse for buttons and click-to-seek. `None` → enabled.
    pub mouse: Option<bool>,
    /// Show album art / video thumbnail in the player view. `None` → off (opt-in). The
    /// terminal's graphics protocol is probed unconditionally at startup (the About icon needs
    /// it regardless), so turning this on takes effect live. See [`crate::artwork`].
    pub album_art: Option<bool>,
    /// Detail level for remote album art rendered inside the terminal. Defaults to `High`, which
    /// matches the pre-setting 768px cap; local embedded covers retain that existing cap.
    pub album_art_quality: AlbumArtQuality,
    /// Where the player control block sits (see [`PlayerBarPosition`]). `None` → `Bottom`,
    /// the docked layout; `Top` keeps the legacy layout.
    pub player_bar_position: Option<PlayerBarPosition>,
    /// Whether the docked control box is collapsed on non-Player screens (the ▲/▼ footer
    /// toggle / `B`). The Player screen always shows the box — it IS the player. `None` → false.
    pub control_box_collapsed: Option<bool>,

    // Playback / EQ -----------------------------------------------------------
    /// Selected equalizer preset.
    pub eq_preset: EqPreset,
    /// Hand-tuned band gains (dB). `None` → use the preset's gains.
    pub eq_bands: Option<[f64; eq::BANDS]>,
    /// Loudness normalization (`dynaudnorm`). `None` → off.
    pub normalize: Option<bool>,
    /// Playback speed multiplier. `None` → 1.0×.
    pub speed: Option<f64>,
    /// Seek step in seconds for the seek-back/-forward keys. `None` → 10s.
    pub seek_seconds: Option<f64>,
    /// Adjust volume with the mouse wheel over the player volume cluster. `None` → on.
    pub mouse_wheel_volume: Option<bool>,
    /// Text zoom level in percent (one of 100/125/150/175/200/250/300), rendered via
    /// the terminal's text sizing protocol (Ctrl+wheel / Ctrl+-/=). `None` → 100%.
    /// Applied only on terminals that pass the startup probe, so a config saved under
    /// kitty stays harmless elsewhere.
    pub text_zoom: Option<u16>,
    /// Freeze the Ctrl+wheel zoom gesture (`ToggleZoomWheelLock`, default Ctrl+L): while
    /// locked, Ctrl+wheel scrolls like a plain wheel and only the Ctrl+-/= keys zoom.
    /// `None` → unlocked.
    pub zoom_wheel_lock: Option<bool>,
    /// Gapless playback. `None` → on. Takes effect at the next launch (an mpv flag).
    pub gapless: Option<bool>,
    /// Shuffle playback order. `None` → off.
    pub shuffle: Option<bool>,
    /// Repeat mode.
    pub repeat: Repeat,
    /// Add manually enqueued tracks immediately after the current track instead of the queue end.
    /// `None` → off, preserving the historical append-to-end behavior.
    pub enqueue_next: Option<bool>,
    /// Auto-extend the queue with related tracks when it runs low. `None` -> off.
    #[serde(alias = "autoplay_radio")]
    pub autoplay_streaming: Option<bool>,
    /// Auto-play the restored last track as soon as the app launches. `None` → off
    /// (opt-in; a fresh launch otherwise seeds the track paused and idle).
    pub autoplay_on_start: Option<bool>,
    /// When the `v` video overlay is open, auto-play the next queue track's video as the
    /// current one ends (TUI only; the overlay doesn't exist in the daemon). `None` → off.
    pub auto_continue_videos: Option<bool>,
    /// Search source selection, enabled providers, and provider identifiers.
    pub search: SearchConfig,
    /// Local streaming engine tuning (scoring weights, diversity, cooldown). Defaults ship a
    /// single tuned `Balanced` profile; every field is `#[serde(default)]`.
    #[serde(alias = "radio")]
    pub streaming: StreamingConfig,

    // Animations --------------------------------------------------------------
    /// Player-view eye-candy toggles (the Animations tab). All off by default; see
    /// [`AnimationsConfig`].
    pub animations: AnimationsConfig,

    // DJ Gem assistant ------------------------------------------------------------
    /// Google Gemini API key. The `GEMINI_API_KEY` env var overrides this when set.
    pub gemini_api_key: Option<String>,
    /// Which Gemini model the assistant uses.
    pub gemini_model: GeminiModel,
    /// Whether the DJ Gem assistant is enabled. `None` → on, so existing configs that already hold
    /// a key keep DJ Gem working. Lets the user switch DJ Gem off while keeping the key saved.
    pub ai_enabled: Option<bool>,
    /// Show Korean/Japanese/CJK track metadata as Latin-script display overlays. `None` → off.
    /// This never mutates the source metadata; it only affects UI labels and may use Gemini to
    /// improve the local romanizer when an API key is configured.
    pub romanized_titles: Option<bool>,
    /// The language DJ Gem replies in, independent of the UI [`language`](Self::language).
    /// `Auto` (default) follows the UI language; a concrete choice forces that language.
    /// Retro mode overrides it to English. See [`Self::effective_dj_gem_language`].
    pub dj_gem_language: DjGemLanguage,

    // Theme -------------------------------------------------------------------
    /// Color theme preset plus per-role `#RRGGBB` overrides.
    pub theme: ThemeConfig,
    /// Dedicated-radio-mode theme. `theme` always holds the *normal* theme (a radio-mode
    /// settings save deliberately keeps it that way), so the radio theme needs its own
    /// persisted slot or it dies with the process. `None` → Radio default on radio entry.
    pub radio_theme: Option<ThemeConfig>,
    /// Dedicated-Local-Deck theme. Like `radio_theme`, this is separate so editing Local's
    /// appearance cannot overwrite the normal theme. `None` → Local Launch on Local entry,
    /// including for configs written before this field existed.
    pub local_theme: Option<ThemeConfig>,
    /// Linux basic TTY compatibility mode: English UI, Retro theme, ASCII-safe rendering.
    pub retro_mode: bool,

    // Localization ------------------------------------------------------------
    /// UI language. `English` is the default; switching it re-renders every label, button,
    /// hint, and message in the chosen language (see [`crate::i18n`]).
    pub language: Language,

    // Keybindings -------------------------------------------------------------
    /// User keybinding overrides, keyed `"<context>.<action>"` → chord string (e.g.
    /// `"player.toggle_pause" -> "space"`). Only entries that differ from the built-in
    /// defaults are stored; everything else falls back to [`crate::keymap`]'s defaults.
    pub keybindings: std::collections::BTreeMap<String, String>,

    /// User mouse gesture overrides, keyed `"<context>.<gesture>"` → action id (e.g.
    /// `"search.right_click" -> "context_menu"`). Only deviations from the built-in
    /// defaults are stored; everything else falls back to [`crate::mousemap`]'s defaults.
    pub mouse_bindings: std::collections::BTreeMap<String, String>,

    // External video overlay --------------------------------------------------
    /// Window layout for the mpv video overlay (`v` opens, `Shift+V` toggles). Defaults to
    /// `Compact` (top-right ~30%).
    pub video_layout: VideoOverlay,

    // OS media session ----------------------------------------------------------
    /// Publish playback to the OS media session — macOS Now Playing, Windows SMTC,
    /// Linux MPRIS — and accept media keys / widget control. `None` → on.
    pub media_controls: Option<bool>,

    // Scrobbling ------------------------------------------------------------------
    /// Last.fm / ListenBrainz accounts and scrobbling behavior. See [`ScrobbleConfig`].
    pub scrobble: ScrobbleConfig,

    // Spotify transfer ------------------------------------------------------------
    /// Spotify Web API app registration for playlist import/export. See [`SpotifyConfig`].
    pub spotify: SpotifyConfig,

    // External tools ----------------------------------------------------------------
    /// Managed yt-dlp + binary-path overrides. See [`ToolsConfig`] and [`crate::tools`].
    pub tools: ToolsConfig,

    // Audio backend ------------------------------------------------------------------
    /// First-class audio backend settings. The only supported backend is mpv; this group
    /// makes its output/device/cache policy explicit instead of relying only on escape hatches.
    pub audio: AudioConfig,

    // Radio recording -----------------------------------------------------------------
    /// Shortwave-style recording of the live radio stream. See [`RecordingConfig`].
    pub recording: RecordingConfig,

    // Updates -------------------------------------------------------------------------
    /// Check GitHub on startup for a newer YuTuTui! release and, if behind, show an About-card
    /// notice + nav-brand dot + one-time toast. Defaults to `true`; set `false` to make the
    /// app never contact GitHub for its own version. See [`crate::update`].
    pub update_check_enabled: bool,
}

/// Local Deck library roots. Kept separate from `download_dir`: the download folder remains the
/// app-owned fallback, while these settings describe broader user-owned music folders.
pub const LOCAL_IMPORT_PATH_TEMPLATE_DEFAULT: &str =
    "{album_artist}/{year} - {album}/{disc_track} - {title} [{youtube_id}]";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LocalConfig {
    /// Include the configured/default download folder as a depth-limited scan root
    /// (up to Artist/Album nesting). `None` preserves the default for older config files.
    pub include_download_dir: Option<bool>,
    /// Additional user music folders. The current Settings UI edits the first entry; the Vec
    /// leaves room for a later multi-root editor without another config migration.
    pub roots: Vec<LocalRootConfig>,
    /// Path template used by import organization commands when no CLI override is provided.
    pub import_path_template: Option<String>,
}

impl LocalConfig {
    pub fn include_download_dir(&self) -> bool {
        self.include_download_dir.unwrap_or(true)
    }

    pub fn first_root(&self) -> Option<&LocalRootConfig> {
        self.roots.first()
    }

    pub fn import_path_template(&self) -> &str {
        self.import_path_template
            .as_deref()
            .map(str::trim)
            .filter(|template| !template.is_empty())
            .unwrap_or(LOCAL_IMPORT_PATH_TEMPLATE_DEFAULT)
    }
}

/// One Local Deck user music folder.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LocalRootConfig {
    pub path: PathBuf,
    pub enabled: Option<bool>,
    pub recursive: Option<bool>,
}

impl LocalRootConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn recursive(&self) -> bool {
        self.recursive.unwrap_or(true)
    }

    pub fn normalized_path(&self) -> Option<PathBuf> {
        normalize_user_dir(&self.path.to_string_lossy())
    }
}

/// Radio recording (a Shortwave-style feature). Only takes effect while an internet-radio
/// station plays. `#[serde(default)]` so older config files forward-migrate cleanly and an
/// unknown `mode` string degrades to the default rather than resetting the whole file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingConfig {
    /// What to do with each track heard (Off / Decide / Save all). Defaults to Off (opt-in).
    pub mode: crate::recorder::RecordingMode,
    /// Discard tracks shorter than this many seconds. Clamped to [5, 600].
    pub min_duration_secs: u32,
    /// Force-split a track once it reaches this many seconds. Clamped to [60, 3600].
    pub max_duration_secs: u32,
    /// Where saved recordings go. `None` → `<music>/yututui/recordings`.
    pub track_directory: Option<PathBuf>,
    /// Max recent tracks kept in the in-memory recordings browser. Clamped to [1, 50].
    pub past_tracks_count: usize,
    /// Show a toast when a track is recorded / saved. Defaults to on.
    pub notify: bool,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            mode: crate::recorder::RecordingMode::Nothing,
            min_duration_secs: RECORDING_MIN_SECONDS_DEFAULT,
            max_duration_secs: RECORDING_MAX_SECONDS_DEFAULT,
            track_directory: None,
            past_tracks_count: RECORDING_PAST_TRACKS_DEFAULT,
            notify: true,
        }
    }
}

#[derive(Clone)]
pub struct PlayerRuntimeConfig {
    pub volume: i64,
    pub cookies_file: Option<PathBuf>,
    pub gapless: bool,
    pub audio: AudioRuntimeConfig,
}

#[derive(Clone)]
pub struct DownloadRuntimeConfig {
    pub dir: PathBuf,
    pub cookies_file: Option<PathBuf>,
    pub max_concurrent: usize,
}

#[derive(Clone)]
pub struct AiRuntimeConfig {
    pub key: Option<String>,
    pub model: GeminiModel,
    pub assistant_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            beginner_mode: false,
            beginner_tutorial: BeginnerTutorialProgress::default(),
            cookie: None,
            cookies_file: None,
            volume: 100,
            download_dir: None,
            local: LocalConfig::default(),
            download_concurrency: None,
            mouse: None,
            album_art: None,
            album_art_quality: AlbumArtQuality::default(),
            player_bar_position: None,
            control_box_collapsed: None,
            eq_preset: EqPreset::default(),
            eq_bands: None,
            normalize: None,
            speed: None,
            seek_seconds: None,
            mouse_wheel_volume: None,
            text_zoom: None,
            zoom_wheel_lock: None,
            gapless: None,
            shuffle: None,
            repeat: Repeat::default(),
            enqueue_next: None,
            autoplay_streaming: None,
            autoplay_on_start: None,
            auto_continue_videos: None,
            search: SearchConfig::default(),
            streaming: StreamingConfig::default(),
            animations: AnimationsConfig::default(),
            gemini_api_key: None,
            gemini_model: GeminiModel::default(),
            ai_enabled: None,
            romanized_titles: None,
            dj_gem_language: DjGemLanguage::default(),
            theme: ThemeConfig::default(),
            radio_theme: None,
            local_theme: None,
            retro_mode: false,
            language: Language::default(),
            keybindings: std::collections::BTreeMap::new(),
            mouse_bindings: std::collections::BTreeMap::new(),
            video_layout: VideoOverlay::default(),
            media_controls: None,
            scrobble: ScrobbleConfig::default(),
            spotify: SpotifyConfig::default(),
            tools: ToolsConfig::default(),
            audio: AudioConfig::default(),
            recording: RecordingConfig::default(),
            update_check_enabled: true,
        }
    }
}

impl Config {
    pub fn player_runtime(&self, cookies_file: Option<PathBuf>) -> PlayerRuntimeConfig {
        PlayerRuntimeConfig {
            volume: self.volume,
            cookies_file,
            gapless: self.effective_gapless(),
            audio: self.audio.runtime(),
        }
    }

    pub fn download_runtime(&self, cookies_file: Option<PathBuf>) -> DownloadRuntimeConfig {
        DownloadRuntimeConfig {
            dir: self.effective_download_dir(),
            cookies_file,
            max_concurrent: self.effective_download_concurrency(),
        }
    }

    pub fn ai_runtime(&self) -> AiRuntimeConfig {
        AiRuntimeConfig {
            key: self.effective_ai_service_key(),
            model: self.effective_gemini_model(),
            assistant_enabled: self.effective_ai_enabled(),
        }
    }

    /// The `Cookie:` header to authenticate ytmapi-rs, from the inline value or by
    /// parsing the configured/default `cookies.txt`. `None` if neither yields cookies.
    pub fn effective_cookie(&self) -> Option<String> {
        if let Some(c) = &self.cookie
            && !c.trim().is_empty()
        {
            return Some(c.clone());
        }
        if let Some(file) = self.effective_cookies_file()
            && let Ok(bytes) =
                crate::util::safe_fs::read_no_symlink_limited(&file, MAX_COOKIE_BYTES)
            && let Ok(content) = String::from_utf8(bytes)
        {
            let header = parse_netscape_cookies(&content);
            if !header.is_empty() {
                return Some(header);
            }
        }
        None
    }

    /// The cookies file to try. An explicit setting wins; otherwise the cross-platform
    /// default is `<user music dir>/yututui/cookies.txt`.
    pub fn effective_cookies_file(&self) -> Option<PathBuf> {
        self.cookies_file.clone().or_else(default_cookies_file)
    }

    /// A cookies file that can safely be handed to external tools (`mpv`/`yt-dlp`).
    ///
    /// The effective path may be a default export location that does not exist yet. Passing a
    /// missing file makes yt-dlp fail instead of falling back to anonymous playback, so external
    /// process spawns must use this helper rather than [`Self::effective_cookies_file`]. This also
    /// rejects symlinks/reparse points and oversized files before an external process sees them.
    pub fn existing_cookies_file(&self) -> Option<PathBuf> {
        self.effective_cookies_file()
            .filter(|path| validate_external_cookies_file(path).is_ok())
    }

    /// The cookies file path to pass to external tools. A valid configured/default cookie jar is
    /// copied into the private app data directory first, so mpv/yt-dlp receive an app-owned copy
    /// rather than an arbitrary user-supplied path. If no app data directory is available, falls
    /// back to the strictly validated source file.
    pub fn cookies_file_for_external_tools(&self, data_dir: Option<&Path>) -> Option<PathBuf> {
        let (path, warning) = self.cookies_file_for_external_tools_with_warning(data_dir);
        if let Some(warning) = warning {
            tracing::warn!(reason = %warning, "cookies file unavailable for external tools");
        }
        path
    }

    /// Same as [`Self::cookies_file_for_external_tools`], but also returns an actionable warning
    /// suitable for the TUI status line. The warning intentionally avoids echoing the cookies path.
    pub fn cookies_file_for_external_tools_with_warning(
        &self,
        data_dir: Option<&Path>,
    ) -> (Option<PathBuf>, Option<String>) {
        let Some(source) = self.effective_cookies_file() else {
            return (None, None);
        };
        if let Err(e) = validate_external_cookies_file(&source) {
            if self.cookies_file.is_none() && e.kind() == std::io::ErrorKind::NotFound {
                return (None, None);
            }
            return (None, Some(external_cookies_warning(&e)));
        }
        let Some(data_dir) = data_dir else {
            return (Some(source), None);
        };
        match import_external_cookies_file(&source, data_dir) {
            Ok(path) => (Some(path), None),
            Err(e) => (None, Some(external_cookies_warning(&e))),
        }
    }

    /// The concrete directory downloads are saved to. Precedence: `YTM_DOWNLOAD_DIR`
    /// env override, then the configured `download_dir`, then `<user music dir>/yututui`.
    /// The music folder resolves per-OS (`~/Music` on macOS, the Music known-folder on
    /// Windows) via the `directories` crate.
    pub fn effective_download_dir(&self) -> PathBuf {
        if let Ok(env) = std::env::var("YTM_DOWNLOAD_DIR")
            && let Some(dir) = normalize_user_dir(&env)
        {
            return dir;
        }
        if let Some(dir) = self
            .download_dir
            .as_ref()
            .and_then(|d| normalize_user_dir(&d.to_string_lossy()))
        {
            return dir;
        }
        default_download_dir()
    }

    /// Where saved recordings go. Precedence: `YTM_RECORDING_DIR` env override (tests), then
    /// the configured `recording.track_directory`, then `<music>/yututui/recordings`.
    pub fn effective_recording_dir(&self) -> PathBuf {
        if let Ok(env) = std::env::var("YTM_RECORDING_DIR")
            && let Some(dir) = normalize_user_dir(&env)
        {
            return dir;
        }
        if let Some(dir) = self
            .recording
            .track_directory
            .as_ref()
            .and_then(|d| normalize_user_dir(&d.to_string_lossy()))
        {
            return dir;
        }
        default_recording_dir()
    }

    /// Minimum kept-track duration in seconds (clamped).
    pub fn effective_recording_min(&self) -> u32 {
        self.recording
            .min_duration_secs
            .clamp(RECORDING_MIN_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX)
    }

    /// Maximum track duration before a force-split, in seconds (clamped, always `> min`).
    pub fn effective_recording_max(&self) -> u32 {
        let min = self.effective_recording_min();
        self.recording
            .max_duration_secs
            .clamp(RECORDING_MAX_SECONDS_MIN, RECORDING_MAX_SECONDS_MAX)
            .max(min + 1)
    }

    /// Max recent tracks kept in the recordings browser (clamped).
    pub fn effective_recording_past_tracks(&self) -> usize {
        self.recording
            .past_tracks_count
            .clamp(RECORDING_PAST_TRACKS_MIN, RECORDING_PAST_TRACKS_MAX)
    }

    /// Concurrent downloads, with an env override for quick one-off throttling.
    pub fn effective_download_concurrency(&self) -> usize {
        if let Ok(env) = std::env::var("YTM_DOWNLOAD_CONCURRENCY")
            && let Ok(n) = env.trim().parse::<usize>()
        {
            return n.clamp(DOWNLOAD_CONCURRENCY_MIN, DOWNLOAD_CONCURRENCY_MAX);
        }
        self.download_concurrency
            .unwrap_or(DOWNLOAD_CONCURRENCY_DEFAULT)
            .clamp(DOWNLOAD_CONCURRENCY_MIN, DOWNLOAD_CONCURRENCY_MAX)
    }

    /// Whether to capture the mouse (buttons and click-to-seek). Enabled unless set to `false`.
    pub fn effective_mouse(&self) -> bool {
        self.mouse.unwrap_or(true)
    }

    /// Whether to show album art / thumbnails in the player view (default off; opt-in).
    pub fn effective_album_art(&self) -> bool {
        self.album_art.unwrap_or(false)
    }

    /// Where the player control block sits (default `Bottom`, the docked layout).
    pub fn effective_player_bar_position(&self) -> PlayerBarPosition {
        self.player_bar_position.unwrap_or_default()
    }

    /// Whether the docked control box is collapsed on non-Player screens (default false).
    pub fn control_box_collapsed(&self) -> bool {
        self.control_box_collapsed.unwrap_or(false)
    }

    /// Whether to publish playback to the OS media session (macOS Now Playing,
    /// Windows SMTC, Linux MPRIS) and accept media keys / widget control (default on).
    pub fn effective_media_controls(&self) -> bool {
        self.media_controls.unwrap_or(true)
    }

    /// Whether local files scrobble too (when they carry title + artist metadata; default on).
    pub fn effective_scrobble_local_files(&self) -> bool {
        self.scrobble.local_files.unwrap_or(true)
    }

    /// The Spotify loopback-redirect port (must match the registered redirect URI).
    pub fn effective_spotify_port(&self) -> u16 {
        self.spotify
            .redirect_port
            .unwrap_or(SPOTIFY_REDIRECT_PORT_DEFAULT)
    }

    /// The runtime snapshot handed to the scrobble actor. App credentials resolve from
    /// the embedded pair with config overrides; the session requires those credentials
    /// plus a connected, enabled account.
    pub fn scrobble_settings(&self) -> crate::scrobble::ScrobbleSettings {
        let lastfm = &self.scrobble.lastfm;
        let (api_key, api_secret) = crate::scrobble::lastfm::app_credentials(
            lastfm.api_key.as_deref(),
            lastfm.api_secret.as_deref(),
        );
        let lastfm_app =
            (!api_key.is_empty() && !api_secret.is_empty()).then_some(crate::scrobble::LastfmApp {
                api_key,
                api_secret,
            });
        crate::scrobble::ScrobbleSettings {
            lastfm: (lastfm_app.is_some() && lastfm.is_active()).then(|| {
                crate::scrobble::LastfmSession {
                    session_key: lastfm.session_key.clone().unwrap_or_default(),
                    love_sync: lastfm.love_sync.unwrap_or(true),
                }
            }),
            lastfm_app,
            listenbrainz: self.scrobble.listenbrainz.is_active().then(|| {
                crate::scrobble::ListenBrainzSession {
                    token: self.scrobble.listenbrainz.token.clone().unwrap_or_default(),
                    api_url: self
                        .scrobble
                        .listenbrainz
                        .api_url
                        .clone()
                        .filter(|u| !u.trim().is_empty())
                        .unwrap_or_else(|| {
                            crate::scrobble::listenbrainz::DEFAULT_API_URL.to_owned()
                        }),
                }
            }),
            local_files: self.effective_scrobble_local_files(),
        }
    }

    /// The ten band gains to apply: the hand-tuned array if set, else the preset's.
    pub fn effective_eq_bands(&self) -> [f64; eq::BANDS] {
        // Clamp every band to a finite in-range gain so a corrupt/hand-edited config can't
        // feed `g=NaN` into the mpv filter or a non-finite gain into the wire settings model
        // (which would fail JSON serialization). A valid whole-dB gain is unchanged.
        let bands = self.eq_bands.unwrap_or_else(|| self.eq_preset.gains());
        std::array::from_fn(|i| eq::clamp_band(bands[i]))
    }

    /// Whether loudness normalization is on (default off).
    pub fn effective_normalize(&self) -> bool {
        self.normalize.unwrap_or(false)
    }

    /// Playback speed, clamped to the supported range (default 1.0×).
    pub fn effective_speed(&self) -> f64 {
        // Route through the shared clamp so a non-finite / off-tenth persisted speed is
        // normalized identically to every other speed path (finite guard + round + clamp).
        clamp_speed(self.speed.unwrap_or(1.0))
    }

    /// Seek step in seconds, clamped to the supported range (default 10s).
    pub fn effective_seek_seconds(&self) -> f64 {
        clamp_seek_seconds(self.seek_seconds.unwrap_or(SEEK_SECONDS_DEFAULT))
    }

    /// Whether the mouse wheel changes volume over the player volume cluster (default on).
    pub fn effective_mouse_wheel_volume(&self) -> bool {
        self.mouse_wheel_volume.unwrap_or(true)
    }

    /// The persisted text-zoom percent, snapped to the nearest supported level.
    pub fn effective_text_zoom(&self) -> u16 {
        crate::zoom::snap(self.text_zoom.unwrap_or(100))
    }

    /// Whether the Ctrl+wheel zoom gesture is frozen (default unlocked).
    pub fn effective_zoom_wheel_lock(&self) -> bool {
        self.zoom_wheel_lock.unwrap_or(false)
    }

    /// Whether gapless playback is on (default on).
    pub fn effective_gapless(&self) -> bool {
        self.gapless.unwrap_or(true)
    }

    /// Whether queue shuffle is on (default off).
    pub fn effective_shuffle(&self) -> bool {
        self.shuffle.unwrap_or(false)
    }

    /// The repeat mode (default off).
    pub fn effective_repeat(&self) -> Repeat {
        self.repeat
    }

    /// Whether manual enqueue inserts tracks immediately after the current track (default off).
    pub fn effective_enqueue_next(&self) -> bool {
        self.enqueue_next.unwrap_or(false)
    }

    /// Whether queue auto-extend streaming is on (default off).
    pub fn effective_autoplay_streaming(&self) -> bool {
        self.autoplay_streaming.unwrap_or(false)
    }

    /// Whether to auto-play the restored track as soon as the app launches (default off).
    pub fn effective_autoplay_on_start(&self) -> bool {
        self.autoplay_on_start.unwrap_or(false)
    }

    /// Whether the video overlay auto-continues into the next track's video (default off).
    pub fn effective_auto_continue_videos(&self) -> bool {
        self.auto_continue_videos.unwrap_or(false)
    }

    /// Search provider settings with a valid selected source.
    pub fn effective_search(&self) -> SearchConfig {
        self.search.clone().normalized()
    }

    /// The Gemini API key to use. The `GEMINI_API_KEY` env var wins over the config
    /// value; whitespace is trimmed and an empty result is treated as unset (`None`).
    pub fn effective_gemini_api_key(&self) -> Option<String> {
        if let Ok(env) = std::env::var("GEMINI_API_KEY") {
            let trimmed = env.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        self.gemini_api_key
            .as_ref()
            .map(|k| k.trim().to_owned())
            .filter(|k| !k.is_empty())
    }

    /// The Gemini model the assistant uses.
    pub fn effective_gemini_model(&self) -> GeminiModel {
        self.gemini_model
    }

    /// Whether the DJ Gem assistant is enabled (default on). When off, [`Self::effective_ai_key`]
    /// reports no key, so the assistant stays torn down even if a key is configured.
    pub fn effective_ai_enabled(&self) -> bool {
        self.ai_enabled.unwrap_or(true)
    }

    /// The DJ Gem key to actually use: the effective Gemini key, but only while DJ Gem is enabled.
    /// `None` when DJ Gem is switched off — the lever the settings toggle pulls to disable DJ Gem
    /// without discarding the saved key.
    pub fn effective_ai_key(&self) -> Option<String> {
        if self.effective_ai_enabled() {
            self.effective_gemini_api_key()
        } else {
            None
        }
    }

    /// Whether CJK titles/artists should be shown with Latin-script display overlays.
    pub fn effective_romanized_titles(&self) -> bool {
        self.romanized_titles.unwrap_or(false)
    }

    /// The Gemini key needed by any Gemini-backed feature. DJ Gem can be off while title
    /// romanization remains on, so actor lifetime is gated by this broader service key.
    pub fn effective_ai_service_key(&self) -> Option<String> {
        if self.effective_ai_enabled() || self.effective_romanized_titles() {
            self.effective_gemini_api_key()
        } else {
            None
        }
    }

    /// The normalized theme config to apply at runtime. Retro mode no longer forces the
    /// Retro preset here: enabling it seeds the theme once (see
    /// `settings_toggle_retro_mode`), after which preset and colors stay user-editable —
    /// retro keeps only the English UI + ASCII scrub guarantees.
    pub fn effective_theme(&self) -> ThemeConfig {
        self.theme.normalized()
    }

    /// The persisted dedicated-radio-mode theme, normalized. `None` when the user has
    /// never saved a theme inside radio mode.
    pub fn effective_radio_theme(&self) -> Option<ThemeConfig> {
        self.radio_theme.as_ref().map(ThemeConfig::normalized)
    }

    /// The normalized dedicated-Local-Deck theme. Unlike the persisted optional slot, the
    /// runtime value always exists: fresh and legacy configs both enter Local with Local Launch.
    pub fn effective_local_theme(&self) -> ThemeConfig {
        self.local_theme
            .as_ref()
            .map(ThemeConfig::normalized)
            .unwrap_or_else(ThemeConfig::local_launch)
    }

    /// The UI language to apply at runtime (default English).
    pub fn effective_language(&self) -> Language {
        if self.retro_mode {
            Language::English
        } else {
            self.language
        }
    }

    /// Whether Linux basic TTY compatibility mode is active (default off).
    pub fn effective_retro_mode(&self) -> bool {
        self.retro_mode
    }

    /// The DJ Gem reply language to apply at runtime, resolved from the raw setting: retro mode
    /// forces English; `Auto` follows the UI language (Korean UI → Korean, otherwise left as
    /// `Auto` so the model replies in the user's own language); a concrete choice is used as-is.
    /// The resolved value is pushed to [`crate::i18n::set_dj_gem_language`] at startup and on save.
    pub fn effective_dj_gem_language(&self) -> DjGemLanguage {
        if self.retro_mode {
            return DjGemLanguage::English;
        }
        match self.dj_gem_language {
            DjGemLanguage::Auto if self.effective_language() == Language::Korean => {
                DjGemLanguage::Korean
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod cache_policy_tests;
#[cfg(test)]
mod hardening_tests;

#[cfg(test)]
mod tests;
