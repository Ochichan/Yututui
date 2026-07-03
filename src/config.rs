//! Persistent configuration: cross-platform paths, atomic save, and a one-time import
//! of the old `~/.youtube-music-cli/config.json`.
//!
//! Auth: either an inline `cookie` (raw `Cookie:` header) or a `cookies_file` pointing
//! at a Netscape `cookies.txt`. If no file is configured, `~/Music/ytm-tui/cookies.txt`
//! (the platform music folder) is tried. The header form feeds ytmapi-rs; the file is
//! also handed to mpv/yt-dlp so they own stream resolution (PO tokens, throttling).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

use crate::ai::GeminiModel;
use crate::eq::{self, EqPreset};
use crate::i18n::Language;
use crate::queue::Repeat;
use crate::search_source::SearchConfig;
use crate::streaming::StreamingConfig;
use crate::t;
use crate::theme::ThemeConfig;

/// Clamp range for playback speed (matches the `>`/`<` controls and the settings slider).
pub const SPEED_MIN: f64 = 0.5;
pub const SPEED_MAX: f64 = 2.0;

/// Clamp range for the seek step (seconds) used by the seek-back/-forward keys, exposed as
/// a slider on the Playback settings tab. Default is 10s.
pub const SEEK_SECONDS_MIN: f64 = 1.0;
pub const SEEK_SECONDS_MAX: f64 = 60.0;
pub const SEEK_SECONDS_DEFAULT: f64 = 10.0;

/// Concurrent `yt-dlp`/ffmpeg downloads. Keep the default conservative because each download
/// can spawn multiple external processes and dominate CPU/RAM outside this Rust process.
pub const DOWNLOAD_CONCURRENCY_MIN: usize = 1;
pub const DOWNLOAD_CONCURRENCY_MAX: usize = 3;
pub const DOWNLOAD_CONCURRENCY_DEFAULT: usize = 2;

/// UI eye-candy toggles (the **Animations** settings tab). Every field is an
/// independent on/off; **all default to `false`** so a fresh install behaves exactly like
/// before (the app's whole identity is "fast and light"). `master` is a global kill-switch:
/// when it is off, nothing animates regardless of the per-effect flags, and the animation
/// frame-clock never even wakes (see `App::animation_active`). Grouped under one JSON key
/// (`"animations"`) and `#[serde(default)]` so older config files forward-migrate cleanly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AnimationsConfig {
    /// Global enable. Off → the player renders identically to today, zero overhead.
    pub master: bool,
    // Element-level effects (restyle existing widgets in place) -----------------
    /// Shimmer + marquee scroll on the now-playing title line.
    pub title: bool,
    /// Pulse the `♥` like-marker when the track is in the library.
    pub heart: bool,
    /// Seekbar motion: the sweeping comet on the filled gauge plus smooth sub-second fill
    /// (the gauge interpolates between mpv's one-per-second reports while the clock runs).
    pub seekbar: bool,
    /// Spinning throbber next to "▸ playing" on the status line.
    pub spinner: bool,
    /// Faux VU `▁▂▃▅▇` bars on the status line (and a mini VU marker on the queue window's
    /// now-playing row).
    pub eq_bars: bool,
    /// Pulse/glow the transport play-pause glyph.
    pub controls: bool,
    /// "Breathing" outer border colour cycle.
    pub border: bool,
    // Player one-shots (event feedback, each plays once and the clock re-sleeps) -
    /// Letter-cascade reveal of the title line when a new track starts.
    pub track_intro: bool,
    /// Synced-lyrics polish: the current line breathes and flashes as it becomes current;
    /// far lines fade with distance.
    pub lyrics: bool,
    /// Transient status messages type themselves in with a bright caret head.
    pub toast: bool,
    /// A short volume gauge flashes under the transport strip when the volume changes.
    pub volume_flash: bool,
    /// A little burst of hearts/sparks around the title when the track is liked.
    pub like_burst: bool,
    /// A bright ripple at the seekbar head after a seek.
    pub seek_flash: bool,
    // UI-wide effects (Search / Library / Settings / DJ Gem, not just the player) -
    /// The focused list selection bar breathes gently toward the accent colour.
    pub selection: bool,
    /// List rows cascade in top-to-bottom on view/tab switches and new search results.
    pub stagger: bool,
    /// Text-input carets blink (search box, filter, playlist names, settings fields, DJ Gem).
    pub caret: bool,
    /// The active tab pops with a brief accent wash on view/tab switches.
    pub tabs: bool,
    /// Popups and dropdowns materialize with a ~150 ms fade-in instead of appearing at once.
    pub popup_fade: bool,
    /// Activity indicators animate: "Searching…" dots, lyrics fetching, DJ Gem "…thinking",
    /// and a spinner on a running download's `⬇ N%` tag.
    pub activity: bool,
    /// The About card twinkles: sparkles around the icon and a gradient sweep on the name.
    pub about_fx: bool,
    // Filler-canvas effects (drawn only in blank zones) ------------------------
    /// Matrix-style digital rain in the free zone(s).
    pub rain: bool,
    /// Classic spinning ASCII donut.
    pub donut: bool,
    /// Decorative (non-audio-reactive) spectrum visualizer.
    pub visualizer: bool,
    /// Drifting stars / musical notes.
    pub starfield: bool,
    /// DVD-style bouncing logo.
    pub bounce: bool,
    // Behaviour knobs (not effects) -------------------------------------------
    /// Park the animation tick while the terminal is unfocused (minimized or behind another
    /// window). Defaults to `true`; opt out to keep animating off-screen. No-op on terminals that
    /// don't report focus (DECSET ?1004). See [`crate::app::App::animation_active`].
    pub pause_unfocused: bool,
    /// Target animation frame rate. Read through [`Self::effective_fps`], which clamps to
    /// [`FPS_MIN`]..=[`FPS_MAX`] so a hand-edited or corrupt config can't spin the loop or freeze
    /// it. Lower values trade smoothness for less CPU/battery. Default [`FPS_DEFAULT`].
    pub fps: u16,
}

/// Animation frame-rate bounds. The floor keeps motion perceptible; the ceiling caps the
/// redraw cost. The default matches the long-standing fixed ~30 fps tick.
pub const FPS_MIN: u16 = 5;
pub const FPS_MAX: u16 = 60;
pub const FPS_DEFAULT: u16 = 30;

impl Default for AnimationsConfig {
    /// All visual effects start **off** (reduced-motion by default); the behaviour knobs default
    /// to `pause_unfocused: true` and `fps: 30`. A manual impl (rather than `#[derive(Default)]`)
    /// is required so these aren't `bool`'s `false` / `u16`'s `0` (a `0` fps would clamp to the
    /// floor and silently override the intended default).
    fn default() -> Self {
        Self {
            master: false,
            title: false,
            heart: false,
            seekbar: false,
            spinner: false,
            eq_bars: false,
            controls: false,
            border: false,
            track_intro: false,
            lyrics: false,
            toast: false,
            volume_flash: false,
            like_burst: false,
            seek_flash: false,
            selection: false,
            stagger: false,
            caret: false,
            tabs: false,
            popup_fade: false,
            activity: false,
            about_fx: false,
            rain: false,
            donut: false,
            visualizer: false,
            starfield: false,
            bounce: false,
            pause_unfocused: true,
            fps: FPS_DEFAULT,
        }
    }
}

impl AnimationsConfig {
    /// The frame rate to actually drive the tick at, clamped to a sane range so a bad config
    /// value (0, or absurdly high) can't busy-spin or stall the animation loop.
    pub fn effective_fps(&self) -> u16 {
        self.fps.clamp(FPS_MIN, FPS_MAX)
    }

    /// Whether any individual effect is enabled (ignores `master`).
    pub fn any_effect(&self) -> bool {
        self.title
            || self.heart
            || self.seekbar
            || self.spinner
            || self.eq_bars
            || self.controls
            || self.border
            || self.track_intro
            || self.lyrics
            || self.toast
            || self.volume_flash
            || self.like_burst
            || self.seek_flash
            || self.selection
            || self.stagger
            || self.caret
            || self.tabs
            || self.popup_fade
            || self.activity
            || self.about_fx
            || self.rain
            || self.donut
            || self.visualizer
            || self.starfield
            || self.bounce
    }

    /// Whether animations should actually run: the master switch is on *and* at least one
    /// effect is enabled. When this is `false`, the per-frame animation clock stays asleep.
    pub fn active(&self) -> bool {
        self.master && self.any_effect()
    }
}

/// Window layout for the external mpv video overlay launched from the player (`v`), toggled
/// live with `Shift+V`. `Compact` docks a small ~30% window top-right; `Large` centers a
/// ~50% window. Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoOverlay {
    #[default]
    Compact,
    Large,
}

impl VideoOverlay {
    /// The other layout (for the `Shift+V` toggle).
    pub fn toggled(self) -> Self {
        match self {
            Self::Compact => Self::Large,
            Self::Large => Self::Compact,
        }
    }

    /// Short human label for the status toast.
    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => t!("top-right · 30%", "우상단 · 30%"),
            Self::Large => t!("center · 50%", "가운데 · 50%"),
        }
    }

    /// mpv flags for a borderless, always-on-top overlay window. `Compact` docks top-right;
    /// `Large` is left at mpv's default (centered) position, half size.
    pub fn mpv_window_args(self) -> Vec<String> {
        let mut args = vec!["--ontop".to_owned(), "--no-border".to_owned()];
        match self {
            Self::Compact => {
                args.push("--autofit=30%".to_owned());
                args.push("--geometry=-20+20".to_owned());
            }
            Self::Large => args.push("--autofit=50%".to_owned()),
        }
        args
    }
}

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
}

pub const SPOTIFY_REDIRECT_PORT_DEFAULT: u16 = 9271;

// No `Debug`: this holds secrets (`cookie` raw header, `gemini_api_key`, the scrobbling
// session key/token), so a stray `{:?}` must not be able to leak them — same reason
// `GeminiClient` omits it.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Raw `Cookie:` header for music.youtube.com (takes precedence over the file).
    pub cookie: Option<String>,
    /// Path to a Netscape `cookies.txt` exported from the browser.
    /// `None` -> `<user music dir>/ytm-tui/cookies.txt`.
    pub cookies_file: Option<PathBuf>,
    /// Startup volume, 0-100.
    pub volume: i64,
    /// Where downloads are saved. `None` -> `<user music dir>/ytm-tui`.
    pub download_dir: Option<PathBuf>,
    /// Most simultaneous downloads. `None` -> [`DOWNLOAD_CONCURRENCY_DEFAULT`].
    pub download_concurrency: Option<usize>,
    /// Capture the mouse for buttons and click-to-seek. `None` → enabled.
    pub mouse: Option<bool>,
    /// Show album art / video thumbnail in the player view. `None` → off (opt-in: the
    /// terminal is only probed for a graphics protocol when this is on, and turning it on
    /// takes effect at the next launch). See [`crate::artwork`].
    pub album_art: Option<bool>,

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

    // Theme -------------------------------------------------------------------
    /// Color theme preset plus per-role `#RRGGBB` overrides.
    pub theme: ThemeConfig,
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
}

#[derive(Clone)]
pub struct PlayerRuntimeConfig {
    pub volume: i64,
    pub cookies_file: Option<PathBuf>,
    pub gapless: bool,
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
            cookie: None,
            cookies_file: None,
            volume: 100,
            download_dir: None,
            download_concurrency: None,
            mouse: None,
            album_art: None,
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
            search: SearchConfig::default(),
            streaming: StreamingConfig::default(),
            animations: AnimationsConfig::default(),
            gemini_api_key: None,
            gemini_model: GeminiModel::default(),
            ai_enabled: None,
            romanized_titles: None,
            theme: ThemeConfig::default(),
            retro_mode: false,
            language: Language::default(),
            keybindings: std::collections::BTreeMap::new(),
            video_layout: VideoOverlay::default(),
            media_controls: None,
            scrobble: ScrobbleConfig::default(),
            spotify: SpotifyConfig::default(),
        }
    }
}

impl Config {
    /// Load config, importing from the old app on first run. Never fails: a missing or
    /// corrupt file falls back to defaults (+ migration).
    pub fn load() -> Self {
        if let Some(path) = config_path()
            && let Ok(text) = safe_fs::read_to_string_no_symlink(&path)
            && let Ok(cfg) = serde_json::from_str::<Config>(&text)
        {
            return cfg;
        }
        // First run (or unreadable): migrate from the old app, then persist.
        let mut cfg = Config::default();
        if let Some(old) = old_config_path() {
            import_old_from(&old, &mut cfg);
        }
        let _ = cfg.save();
        cfg
    }

    /// Persist atomically (write a temp file, then rename over the target).
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = config_path() else {
            return Ok(()); // no config dir on this platform; nothing to do
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    pub fn player_runtime(&self, cookies_file: Option<PathBuf>) -> PlayerRuntimeConfig {
        PlayerRuntimeConfig {
            volume: self.volume,
            cookies_file,
            gapless: self.effective_gapless(),
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
            && let Ok(content) = fs::read_to_string(file)
        {
            let header = parse_netscape_cookies(&content);
            if !header.is_empty() {
                return Some(header);
            }
        }
        None
    }

    /// The cookies file to try. An explicit setting wins; otherwise the cross-platform
    /// default is `<user music dir>/ytm-tui/cookies.txt`.
    pub fn effective_cookies_file(&self) -> Option<PathBuf> {
        self.cookies_file.clone().or_else(default_cookies_file)
    }

    /// The concrete directory downloads are saved to. Precedence: `YTM_DOWNLOAD_DIR`
    /// env override, then the configured `download_dir`, then `<user music dir>/ytm-tui`.
    /// The music folder resolves per-OS (`~/Music` on macOS, the Music known-folder on
    /// Windows) via the `directories` crate.
    pub fn effective_download_dir(&self) -> PathBuf {
        if let Ok(env) = std::env::var("YTM_DOWNLOAD_DIR")
            && !env.is_empty()
        {
            return PathBuf::from(env);
        }
        if let Some(dir) = &self.download_dir {
            return dir.clone();
        }
        default_download_dir()
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
        self.eq_bands.unwrap_or_else(|| self.eq_preset.gains())
    }

    /// Whether loudness normalization is on (default off).
    pub fn effective_normalize(&self) -> bool {
        self.normalize.unwrap_or(false)
    }

    /// Playback speed, clamped to the supported range (default 1.0×).
    pub fn effective_speed(&self) -> f64 {
        self.speed.unwrap_or(1.0).clamp(SPEED_MIN, SPEED_MAX)
    }

    /// Seek step in seconds, clamped to the supported range (default 10s).
    pub fn effective_seek_seconds(&self) -> f64 {
        self.seek_seconds
            .unwrap_or(SEEK_SECONDS_DEFAULT)
            .clamp(SEEK_SECONDS_MIN, SEEK_SECONDS_MAX)
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
}

/// Default location for an optional exported Netscape cookies file.
///
/// macOS: `~/Music/ytm-tui/cookies.txt`
/// Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`
pub fn default_cookies_file() -> Option<PathBuf> {
    default_ytm_dir().map(|dir| dir.join("cookies.txt"))
}

/// Default directory for downloaded tracks.
///
/// macOS: `~/Music/ytm-tui`
/// Windows: `%USERPROFILE%\Music\ytm-tui`
pub fn default_download_dir() -> PathBuf {
    default_ytm_dir().unwrap_or_else(|| PathBuf::from("ytm-tui"))
}

fn default_ytm_dir() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.audio_dir().map(std::path::Path::to_path_buf))
        .map(ytm_dir_under_audio_dir)
}

fn ytm_dir_under_audio_dir(audio_dir: PathBuf) -> PathBuf {
    audio_dir.join("ytm-tui")
}

fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.config_dir().join("config.json"))
}

fn old_config_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".youtube-music-cli/config.json"))
}

/// Pull whatever we can reuse out of the old TypeScript app's config. Favorites,
/// history and playlists are migrated later (M5) when the library view consumes them.
fn import_old_from(path: &std::path::Path, cfg: &mut Config) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(v) = json.get("volume").and_then(serde_json::Value::as_i64) {
        cfg.volume = v.clamp(0, 100);
    }
    if let Some(c) = json
        .pointer("/youtubeMusic/cookie")
        .and_then(serde_json::Value::as_str)
        && !c.is_empty()
    {
        cfg.cookie = Some(c.to_owned());
    }
    if let Some(d) = json
        .get("downloadDirectory")
        .and_then(serde_json::Value::as_str)
        && !d.is_empty()
    {
        cfg.download_dir = Some(PathBuf::from(d));
    }
}

/// Turn a Netscape `cookies.txt` body into a `name=value; ...` header, keeping only
/// youtube.com cookies (where the YTM auth cookies — SAPISID etc. — live).
fn parse_netscape_cookies(content: &str) -> String {
    let mut pairs = Vec::new();
    for raw in content.lines() {
        // `#HttpOnly_` lines are real cookies; any other leading `#` is a comment.
        let line = raw.strip_prefix("#HttpOnly_").unwrap_or(raw);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 {
            continue;
        }
        if !fields[0].contains("youtube.com") {
            continue;
        }
        let (name, value) = (fields[5].trim(), fields[6].trim());
        if !name.is_empty() {
            pairs.push(format!("{name}={value}"));
        }
    }
    pairs.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_full_volume() {
        let c = Config::default();
        assert_eq!(c.volume, 100);
        assert!(c.cookie.is_none());
    }

    #[test]
    fn json_round_trips() {
        let mut theme = ThemeConfig::default();
        theme.set_preset(crate::theme::ThemePreset::Midnight);
        theme
            .set_override(crate::theme::ThemeRole::BorderPrimary, "#123456")
            .unwrap();
        let c = Config {
            cookie: Some("SID=abc".to_owned()),
            cookies_file: Some(PathBuf::from("/tmp/cookies.txt")),
            volume: 70,
            download_dir: Some(PathBuf::from("/tmp/dl")),
            download_concurrency: Some(2),
            mouse: Some(false),
            album_art: Some(true),
            eq_preset: EqPreset::BassBoost,
            eq_bands: Some([1.0; eq::BANDS]),
            normalize: Some(true),
            speed: Some(1.5),
            seek_seconds: Some(15.0),
            mouse_wheel_volume: Some(false),
            text_zoom: Some(150),
            zoom_wheel_lock: Some(true),
            gapless: Some(false),
            shuffle: Some(true),
            repeat: Repeat::One,
            enqueue_next: Some(true),
            autoplay_streaming: Some(true),
            autoplay_on_start: Some(true),
            search: SearchConfig::default(),
            streaming: StreamingConfig::default(),
            animations: AnimationsConfig {
                master: true,
                rain: true,
                ..Default::default()
            },
            gemini_api_key: Some("AIzaSecret".to_owned()),
            gemini_model: GeminiModel::Latest,
            ai_enabled: Some(false),
            romanized_titles: Some(true),
            theme,
            retro_mode: true,
            language: Language::Korean,
            keybindings: std::collections::BTreeMap::new(),
            video_layout: VideoOverlay::Large,
            media_controls: Some(false),
            scrobble: ScrobbleConfig {
                lastfm: LastfmConfig {
                    enabled: Some(true),
                    session_key: Some("sk-123".to_owned()),
                    username: Some("listener".to_owned()),
                    love_sync: Some(false),
                    api_key: None,
                    api_secret: None,
                },
                listenbrainz: ListenBrainzConfig {
                    enabled: None,
                    token: Some("lb-token".to_owned()),
                    api_url: None,
                },
                local_files: Some(false),
            },
            spotify: SpotifyConfig {
                client_id: Some("spotify-app-id".to_owned()),
                redirect_port: Some(9333),
                market: Some("KR".to_owned()),
            },
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.volume, 70);
        assert_eq!(back.cookie.as_deref(), Some("SID=abc"));
        assert_eq!(back.download_dir, Some(PathBuf::from("/tmp/dl")));
        assert_eq!(back.mouse, Some(false));
        assert_eq!(back.album_art, Some(true));
        assert_eq!(back.eq_preset, EqPreset::BassBoost);
        assert_eq!(back.eq_bands, Some([1.0; eq::BANDS]));
        assert_eq!(back.normalize, Some(true));
        assert_eq!(back.speed, Some(1.5));
        assert_eq!(back.seek_seconds, Some(15.0));
        assert_eq!(back.mouse_wheel_volume, Some(false));
        assert_eq!(back.gapless, Some(false));
        assert_eq!(back.shuffle, Some(true));
        assert_eq!(back.repeat, Repeat::One);
        assert_eq!(back.enqueue_next, Some(true));
        assert_eq!(back.autoplay_streaming, Some(true));
        assert_eq!(back.autoplay_on_start, Some(true));
        assert_eq!(back.ai_enabled, Some(false));
        assert_eq!(back.romanized_titles, Some(true));
        assert!(back.animations.master);
        assert!(back.animations.rain);
        assert!(!back.animations.donut);
        assert_eq!(back.gemini_api_key.as_deref(), Some("AIzaSecret"));
        assert_eq!(back.gemini_model, GeminiModel::Latest);
        assert!(back.retro_mode);
        assert_eq!(back.video_layout, VideoOverlay::Large);
        assert_eq!(back.media_controls, Some(false));
        assert_eq!(back.scrobble.lastfm.session_key.as_deref(), Some("sk-123"));
        assert_eq!(back.scrobble.lastfm.username.as_deref(), Some("listener"));
        assert_eq!(back.scrobble.lastfm.love_sync, Some(false));
        assert_eq!(
            back.scrobble.listenbrainz.token.as_deref(),
            Some("lb-token")
        );
        assert_eq!(back.scrobble.local_files, Some(false));
        assert!(back.scrobble.lastfm.is_active());
        assert_eq!(back.spotify.client_id.as_deref(), Some("spotify-app-id"));
        assert_eq!(back.effective_spotify_port(), 9333);
        assert_eq!(back.theme.preset, "midnight");
        assert_eq!(
            back.theme
                .overrides
                .get("border_primary")
                .map(String::as_str),
            Some("#123456")
        );
    }

    #[test]
    fn keybindings_persist_through_config_json() {
        use crate::keymap::{Action, KeyContext, KeyMap, parse_chord};

        // Rebind a key, then capture it the way `close_settings` does on save.
        let mut km = KeyMap::default();
        km.rebind(
            KeyContext::Player,
            Action::TogglePause,
            parse_chord("x").unwrap(),
        )
        .unwrap();
        let cfg = Config {
            keybindings: km.to_overrides(),
            ..Config::default()
        };
        // Only the diff from defaults is persisted.
        assert_eq!(
            cfg.keybindings
                .get("player.toggle_pause")
                .map(String::as_str),
            Some("x")
        );

        // Round-trip through the exact serde path `Config::save`/`load` use (write JSON,
        // read it back) — proving the override survives a restart.
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();

        // On next launch the persisted override rebuilds into the live keymap.
        let restored = KeyMap::from_config(&back);
        assert_eq!(
            restored.action(KeyContext::Player, parse_chord("x").unwrap()),
            Some(Action::TogglePause)
        );
        assert_eq!(
            restored.action(KeyContext::Player, parse_chord("space").unwrap()),
            None
        );
    }

    #[test]
    fn gemini_key_env_overrides_config() {
        let cfg = Config {
            gemini_api_key: Some("from_config".to_owned()),
            ..Config::default()
        };
        // SAFETY: single-threaded test; set+unset around the calls.
        unsafe { std::env::set_var("GEMINI_API_KEY", "  from_env  ") };
        assert_eq!(cfg.effective_gemini_api_key().as_deref(), Some("from_env"));
        unsafe { std::env::remove_var("GEMINI_API_KEY") };
        assert_eq!(
            cfg.effective_gemini_api_key().as_deref(),
            Some("from_config")
        );

        // Empty/whitespace key reads as unset.
        let blank = Config {
            gemini_api_key: Some("   ".to_owned()),
            ..Config::default()
        };
        assert_eq!(blank.effective_gemini_api_key(), None);
    }

    #[test]
    fn ai_off_switch_gates_the_key_without_discarding_it() {
        // DJ Gem explicitly off: the key stays in config, but the *effective* key the assistant
        // spawns from is None — so DJ Gem stays down even with a key saved. (None regardless of any
        // `GEMINI_API_KEY` env var, since the disabled branch never consults the env.)
        let off = Config {
            gemini_api_key: Some("AIzaSaved".to_owned()),
            ai_enabled: Some(false),
            ..Config::default()
        };
        assert_eq!(off.gemini_api_key.as_deref(), Some("AIzaSaved")); // key retained
        assert!(!off.effective_ai_enabled());
        assert_eq!(off.effective_ai_key(), None); // but gated off

        // Enabled (or the default unset → on) passes the effective key straight through. Asserts
        // the *relationship* rather than a literal, so a concurrently-set env var can't flake it.
        let on = Config {
            gemini_api_key: Some("AIzaSaved".to_owned()),
            ai_enabled: Some(true),
            ..Config::default()
        };
        assert!(on.effective_ai_enabled());
        assert_eq!(on.effective_ai_key(), on.effective_gemini_api_key());

        let default_on = Config {
            ai_enabled: None,
            ..Config::default()
        };
        assert!(default_on.effective_ai_enabled()); // unset defaults to on
        assert_eq!(
            default_on.effective_ai_key(),
            default_on.effective_gemini_api_key()
        );
    }

    #[test]
    fn playback_effective_defaults_and_overrides() {
        let d = Config::default();
        assert_eq!(d.effective_eq_bands(), [0.0; eq::BANDS]);
        assert!(!d.effective_normalize());
        assert_eq!(d.effective_speed(), 1.0);
        assert_eq!(d.effective_seek_seconds(), SEEK_SECONDS_DEFAULT);
        assert!(d.effective_mouse_wheel_volume());
        assert!(d.effective_gapless());
        assert!(!d.effective_shuffle());
        assert_eq!(d.effective_repeat(), Repeat::Off);
        assert!(!d.effective_enqueue_next());
        assert!(!d.effective_autoplay_streaming());
        assert!(!d.effective_autoplay_on_start());

        // Preset gains feed through when no hand-tuned bands are set.
        let preset = Config {
            eq_preset: EqPreset::BassBoost,
            ..Config::default()
        };
        assert_eq!(preset.effective_eq_bands(), EqPreset::BassBoost.gains());

        // Speed is clamped to the supported range.
        let fast = Config {
            speed: Some(9.0),
            ..Config::default()
        };
        assert_eq!(fast.effective_speed(), SPEED_MAX);

        // Seek step is clamped to its supported range too.
        let big = Config {
            seek_seconds: Some(999.0),
            ..Config::default()
        };
        assert_eq!(big.effective_seek_seconds(), SEEK_SECONDS_MAX);
        let tiny = Config {
            seek_seconds: Some(0.0),
            ..Config::default()
        };
        assert_eq!(tiny.effective_seek_seconds(), SEEK_SECONDS_MIN);

        let wheel_off = Config {
            mouse_wheel_volume: Some(false),
            ..Config::default()
        };
        assert!(!wheel_off.effective_mouse_wheel_volume());

        let enqueue_next = Config {
            enqueue_next: Some(true),
            ..Config::default()
        };
        assert!(enqueue_next.effective_enqueue_next());
    }

    #[test]
    fn mouse_enabled_by_default_and_overridable() {
        assert!(Config::default().effective_mouse());
        let off = Config {
            mouse: Some(false),
            ..Config::default()
        };
        assert!(!off.effective_mouse());
    }

    #[test]
    fn album_art_off_by_default_and_overridable() {
        assert!(!Config::default().effective_album_art()); // opt-in
        let on = Config {
            album_art: Some(true),
            ..Config::default()
        };
        assert!(on.effective_album_art());
    }

    #[test]
    fn missing_fields_use_defaults() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(back.volume, 100);
    }

    #[test]
    fn animations_off_by_default_and_active_logic() {
        let a = AnimationsConfig::default();
        assert!(!a.master);
        assert!(!a.any_effect());
        assert!(!a.active());

        // An effect on but master off → inactive (global kill-switch wins).
        let effect_only = AnimationsConfig {
            rain: true,
            ..Default::default()
        };
        assert!(effect_only.any_effect());
        assert!(!effect_only.active());

        // The UI-wide effects count as effects too — master + only `caret` (or `toast`)
        // must wake the clock, or the new toggles would silently never run.
        let ui_only = AnimationsConfig {
            master: true,
            caret: true,
            ..Default::default()
        };
        assert!(ui_only.any_effect());
        assert!(ui_only.active());
        let toast_only = AnimationsConfig {
            master: true,
            toast: true,
            ..Default::default()
        };
        assert!(toast_only.active());

        // Master on but no effect → still inactive (nothing to draw).
        let master_only = AnimationsConfig {
            master: true,
            ..Default::default()
        };
        assert!(!master_only.active());

        // Master + an effect → active.
        let on = AnimationsConfig {
            master: true,
            donut: true,
            ..Default::default()
        };
        assert!(on.active());

        // A missing "animations" key forward-migrates to all-off.
        let back: Config = serde_json::from_str("{}").unwrap();
        assert!(!back.animations.active());
    }

    #[test]
    fn parses_netscape_cookies_youtube_only() {
        let txt = "# Netscape HTTP Cookie File\n\
                   .youtube.com\tTRUE\t/\tTRUE\t1999999999\tSAPISID\tsecret1\n\
                   #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1999999999\tSID\tsecret2\n\
                   .example.com\tTRUE\t/\tFALSE\t1999999999\tIGNORED\tnope\n";
        let header = parse_netscape_cookies(txt);
        assert!(header.contains("SAPISID=secret1"));
        assert!(header.contains("SID=secret2"));
        assert!(!header.contains("IGNORED"));
    }

    #[test]
    fn default_cookies_file_lives_under_audio_dir() {
        assert_eq!(
            ytm_dir_under_audio_dir(PathBuf::from("/Users/alice/Music")).join("cookies.txt"),
            PathBuf::from("/Users/alice/Music/ytm-tui/cookies.txt")
        );
    }

    #[test]
    fn default_download_dir_lives_under_audio_dir() {
        assert_eq!(
            ytm_dir_under_audio_dir(PathBuf::from("/Users/alice/Music")),
            PathBuf::from("/Users/alice/Music/ytm-tui")
        );
    }

    #[test]
    fn configured_cookies_file_overrides_default() {
        let cfg = Config {
            cookies_file: Some(PathBuf::from("/custom/cookies.txt")),
            ..Config::default()
        };
        assert_eq!(
            cfg.effective_cookies_file(),
            Some(PathBuf::from("/custom/cookies.txt"))
        );
    }

    #[test]
    fn env_overrides_download_dir() {
        // SAFETY: single-threaded test; we set+unset around the call.
        unsafe { std::env::set_var("YTM_DOWNLOAD_DIR", "/tmp/ytm-dl-test") };
        let dir = Config::default().effective_download_dir();
        unsafe { std::env::remove_var("YTM_DOWNLOAD_DIR") };
        assert_eq!(dir, PathBuf::from("/tmp/ytm-dl-test"));
    }

    #[test]
    fn download_concurrency_defaults_clamps_and_honors_env() {
        let old = std::env::var_os("YTM_DOWNLOAD_CONCURRENCY");
        unsafe { std::env::remove_var("YTM_DOWNLOAD_CONCURRENCY") };

        assert_eq!(
            Config::default().effective_download_concurrency(),
            DOWNLOAD_CONCURRENCY_DEFAULT
        );
        let high = Config {
            download_concurrency: Some(99),
            ..Config::default()
        };
        assert_eq!(
            high.effective_download_concurrency(),
            DOWNLOAD_CONCURRENCY_MAX
        );
        let zero = Config {
            download_concurrency: Some(0),
            ..Config::default()
        };
        assert_eq!(
            zero.effective_download_concurrency(),
            DOWNLOAD_CONCURRENCY_MIN
        );

        unsafe { std::env::set_var("YTM_DOWNLOAD_CONCURRENCY", "99") };
        assert_eq!(
            Config::default().effective_download_concurrency(),
            DOWNLOAD_CONCURRENCY_MAX
        );
        unsafe { std::env::set_var("YTM_DOWNLOAD_CONCURRENCY", "not-a-number") };
        let configured = Config {
            download_concurrency: Some(1),
            ..Config::default()
        };
        assert_eq!(configured.effective_download_concurrency(), 1);

        match old {
            Some(v) => unsafe { std::env::set_var("YTM_DOWNLOAD_CONCURRENCY", v) },
            None => unsafe { std::env::remove_var("YTM_DOWNLOAD_CONCURRENCY") },
        }
    }

    #[test]
    fn imports_old_download_directory() {
        let dir = std::env::temp_dir().join(format!("ytm-old-dl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        fs::write(&p, r#"{"downloadDirectory":"/music/dl"}"#).unwrap();
        let mut cfg = Config::default();
        import_old_from(&p, &mut cfg);
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/music/dl")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn imports_old_volume_and_cookie() {
        let dir = std::env::temp_dir().join(format!("ytm-old-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        fs::write(
            &p,
            r#"{"volume":42,"youtubeMusic":{"cookie":"SID=fromold"}}"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        import_old_from(&p, &mut cfg);
        assert_eq!(cfg.volume, 42);
        assert_eq!(cfg.cookie.as_deref(), Some("SID=fromold"));
        let _ = fs::remove_dir_all(&dir);
    }
}
