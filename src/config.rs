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
use crate::t;
use crate::theme::ThemeConfig;

mod audio;
mod recovery;
mod spotify;
pub use audio::{
    AudioBackend, AudioConfig, AudioRuntimeConfig, MPV_CACHE_BACK_DEFAULT,
    MPV_CACHE_BACK_LEGACY_DEFAULT, MPV_CACHE_DEFAULTS_REVISION, MPV_CACHE_FORWARD_DEFAULT,
    MPV_CACHE_FORWARD_LEGACY_DEFAULT, MpvAudioConfig, MpvAudioRuntimeConfig,
};
pub use spotify::SpotifyImportMode;

/// Clamp range for playback speed (matches the `>`/`<` controls and the settings slider).
pub const SPEED_MIN: f64 = 0.5;
pub const SPEED_MAX: f64 = 2.0;

/// Version of the interactive Beginner Mode walkthrough shipped by this build. Bump this only
/// when the ordered steps or their completion contracts change; copy-only edits must not make a
/// user repeat the tour.
pub const BEGINNER_TUTORIAL_VERSION: u16 = 1;

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
    pub fn welcome() -> Self {
        Self::default()
    }
}

impl Default for BeginnerTutorialProgress {
    fn default() -> Self {
        Self {
            content_version: BEGINNER_TUTORIAL_VERSION,
            next_step: "welcome".to_owned(),
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
    /// Dedicated-Radio-mode override for `master`. `None` inherits `master` (existing
    /// configs keep behaving as one global switch); the first ✨/`A` toggle taken while in
    /// Radio mode pins it, after which the two modes animate independently. A scope
    /// selector, not an effect — deliberately excluded from [`Self::any_effect`]. Resolved
    /// through [`Self::effective`]; there is no Settings row for it.
    pub radio_master: Option<bool>,
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
    // Second-wave Now Playing element effects ----------------------------------
    /// The seekbar's gauge and time label glow briefly as each playback second lands.
    pub time_glow: bool,
    /// Tiny sparks twinkle around the seekbar head while playback runs.
    pub progress_sparkle: bool,
    /// A short bright comet chases clockwise around the Player view's outer border.
    pub border_chase: bool,
    /// A light wave washes across the transport controls when play/pause toggles.
    pub pause_flash: bool,
    /// Error status messages shake side-to-side with a decaying oscillation.
    pub error_shake: bool,
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
    /// Occasional diagonal shooting-star streaks.
    pub comets: bool,
    /// Sparse drifting snowfall.
    pub snow: bool,
    /// Fireflies wandering on smooth glowing paths.
    pub fireflies: bool,
    /// Rotating 3-D wireframe cube.
    pub cube: bool,
    /// ASCII aquarium: fish swimming both ways plus rising bubbles.
    pub aquarium: bool,
    /// Layered ocean waves along the bottom of the free zone.
    pub waves: bool,
    /// Periodic firework launches with radial particle bursts.
    pub fireworks: bool,
    /// Conway's Game of Life colony, colour-coded by cell age.
    pub life: bool,
    /// The classic pipes screensaver growing across the free zone.
    pub pipes: bool,
    /// Demoscene plasma colour field over the whole free zone (the heaviest effect).
    pub plasma: bool,
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
            radio_master: None,
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
            time_glow: false,
            progress_sparkle: false,
            border_chase: false,
            pause_flash: false,
            error_shake: false,
            rain: false,
            donut: false,
            visualizer: false,
            starfield: false,
            bounce: false,
            comets: false,
            snow: false,
            fireflies: false,
            cube: false,
            aquarium: false,
            waves: false,
            fireworks: false,
            life: false,
            pipes: false,
            plasma: false,
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
            || self.time_glow
            || self.progress_sparkle
            || self.border_chase
            || self.pause_flash
            || self.error_shake
            || self.any_canvas()
    }

    /// Whether any filler-canvas effect is enabled — the group `ui::anim::render_canvas`
    /// dispatches, drawn only into blank zones.
    pub fn any_canvas(&self) -> bool {
        self.bounce || self.any_canvas_heavy()
    }

    /// Canvas effects that repaint enough cells per frame to earn the reduced draw-fps cap
    /// and DEC synchronized update: every canvas effect except the single-label `bounce`.
    pub fn any_canvas_heavy(&self) -> bool {
        self.rain
            || self.donut
            || self.visualizer
            || self.starfield
            || self.comets
            || self.snow
            || self.fireflies
            || self.cube
            || self.aquarium
            || self.waves
            || self.fireworks
            || self.life
            || self.pipes
            || self.plasma
    }

    /// Whether animations should actually run: the master switch is on *and* at least one
    /// effect is enabled. When this is `false`, the per-frame animation clock stays asleep.
    pub fn active(&self) -> bool {
        self.master && self.any_effect()
    }

    /// The config as the render/gating layer should see it in the given mode: `master`
    /// resolves to the Radio override while dedicated Radio mode is active (`None` =
    /// inherit). Callers must keep persisting the *stored* config — saving this resolved
    /// copy would bake the inherit link into the file.
    pub fn effective(self, radio: bool) -> Self {
        Self {
            master: if radio {
                self.radio_master.unwrap_or(self.master)
            } else {
                self.master
            },
            ..self
        }
    }
}

/// Source/detail level for remote album art shown inside the terminal. `High` preserves the
/// historical max-resolution preference and 768px cap; `Original` keeps the fetched source
/// dimensions intact. Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlbumArtQuality {
    Standard,
    #[default]
    High,
    Original,
}

impl AlbumArtQuality {
    /// Step through `Standard → High → Original`, wrapping in either direction.
    pub fn cycled(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Standard, true) => Self::High,
            (Self::High, true) => Self::Original,
            (Self::Original, true) => Self::Standard,
            (Self::Standard, false) => Self::Original,
            (Self::High, false) => Self::Standard,
            (Self::Original, false) => Self::High,
        }
    }

    /// Short human label for the Playback settings row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => t!("Standard · up to 640 px", "표준 · 최대 640 px"),
            Self::High => t!("High · up to 768 px", "고화질 · 최대 768 px"),
            Self::Original => t!("Original source", "원본 화질"),
        }
    }
}

/// Window layout for the external mpv video overlay launched from the player (`v`), cycled
/// live with `Shift+V` and chosen as the open default in Settings. `Compact` docks a small
/// ~30% window top-right; `Large` centers a ~50% window; `Fullscreen` fills the screen.
/// Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoOverlay {
    #[default]
    Compact,
    Large,
    Fullscreen,
}

impl VideoOverlay {
    /// Step to the next/previous layout in the `Compact → Large → Fullscreen` cycle.
    pub fn cycled(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Compact, true) => Self::Large,
            (Self::Large, true) => Self::Fullscreen,
            (Self::Fullscreen, true) => Self::Compact,
            (Self::Compact, false) => Self::Fullscreen,
            (Self::Large, false) => Self::Compact,
            (Self::Fullscreen, false) => Self::Large,
        }
    }

    /// The next layout (for the forward-cycling `Shift+V` toggle).
    pub fn toggled(self) -> Self {
        self.cycled(true)
    }

    /// Short human label for the status toast.
    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => t!("top-right · 30%", "우상단 · 30%"),
            Self::Large => t!("center · 50%", "가운데 · 50%"),
            Self::Fullscreen => t!("fullscreen", "전체화면"),
        }
    }

    /// mpv flags for the overlay window. `Compact` docks a borderless top-right ~30% window;
    /// `Large` a borderless centered ~50% window; `Fullscreen` fills the screen (borderless/
    /// on-top/autofit are meaningless there, so they're dropped).
    pub fn mpv_window_args(self) -> Vec<String> {
        match self {
            Self::Compact => vec![
                "--ontop".to_owned(),
                "--no-border".to_owned(),
                "--autofit=30%".to_owned(),
                "--geometry=-20+20".to_owned(),
            ],
            Self::Large => vec![
                "--ontop".to_owned(),
                "--no-border".to_owned(),
                "--autofit=50%".to_owned(),
            ],
            Self::Fullscreen => vec!["--fullscreen".to_owned()],
        }
    }
}

/// Where the player control block (title / seekbar / transport / status) sits. `Top` is the
/// legacy layout: the block heads the Player view and other screens carry no player chrome.
/// `Bottom` (the default) docks the block above the footer on every screen, and the Player
/// view centers its filler in the space above. Persisted in `config.json`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerBarPosition {
    Top,
    #[default]
    Bottom,
}

impl PlayerBarPosition {
    /// The other position (for the Settings `< >` cycle — two states, so both directions agree).
    pub fn toggled(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    /// Short human label for the Settings row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Top => t!("Top (classic)", "상단 (클래식)"),
            Self::Bottom => t!("Bottom (docked)", "하단 (도킹)"),
        }
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
    /// Defaults written only for a genuinely new profile. Keep this separate from [`Default`]:
    /// serde recovery, legacy configs, tests, and non-TUI reset paths all rely on conservative
    /// defaults that do not opt an existing user into a walkthrough.
    pub(crate) fn fresh_install() -> Self {
        Self {
            beginner_mode: true,
            beginner_tutorial: BeginnerTutorialProgress::welcome(),
            ..Self::default()
        }
    }

    pub(crate) fn preflight_persistence_recovery()
    -> Result<(), crate::persist::StartupRecoveryError> {
        let Some(path) = config_path() else {
            return Ok(());
        };
        crate::persist::preflight_journal_recovery::<Self>(
            crate::persist::StoreKind::Config,
            &path,
            MAX_CONFIG_BYTES,
        )
    }

    /// Load config, importing from the old app on first run. Never fails: a missing or
    /// corrupt file falls back to defaults (+ migration).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            // No config dir on this platform: migrate from the old app but don't persist.
            let old = old_config_path();
            let mut cfg = config_for_missing_profile(old.as_deref());
            if let Some(old) = old {
                import_old_from(&old, &mut cfg);
            }
            return cfg;
        };
        Self::load_from(&path)
    }

    /// Load from an explicit path (also the test seam). `config.json` is the only
    /// secret-bearing store (YTM cookie, Gemini key, Last.fm/ListenBrainz tokens), so a
    /// present-but-unloadable file is set aside *before* the defaults `save()` below can
    /// overwrite it — matching `safe_fs::load_json_or_default_limited`'s backup-aside policy.
    fn load_from(path: &std::path::Path) -> Self {
        recovery::load_from_path(path)
    }

    /// Persist atomically (write a temp file, then rename over the target).
    pub fn save(&self) -> std::io::Result<()> {
        crate::persist::ensure_persistence_writes_allowed()?;
        let Some(path) = config_path() else {
            return Ok(()); // no config dir on this platform; nothing to do
        };
        self.save_to(&path)
    }

    fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        crate::persist::write_store_json(path, self)
    }

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

/// Default location for an optional exported Netscape cookies file.
///
/// macOS: `~/Music/yututui/cookies.txt`
/// Windows: `%USERPROFILE%\Music\yututui\cookies.txt`
pub fn default_cookies_file() -> Option<PathBuf> {
    default_ytm_dir().map(|dir| dir.join("cookies.txt"))
}

fn validate_external_cookies_file(path: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cookies file must not be a symlink",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cookies file must not be a reparse point",
            ));
        }
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cookies path is not a regular file",
        ));
    }
    if meta.len() > MAX_COOKIE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cookies file is too large",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                "cookies file is readable outside the owner; importing a private external-tool copy"
            );
        }
    }
    Ok(())
}

fn import_external_cookies_file(source: &Path, data_dir: &Path) -> std::io::Result<PathBuf> {
    validate_external_cookies_file(source)?;
    let bytes = safe_fs::read_no_symlink_limited(source, MAX_COOKIE_BYTES)?;
    let target = data_dir.join(EXTERNAL_COOKIES_COPY);
    safe_fs::write_private_atomic(&target, &bytes)?;
    Ok(target)
}

fn external_cookies_warning(error: &std::io::Error) -> String {
    let reason = match error.kind() {
        std::io::ErrorKind::NotFound => "file was not found",
        std::io::ErrorKind::PermissionDenied => "permission or symlink check failed",
        std::io::ErrorKind::InvalidInput => "path is not a regular file",
        std::io::ErrorKind::InvalidData => "file is too large or invalid",
        _ => "file could not be read or imported",
    };
    format!(
        "Cookies file not used for mpv/yt-dlp: {reason}. Use a real non-symlink cookies.txt under 4 MiB."
    )
}

/// Default directory for downloaded tracks.
///
/// macOS: `~/Music/yututui`
/// Windows: `%USERPROFILE%\Music\yututui`
pub fn default_download_dir() -> PathBuf {
    default_ytm_dir().unwrap_or_else(|| PathBuf::from("yututui"))
}

/// Normalize a user-supplied directory (env override or stored config): trim surrounding
/// whitespace, treat a whitespace-only value as "unset" (→ fall through to the default), and
/// expand a leading `~` / `~/` to the home directory. Without this a value like `~/Music` was
/// stored/used literally (creating a dir named `~`), and `"   "` created a spaces-named dir.
fn normalize_user_dir(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "~" {
        return directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(base) = directories::BaseDirs::new()
    {
        return Some(base.home_dir().join(rest));
    }
    Some(PathBuf::from(trimmed))
}

/// Default directory for saved radio recordings: `<music>/yututui/recordings`.
pub fn default_recording_dir() -> PathBuf {
    default_ytm_dir()
        .map(|d| d.join("recordings"))
        .unwrap_or_else(|| PathBuf::from("yututui/recordings"))
}

fn default_ytm_dir() -> Option<PathBuf> {
    directories::UserDirs::new()
        .and_then(|u| u.audio_dir().map(std::path::Path::to_path_buf))
        .map(ytm_dir_under_audio_dir)
}

fn ytm_dir_under_audio_dir(audio_dir: PathBuf) -> PathBuf {
    audio_dir.join("yututui")
}

pub(crate) fn config_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join("config.json"))
}

fn old_config_path() -> Option<PathBuf> {
    // Never import the original app's config while a config-dir override is active (a non-blank
    // env override — mirroring `paths::config_dir`'s blank-is-unset rule — or the test sandbox):
    // tests must not read the developer's real `~/.youtube-music-cli`.
    let overridden = std::env::var("YTM_CONFIG_DIR").is_ok_and(|v| !v.trim().is_empty());
    if overridden || cfg!(test) {
        return None;
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".youtube-music-cli/config.json"))
}

/// Pick the base for a missing current config. A legacy path that cannot be inspected counts as
/// present: surprising an existing user with onboarding is worse than conservatively leaving it
/// off, and the import itself will still use the hardened bounded/no-symlink reader below.
fn config_for_missing_profile(old: Option<&Path>) -> Config {
    let legacy_present = old.is_some_and(|path| match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    });
    if legacy_present {
        Config::default()
    } else {
        Config::fresh_install()
    }
}

/// Pull whatever we can reuse out of the old TypeScript app's config. Favorites,
/// history and playlists are migrated later (M5) when the library view consumes them.
fn import_old_from(path: &std::path::Path, cfg: &mut Config) {
    // Legacy import: cap the read and refuse a symlink, like every other persisted-state read.
    let Ok(bytes) = crate::util::safe_fs::read_no_symlink_limited(path, MAX_CONFIG_BYTES) else {
        return;
    };
    let Ok(text) = String::from_utf8(bytes) else {
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
        // Suffix-boundary match, not `contains`: `evil-youtube.com` / `notyoutube.com` must not
        // slip through. A Netscape domain field may carry a leading dot (`.youtube.com`).
        let domain = fields[0].trim_start_matches('.');
        if !(domain == "youtube.com" || domain.ends_with(".youtube.com")) {
            continue;
        }
        let (name, value) = (fields[5].trim(), fields[6].trim());
        // Drop any pair carrying header-breaking characters. A genuine cookie value never
        // contains CR/LF/`;`; rejecting them keeps a crafted cookies.txt from injecting extra
        // header pairs into the `name=value; …` string.
        if name.is_empty() || name.contains(['\r', '\n', ';']) || value.contains(['\r', '\n', ';'])
        {
            continue;
        }
        pairs.push(format!("{name}={value}"));
    }
    pairs.join("; ")
}

#[cfg(test)]
mod cache_policy_tests;
#[cfg(test)]
mod hardening_tests;

#[cfg(test)]
mod tests;
