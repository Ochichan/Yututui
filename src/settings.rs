//! Settings-screen state: tabs, editable fields, and a draft the user commits on close.
//!
//! The screen edits a [`SettingsDraft`] — a snapshot of the persisted [`Config`] plus the
//! live audio state — without touching `App`'s committed state until the screen closes.
//! Audio-affecting fields (speed, EQ, normalization) are applied to mpv *live* as they
//! change; closing the screen copies the draft into `App` and persists. Rendering lives in
//! [`crate::ui::views::settings`]; key handling in
//! [`crate::app`].

use std::path::{Path, PathBuf};

use crate::ai::GeminiModel;
use crate::config::{
    AnimationsConfig, Config, LocalRootConfig, MPV_CACHE_BACK_DEFAULT, MPV_CACHE_FORWARD_DEFAULT,
    SpotifyImportMode, default_cookies_file, default_download_dir,
};
use crate::eq::{self, EqPreset};
use crate::i18n::Language;
use crate::keymap::{Action, KeyContext, KeyMap};
use crate::mousemap::MouseMap;
use crate::search_source::SearchConfig;
use crate::streaming::{CuratingMode, StreamingMode};
use crate::t;
use crate::theme::{ThemeConfig, ThemeRole};

mod actions;
mod field_meta;
pub use actions::{FieldKind, PersonalDataExportStatus};

#[cfg(test)]
mod spotify_tests;

/// Per-band gain keyboard step (dB) for the EQ sliders. The gain limits and the clamp
/// live in [`crate::eq`] (re-exported below) so band semantics have a single home.
pub const BAND_GAIN_STEP: f64 = 1.0;
/// Playback-speed step for the settings slider (←/→).
pub const SPEED_STEP: f64 = 0.1;
/// Seek-step slider step (seconds) for the Playback tab (←/→).
pub const SEEK_SECONDS_STEP: f64 = 1.0;
/// Animation frame-rate slider step (fps) for the Graphics tab (←/→). Values are clamped to
/// [`crate::config::FPS_MIN`]..=[`crate::config::FPS_MAX`] on change.
pub const ANIM_FPS_STEP: u16 = 5;

/// The settings tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    /// Playback controls + the EQ, shown as two sub-categories (see [`Self::sections`]).
    Playback,
    /// The remappable-keybinding editor (its own render/navigation path).
    Keys,
    /// Theme preset, color roles, and animation toggles grouped by feature and expected cost.
    Graphics,
    Ai,
    /// External accounts: Last.fm / ListenBrainz scrobbling (and later Spotify), as
    /// per-service sub-categories.
    Accounts,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 6] = [
        SettingsTab::General,
        SettingsTab::Playback,
        SettingsTab::Keys,
        SettingsTab::Graphics,
        SettingsTab::Ai,
        SettingsTab::Accounts,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::General => t!("General", "일반"),
            SettingsTab::Playback => t!("Playback", "재생"),
            SettingsTab::Keys => t!("Hotkeys", "핫키"),
            SettingsTab::Graphics => t!("Graphics", "그래픽"),
            SettingsTab::Ai => t!("DJ Gem", "DJ Gem"),
            SettingsTab::Accounts => t!("Accounts", "계정"),
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    /// The next/previous tab (wraps), for Tab / BackTab.
    pub fn stepped(self, forward: bool) -> Self {
        let n = Self::ALL.len();
        let i = self.index();
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        Self::ALL[j]
    }

    /// The ordered fields shown under this tab. Merged tabs concatenate their sub-categories'
    /// fields in display order; [`Self::sections`] supplies the headers drawn between them.
    pub fn fields(self) -> Vec<Field> {
        match self {
            SettingsTab::General => vec![
                Field::Language,
                Field::SearchSource,
                Field::StreamingSource,
                Field::SearchYoutube,
                Field::SearchSoundCloud,
                Field::SearchAudius,
                Field::AudiusAppName,
                Field::SearchJamendo,
                Field::JamendoClientId,
                Field::SearchInternetArchive,
                Field::SearchRadioBrowser,
                Field::CookiesFile,
                Field::DownloadDir,
                Field::LocalIncludeDownloadDir,
                Field::LocalMusicRoot,
                Field::LocalMusicRootRecursive,
                Field::Mouse,
                Field::AlbumArt,
                Field::PlayerBarPosition,
                Field::BigText,
                Field::AutoplayOnStart,
                Field::EnqueueNext,
                Field::UpdateCheck,
                Field::ExportPersonalData,
                Field::ResetKeybindings,
                Field::ResetAll,
            ],
            // "Now Playing" controls then the EQ (preset + ten bands + normalize).
            SettingsTab::Playback => {
                let mut f = vec![
                    Field::Speed,
                    Field::SeekInterval,
                    Field::MouseWheelVolume,
                    Field::Gapless,
                    Field::MediaControls,
                    Field::AutoContinueVideos,
                    Field::VideoLayout,
                    // Radio-only entry (the recording popup); filtered out by
                    // `SettingsState::fields` when not in radio mode. Keep it last in the
                    // "Now Playing" section so the static count below stays partition-correct.
                    Field::RadioRecording,
                    Field::AudioBackend,
                    Field::AudioOutput,
                    Field::AudioMpvCacheForward,
                    Field::AudioMpvCacheBack,
                    Field::EqPreset,
                ];
                f.extend((0..eq::BANDS).map(Field::Band));
                f.push(Field::Normalize);
                f
            }
            // Theme/colors, then animation groups from low to high average render cost.
            SettingsTab::Graphics => {
                let mut f = vec![Field::RetroMode, Field::ThemePreset, Field::BackgroundNone];
                f.extend(ThemeRole::ALL.iter().copied().map(Field::ThemeColor));
                f.extend([
                    Field::AnimMaster,
                    Field::AnimPauseUnfocused,
                    Field::AnimFps,
                    Field::AnimErrorShake,
                    Field::AnimLikeBurst,
                    Field::AnimTrackIntro,
                    Field::AnimSeekFlash,
                    Field::AnimPauseFlash,
                    Field::AnimVolumeFlash,
                    Field::AnimToast,
                    Field::AnimAboutFx,
                    Field::AnimPopupFade,
                    Field::AnimTabs,
                    Field::AnimStagger,
                    Field::AnimActivity,
                    Field::AnimCaret,
                    Field::AnimSelection,
                    Field::AnimTimeGlow,
                    Field::AnimHeart,
                    Field::AnimSpinner,
                    Field::AnimControls,
                    Field::AnimEqBars,
                    Field::AnimSeekbar,
                    Field::AnimProgressSparkle,
                    Field::AnimTitle,
                    Field::AnimLyrics,
                    Field::AnimBorderChase,
                    Field::AnimBorder,
                    Field::AnimBounce,
                    Field::AnimComets,
                    Field::AnimSnow,
                    Field::AnimStarfield,
                    Field::AnimFireflies,
                    Field::AnimCube,
                    Field::AnimAquarium,
                    Field::AnimWaves,
                    Field::AnimVisualizer,
                    Field::AnimFireworks,
                    Field::AnimRain,
                    Field::AnimLife,
                    Field::AnimPipes,
                    Field::AnimDonut,
                    Field::AnimPlasma,
                ]);
                f
            }
            SettingsTab::Ai => vec![
                Field::AiEnabled,
                Field::GeminiModel,
                Field::ApiKey,
                Field::DjGemLanguage,
                Field::RomanizedTitles,
                Field::ClearRomanizedTitleCache,
                Field::AutoplayStreaming,
                Field::CuratingMode,
                Field::StreamingMode,
            ],
            // Per-service account sections; see `sections` for the headers.
            SettingsTab::Accounts => vec![
                Field::LastfmEnabled,
                Field::LastfmConnect,
                Field::LastfmLoveSync,
                Field::ListenBrainzEnabled,
                Field::ListenBrainzToken,
                Field::SpotifyClientId,
                Field::SpotifyRedirectPort,
                Field::SpotifyConnect,
                Field::SpotifyImportMode,
                Field::SpotifyImport,
                Field::ScrobbleLocalFiles,
            ],
            // The Keys tab is a list of remappable bindings, not `Field`s; it has its own
            // navigation and rendering paths (see `crate::keymap::editable_entries`).
            SettingsTab::Keys => Vec::new(),
        }
    }

    /// Sub-category headers for tabs that group their fields, each as `(title, field_count)`
    /// in field order; the counts partition [`Self::fields`] exactly. An empty result means the
    /// tab renders as a flat list with no headers.
    pub fn sections(self) -> Vec<(&'static str, usize)> {
        match self {
            SettingsTab::Playback => vec![
                // 8 = the 7 Now-Playing controls + the radio-only recording entry. When not
                // in radio mode, `SettingsState::sections` decrements this back to 7 in
                // lockstep with `SettingsState::fields` hiding `RadioRecording`.
                (t!("Now Playing", "현재 재생"), 8),
                (t!("Audio backend", "오디오 백엔드"), 4),
                (t!("EQ", "EQ"), eq::BANDS + 2),
            ],
            // Animation sections and the fields within each are ordered by average resource
            // usage under typical use, lightest first (the user-facing sorting contract).
            SettingsTab::Graphics => vec![
                (t!("Theme", "테마"), 3),
                (t!("Colors", "색상"), ThemeRole::ALL.len()),
                (t!("Animation controls", "애니메이션 제어"), 3),
                (t!("Event feedback", "이벤트 피드백"), 7),
                (t!("Interface motion", "인터페이스 동작"), 7),
                (t!("Now playing", "현재 재생"), 11),
                (t!("Ambient canvas", "배경 캔버스"), 9),
                (t!("Canvas showpieces", "캔버스 쇼피스"), 6),
            ],
            SettingsTab::Accounts => vec![
                ("Last.fm", 3),
                ("ListenBrainz", 2),
                ("Spotify", 5),
                (t!("Scrobbling", "스크로블링"), 1),
            ],
            // Separate the chat/assistant config from the autoplay + curation trio so the
            // "Autoplay / Curating mode / Curating style" group reads as one intuitive unit.
            SettingsTab::Ai => vec![
                (t!("Assistant", "어시스턴트"), 6),
                (t!("Autoplay & curation", "자동재생 · 큐레이팅"), 3),
            ],
            _ => Vec::new(),
        }
    }
}

/// One editable setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    // General
    /// The UI language (English / 한국어), cycled like any other Select field.
    Language,
    /// Default source selected in the search box.
    SearchSource,
    /// Source used for autoplay/DJ Gem streaming candidate search.
    StreamingSource,
    SearchYoutube,
    SearchSoundCloud,
    SearchAudius,
    AudiusAppName,
    SearchJamendo,
    JamendoClientId,
    SearchInternetArchive,
    SearchRadioBrowser,
    CookiesFile,
    DownloadDir,
    LocalIncludeDownloadDir,
    LocalMusicRoot,
    LocalMusicRootRecursive,
    Mouse,
    AlbumArt,
    /// Where the player control block sits (Top classic / Bottom docked on every screen).
    PlayerBarPosition,
    AutoplayOnStart,
    /// Manual "add to queue" inserts immediately after the current track instead of at the end.
    EnqueueNext,
    /// Check GitHub on startup for a newer YuTuTui! release (the About-card update notice).
    UpdateCheck,
    /// A non-destructive button that exports the portable personal-data snapshot to Downloads.
    ExportPersonalData,
    /// A button (not a value): restores all keybindings to their built-in defaults.
    ResetKeybindings,
    /// A button (not a value): activates "reset every setting to defaults".
    ResetAll,
    /// Text zoom to the mode's "big" level (150% via OSC 66, 200% via double-size
    /// lines) — the one-toggle version of Ctrl+wheel for people who never learn chords.
    BigText,
    // Playback
    Speed,
    SeekInterval,
    MouseWheelVolume,
    Gapless,
    /// Publish playback to the OS media session (Now Playing / SMTC / MPRIS) and
    /// accept media keys / widget control. Applied live on save.
    MediaControls,
    /// When the `v` video overlay is open, auto-play the next queue track's video at the
    /// end of the current one (TUI only).
    AutoContinueVideos,
    /// Which layout the `v` video overlay opens in by default (Compact / Large / Fullscreen);
    /// a Select field cycled like the others. `Shift+V` still cycles the live window.
    VideoLayout,
    /// Opens the radio-recording settings popup. Radio-mode only — hidden outside it by
    /// [`SettingsState::fields`]; lives in the "Now Playing" section.
    RadioRecording,
    /// The selected audio backend. v1 exposes mpv as the only backend.
    AudioBackend,
    /// Opens the automatically populated local audio-output picker.
    AudioOutput,
    // Kept as internal draft/edit compatibility fields; no longer rendered as raw rows.
    AudioMpvOutput,
    AudioMpvDevice,
    AudioMpvCacheForward,
    AudioMpvCacheBack,
    // EQ
    EqPreset,
    Band(usize),
    Normalize,
    // DJ Gem
    /// Master on/off for the DJ Gem assistant. Turning it off keeps the saved key but tears the
    /// assistant down, so DJ Gem can be disabled without discarding the API key.
    AiEnabled,
    GeminiModel,
    ApiKey,
    /// The language DJ Gem replies in (Auto / English / Korean / Japanese / Chinese), set
    /// independently of the UI language. A Select field; retro mode pins it to English.
    DjGemLanguage,
    /// Display Korean/Japanese/CJK track metadata as Latin-script overlays.
    RomanizedTitles,
    /// Remove cached Latin-script display overlays without touching source metadata.
    ClearRomanizedTitleCache,
    AutoplayStreaming,
    /// Who curates the autoplay stream: YouTube-native local engine, or DJ Gem's AI rerank.
    CuratingMode,
    /// The streaming station's adventurousness (Focused / Balanced / Discovery).
    StreamingMode,
    // Theme
    /// Linux basic TTY compatibility: English UI, Retro theme, and ASCII-safe rendering.
    RetroMode,
    ThemePreset,
    /// Toggle the background to "no color" (transparent), letting the terminal show through.
    BackgroundNone,
    ThemeColor(ThemeRole),
    // Animations — `AnimMaster` is the global enable and `AnimFps` is the frame-rate slider; the
    // rest are per-effect on/off toggles, each mapping to a flag in [`AnimationsConfig`] via
    // `anim_flag`. `AnimFps` is the one non-toggle here (a `Slider`), so it is excluded from
    // `anim_flag` and handled on its own in `kind`/`value_display`/`settings_change`.
    AnimMaster,
    AnimFps,
    /// Behaviour knob (a toggle): pause the animation tick while the terminal is unfocused.
    /// Maps to `AnimationsConfig::pause_unfocused`, handled explicitly (not via `anim_flag`,
    /// which is for visual effects only).
    AnimPauseUnfocused,
    AnimTitle,
    AnimHeart,
    AnimSeekbar,
    AnimSpinner,
    AnimEqBars,
    AnimControls,
    AnimBorder,
    // Player one-shots (event feedback).
    AnimTrackIntro,
    AnimLyrics,
    AnimToast,
    AnimVolumeFlash,
    AnimLikeBurst,
    AnimSeekFlash,
    AnimPauseFlash,
    AnimErrorShake,
    // UI-wide effects (Search / Library / Settings / DJ Gem / popups).
    AnimSelection,
    AnimStagger,
    AnimCaret,
    AnimTabs,
    AnimPopupFade,
    AnimActivity,
    AnimAboutFx,
    // Second-wave Now Playing element effects.
    AnimTimeGlow,
    AnimProgressSparkle,
    AnimBorderChase,
    // Filler-canvas effects.
    AnimRain,
    AnimDonut,
    AnimVisualizer,
    AnimStarfield,
    AnimBounce,
    AnimComets,
    AnimSnow,
    AnimFireflies,
    AnimCube,
    AnimAquarium,
    AnimWaves,
    // Canvas showpieces (the heavy end of the filler family).
    AnimFireworks,
    AnimLife,
    AnimPipes,
    AnimPlasma,
    // Accounts
    /// Scrobble to Last.fm (kept separate from connecting, so the session survives an off).
    LastfmEnabled,
    /// A button: starts the browser authorization flow, or disconnects when connected.
    LastfmConnect,
    /// Mirror in-app like/unlike to Last.fm love/unlove.
    LastfmLoveSync,
    ListenBrainzEnabled,
    /// The user token from listenbrainz.org/settings (masked like the API key).
    ListenBrainzToken,
    /// Scrobble local files too (when they carry title + artist metadata).
    ScrobbleLocalFiles,
    /// The user's own Spotify app Client ID (a public identifier, not a secret).
    SpotifyClientId,
    /// Loopback port — must match the app's registered redirect URI.
    SpotifyRedirectPort,
    /// A button: run the PKCE browser flow, or disconnect when connected.
    SpotifyConnect,
    /// How TUI Spotify imports write local Library playlists.
    SpotifyImportMode,
    /// A button: list Spotify playlists and import the picked one.
    SpotifyImport,
}

/// Settings actions that require an explicit confirmation before mutating state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsConfirm {
    RetroMode,
    ResetKeybindings,
    ResetAll,
    ClearRomanizedTitleCache,
    LastfmDisconnect,
    SpotifyDisconnect,
    /// Guard entering edit mode on the masked Gemini API key: activating the row clears the
    /// buffer for a fresh key, so a stray Enter/click would otherwise blank the saved key.
    EditApiKey,
}

impl SettingsConfirm {
    pub fn title(self) -> &'static str {
        match self {
            SettingsConfirm::RetroMode => t!(" Confirm retro mode ", " 레트로 모드 확인 "),
            SettingsConfirm::ResetKeybindings => {
                t!(" Confirm reset keybindings ", " 단축키 초기화 확인 ")
            }
            SettingsConfirm::ResetAll => {
                t!(" Confirm reset all settings ", " 모든 설정 초기화 확인 ")
            }
            SettingsConfirm::ClearRomanizedTitleCache => {
                t!(" Confirm clear title cache ", " 제목 캐시 삭제 확인 ")
            }
            SettingsConfirm::LastfmDisconnect => {
                t!(" Confirm Last.fm disconnect ", " Last.fm 연결 해제 확인 ")
            }
            SettingsConfirm::SpotifyDisconnect => {
                t!(" Confirm Spotify disconnect ", " Spotify 연결 해제 확인 ")
            }
            SettingsConfirm::EditApiKey => t!(" Edit API key ", " API 키 수정 확인 "),
        }
    }

    pub fn prompt(self, enabling: bool) -> &'static str {
        match self {
            SettingsConfirm::RetroMode if enabling => {
                t!("Enable Retro mode?", "레트로 모드를 켤까요?")
            }
            SettingsConfirm::RetroMode => t!("Disable Retro mode?", "레트로 모드를 끌까요?"),
            SettingsConfirm::ResetKeybindings => {
                t!(
                    "Reset every keybinding to default?",
                    "모든 단축키를 기본값으로 되돌릴까요?"
                )
            }
            SettingsConfirm::ResetAll => {
                t!(
                    "Restore every setting to its default?",
                    "모든 설정을 기본값으로 되돌릴까요?"
                )
            }
            SettingsConfirm::ClearRomanizedTitleCache => {
                t!(
                    "Clear the romanized title cache?",
                    "로마자 제목 캐시를 삭제할까요?"
                )
            }
            SettingsConfirm::LastfmDisconnect => {
                t!("Disconnect Last.fm?", "Last.fm 연결을 해제할까요?")
            }
            SettingsConfirm::SpotifyDisconnect => {
                t!("Disconnect Spotify?", "Spotify 연결을 해제할까요?")
            }
            SettingsConfirm::EditApiKey => {
                t!("Edit the Gemini API key?", "Gemini API 키를 수정할까요?")
            }
        }
    }

    pub fn detail(self, enabling: bool) -> &'static str {
        match self {
            SettingsConfirm::RetroMode if enabling => t!(
                "This switches the UI to English and the Retro theme.",
                "UI가 영어와 레트로 테마로 전환됩니다."
            ),
            SettingsConfirm::RetroMode => t!(
                "Theme and language stay as they are until changed.",
                "테마와 언어는 직접 바꾸기 전까지 유지됩니다."
            ),
            SettingsConfirm::ResetKeybindings => t!(
                "The edited keymap will be replaced in this settings draft.",
                "현재 설정 초안의 단축키가 모두 교체됩니다."
            ),
            SettingsConfirm::ResetAll => t!(
                "Keybindings, theme, language, and API key included.",
                "단축키, 테마, 언어, API 키 포함."
            ),
            SettingsConfirm::ClearRomanizedTitleCache => t!(
                "Only generated title/artist overlays are removed.",
                "생성된 제목/아티스트 표시 캐시만 삭제됩니다."
            ),
            SettingsConfirm::LastfmDisconnect => t!(
                "Scrobbling stops; queued scrobbles are kept until you reconnect.",
                "스크로블이 중단됩니다. 대기 중 스크로블은 재연결까지 보관돼요."
            ),
            SettingsConfirm::SpotifyDisconnect => t!(
                "Removes the saved token; the Client ID stays configured.",
                "저장된 토큰이 삭제됩니다. 클라이언트 ID는 유지돼요."
            ),
            SettingsConfirm::EditApiKey => t!(
                "The field clears for a fresh key; leave it empty to keep the current one.",
                "새 키 입력을 위해 칸이 비워져요. 비워두면 기존 키가 유지돼요."
            ),
        }
    }
}

/// A human label for band `i`'s center frequency (e.g. `1 kHz`).
pub fn freq_label(i: usize) -> String {
    let hz = eq::BAND_FREQS.get(i).copied().unwrap_or(0);
    if hz >= 1000 {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{hz} Hz")
    }
}

/// The user-editable snapshot.
// No `Debug`: holds the plaintext `gemini_api_key`, so it must not be `{:?}`-printable (see `Config`).
#[derive(Clone)]
pub struct SettingsDraft {
    pub cookies_file: String,
    pub download_dir: String,
    pub local_include_download_dir: bool,
    pub local_music_root: String,
    pub local_music_root_recursive: bool,
    pub search: SearchConfig,
    pub mouse: bool,
    pub album_art: bool,
    /// Where the player control block sits (previewed live while Settings is open).
    pub player_bar_position: crate::config::PlayerBarPosition,
    pub autoplay_on_start: bool,
    pub enqueue_next: bool,
    /// Whether the startup app-update check runs (the About-card update notice).
    pub update_check_enabled: bool,
    pub speed: f64,
    /// Seek step (seconds) for the seek-back/-forward keys.
    pub seek_seconds: f64,
    /// The "large text" toggle (see [`Field::BigText`]). `big_text_percent` is the
    /// level it enables — seeded from the detected zoom mode when Settings opens, since
    /// the draft itself has no terminal access.
    pub big_text: bool,
    pub big_text_percent: u16,
    /// Whether wheel events over the player volume cluster nudge volume.
    pub mouse_wheel_volume: bool,
    pub gapless: bool,
    /// Publish playback to the OS media session (Now Playing / SMTC / MPRIS).
    pub media_controls: bool,
    /// The video overlay auto-continues into the next queue track's video on EOF.
    pub auto_continue_videos: bool,
    /// Which layout the `v` video overlay opens in by default.
    pub video_layout: crate::config::VideoOverlay,
    pub audio_backend: crate::config::AudioBackend,
    pub audio_mpv_output: String,
    pub audio_mpv_device: String,
    pub audio_mpv_cache_forward: String,
    pub audio_mpv_cache_back: String,
    pub autoplay_streaming: bool,
    /// Who curates the autoplay stream (YT Native vs DJ Gem); persists to `streaming.ai.enabled`.
    pub curating_mode: CuratingMode,
    /// The streaming station's adventurousness (drives MMR λ, sampling temperature, artist spacing).
    pub streaming_mode: StreamingMode,
    pub eq_preset: EqPreset,
    pub eq_bands: [f64; eq::BANDS],
    pub normalize: bool,
    pub gemini_model: GeminiModel,
    /// The Gemini key as stored in config (env `GEMINI_API_KEY` still overrides at launch).
    pub gemini_api_key: String,
    /// Whether the DJ Gem assistant is enabled. Lets the user keep the key saved but turn DJ Gem off.
    pub ai_enabled: bool,
    /// Whether CJK track names should be shown as Latin-script display overlays.
    pub romanized_titles: bool,
    /// The language DJ Gem replies in, independent of `language`. Holds the *raw* choice
    /// (including `Auto`); retro's English override is applied at read time in
    /// [`Config::effective_dj_gem_language`], so toggling retro never discards the pick.
    pub dj_gem_language: crate::i18n::DjGemLanguage,
    /// Color theme preset plus role overrides.
    pub theme: ThemeConfig,
    /// Linux basic TTY compatibility: English UI + Retro theme + ASCII-safe rendering.
    pub retro_mode: bool,
    /// UI language. Applied live (via [`crate::i18n::set_language`]) as the user cycles the
    /// dropdown, and persisted on close.
    pub language: Language,
    /// Player-view animation toggles (the Animations tab). Edited in place; the whole struct
    /// is copied into `Config` on save. See [`AnimationsConfig`].
    pub animations: AnimationsConfig,
    // Accounts ----------------------------------------------------------------
    pub lastfm_enabled: bool,
    pub lastfm_love_sync: bool,
    /// The stored session key; not edited directly (the Connect flow / disconnect set it),
    /// carried in the draft so closing the screen round-trips it via `apply_to`.
    pub lastfm_session_key: String,
    /// Display-only: whose account the session belongs to (set by the Connect flow).
    pub lastfm_username: String,
    pub listenbrainz_enabled: bool,
    /// The ListenBrainz user token (masked like the API key).
    pub listenbrainz_token: String,
    pub scrobble_local_files: bool,
    // Spotify (transfer) -------------------------------------------------------
    pub spotify_client_id: String,
    /// Edited as text; validated + parsed on apply.
    pub spotify_redirect_port: String,
    /// Spotify playlist import mode for the TUI picker.
    pub spotify_import_mode: SpotifyImportMode,
    /// Display-only: whether a saved token exists (checked when the screen opens) /
    /// the display name once a connect flow completes this session.
    pub spotify_connected: bool,
    /// A saved token exists but the connection is *orphaned*: config lost the Client
    /// ID (e.g. an older reset) or it no longer matches the token's. The row then
    /// offers a one-press browser reconnect instead of disconnect.
    pub spotify_stale: bool,
    pub spotify_username: String,
    // Radio recording ----------------------------------------------------------
    /// Recording mode (Off / Decide / Save all); the `RadioRecording` button summarizes it.
    pub recording_mode: crate::recorder::RecordingMode,
    pub recording_min_seconds: u32,
    pub recording_max_seconds: u32,
    /// Recordings folder ("" = the default). Edited inside the recording popup, not as a
    /// top-level text field — so the Playback tab stays at one radio item.
    pub recording_dir: String,
    pub recording_past_tracks: usize,
    pub recording_notify: bool,
}

impl SettingsDraft {
    /// Render the current value of `field` for display.
    pub fn value_display(&self, field: Field) -> String {
        match field {
            Field::RadioRecording => self.recording_mode.label(),
            // Each language names itself, so this value is the same regardless of the active
            // UI language (English / 한국어).
            Field::Language => self.language.native_name().to_owned(),
            Field::SearchSource => self.search.source.label().to_owned(),
            Field::StreamingSource => self
                .search
                .normalized_streaming_source(self.search.streaming_source)
                .label()
                .to_owned(),
            Field::SearchYoutube => toggle_str(self.search.youtube),
            Field::SearchSoundCloud => toggle_str(self.search.soundcloud),
            Field::SearchAudius => toggle_str(self.search.audius),
            Field::AudiusAppName => self
                .search
                .audius_app_name
                .clone()
                .unwrap_or_else(|| t!("(default: yututui)", "(기본값: yututui)").to_owned()),
            Field::SearchJamendo => toggle_str(self.search.jamendo),
            Field::JamendoClientId => self
                .search
                .jamendo_client_id
                .clone()
                .unwrap_or_else(|| t!("(none)", "(없음)").to_owned()),
            Field::SearchInternetArchive => toggle_str(self.search.internet_archive),
            Field::SearchRadioBrowser => toggle_str(self.search.radio_browser),
            Field::CookiesFile => {
                if self.cookies_file.is_empty() {
                    default_cookies_file()
                        .map(|p| format!("({}: {})", t!("default", "기본값"), display_path(&p)))
                        .unwrap_or_else(|| t!("(none)", "(없음)").to_owned())
                } else {
                    self.cookies_file.clone()
                }
            }
            Field::DownloadDir => {
                if self.download_dir.is_empty() {
                    format!(
                        "({}: {})",
                        t!("default", "기본값"),
                        display_path(&default_download_dir())
                    )
                } else {
                    self.download_dir.clone()
                }
            }
            Field::LocalIncludeDownloadDir => toggle_str(self.local_include_download_dir),
            Field::LocalMusicRoot => {
                if self.local_music_root.is_empty() {
                    t!("(none)", "(없음)").to_owned()
                } else {
                    self.local_music_root.clone()
                }
            }
            Field::LocalMusicRootRecursive => toggle_str(self.local_music_root_recursive),
            Field::Mouse => toggle_str(self.mouse),
            Field::AlbumArt => toggle_str(self.album_art),
            Field::PlayerBarPosition => self.player_bar_position.label().to_owned(),
            Field::AutoplayOnStart => toggle_str(self.autoplay_on_start),
            Field::EnqueueNext => toggle_str(self.enqueue_next),
            Field::UpdateCheck => toggle_str(self.update_check_enabled),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::SeekInterval => format!("{:.0}s", self.seek_seconds),
            Field::BigText => toggle_str(self.big_text),
            Field::MouseWheelVolume => toggle_str(self.mouse_wheel_volume),
            // Buttons, not values: these rows show how to trigger them.
            Field::ExportPersonalData => PersonalDataExportStatus::Idle.value_display(),
            Field::ResetKeybindings | Field::ResetAll | Field::ClearRomanizedTitleCache => {
                t!("↵ press Enter", "↵ Enter로 실행").to_owned()
            }
            Field::Gapless => toggle_str(self.gapless),
            Field::MediaControls => toggle_str(self.media_controls),
            Field::AutoContinueVideos => toggle_str(self.auto_continue_videos),
            Field::VideoLayout => self.video_layout.label().to_owned(),
            Field::AudioBackend => self.audio_backend.id().to_owned(),
            Field::AudioOutput => audio_optional_display(&self.audio_mpv_device),
            Field::AudioMpvOutput => audio_optional_display(&self.audio_mpv_output),
            Field::AudioMpvDevice => audio_optional_display(&self.audio_mpv_device),
            Field::AudioMpvCacheForward => {
                cache_display(&self.audio_mpv_cache_forward, MPV_CACHE_FORWARD_DEFAULT)
            }
            Field::AudioMpvCacheBack => {
                cache_display(&self.audio_mpv_cache_back, MPV_CACHE_BACK_DEFAULT)
            }
            Field::AutoplayStreaming => toggle_str(self.autoplay_streaming),
            Field::CuratingMode => self.curating_mode.label().to_owned(),
            Field::StreamingMode => self.streaming_mode.label().to_owned(),
            Field::EqPreset => self.eq_preset.label().to_owned(),
            Field::Band(i) => format!("{:+.0} dB", self.eq_bands[i]),
            Field::Normalize => toggle_str(self.normalize),
            Field::AiEnabled => toggle_str(self.ai_enabled),
            Field::GeminiModel => self.gemini_model.label().to_owned(),
            // Retro mode forces English replies, so the row says so plainly instead of showing
            // the (preserved-underneath) picked value that retro would ignore.
            Field::DjGemLanguage => {
                if self.retro_mode {
                    t!("English (Retro mode)", "영어 (레트로 모드)").to_owned()
                } else {
                    self.dj_gem_language.picker_label().to_owned()
                }
            }
            Field::RomanizedTitles => toggle_str(self.romanized_titles),
            Field::RetroMode => toggle_str(self.retro_mode),
            Field::ThemePreset => self.theme.preset_enum().label().to_owned(),
            Field::BackgroundNone => {
                toggle_str(self.theme.is_role_transparent(ThemeRole::Background))
            }
            Field::ThemeColor(role) => self.theme.effective_hex(role),
            // The lone numeric animation field: shown as "<n> fps" (clamped to the valid range).
            Field::AnimFps => format!("{} fps", self.animations.effective_fps()),
            // Behaviour knob, rendered as a checkbox (handled explicitly, not via `anim_flag`).
            Field::AnimPauseUnfocused => toggle_str(self.animations.pause_unfocused),
            // All 41 animation toggles render as a checkbox; one mapping (`anim_flag`) reads
            // the live value out of the draft's `animations`, so display never drifts from
            // the toggle/persist paths. (`field` is the value being matched here.)
            Field::AnimMaster
            | Field::AnimTitle
            | Field::AnimHeart
            | Field::AnimSeekbar
            | Field::AnimSpinner
            | Field::AnimEqBars
            | Field::AnimControls
            | Field::AnimBorder
            | Field::AnimTrackIntro
            | Field::AnimLyrics
            | Field::AnimToast
            | Field::AnimVolumeFlash
            | Field::AnimLikeBurst
            | Field::AnimSeekFlash
            | Field::AnimPauseFlash
            | Field::AnimErrorShake
            | Field::AnimSelection
            | Field::AnimStagger
            | Field::AnimCaret
            | Field::AnimTabs
            | Field::AnimPopupFade
            | Field::AnimActivity
            | Field::AnimAboutFx
            | Field::AnimTimeGlow
            | Field::AnimProgressSparkle
            | Field::AnimBorderChase
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce
            | Field::AnimComets
            | Field::AnimSnow
            | Field::AnimFireflies
            | Field::AnimCube
            | Field::AnimAquarium
            | Field::AnimWaves
            | Field::AnimFireworks
            | Field::AnimLife
            | Field::AnimPipes
            | Field::AnimPlasma => {
                let mut a = self.animations;
                toggle_str(field.anim_flag(&mut a).map(|b| *b).unwrap_or(false))
            }
            // Never echo the key. Editing shows a masked buffer (handled in the view); this
            // is the at-rest summary.
            Field::ApiKey => {
                if self.gemini_api_key.trim().is_empty() {
                    t!("(none)", "(없음)").to_owned()
                } else {
                    t!("***configured***", "***저장됨***").to_owned()
                }
            }
            Field::LastfmEnabled => toggle_str(self.lastfm_enabled),
            // The Connect button doubles as the connection status line.
            Field::LastfmConnect => {
                if self.lastfm_session_key.trim().is_empty() {
                    t!("↵ connect in browser", "↵ Enter로 브라우저 연결").to_owned()
                } else if self.lastfm_username.trim().is_empty() {
                    t!("connected · ↵ disconnect", "연결됨 · ↵ 연결 해제").to_owned()
                } else if crate::i18n::is_korean() {
                    format!("{} 연결됨 · ↵ 연결 해제", self.lastfm_username)
                } else {
                    format!("connected as {} · ↵ disconnect", self.lastfm_username)
                }
            }
            Field::LastfmLoveSync => toggle_str(self.lastfm_love_sync),
            Field::ListenBrainzEnabled => toggle_str(self.listenbrainz_enabled),
            Field::ListenBrainzToken => {
                if self.listenbrainz_token.trim().is_empty() {
                    t!("(none)", "(없음)").to_owned()
                } else {
                    t!("***configured***", "***저장됨***").to_owned()
                }
            }
            Field::ScrobbleLocalFiles => toggle_str(self.scrobble_local_files),
            Field::SpotifyClientId => {
                if self.spotify_client_id.trim().is_empty() {
                    t!(
                        "(none — create an app at developer.spotify.com)",
                        "(없음 — developer.spotify.com에서 앱 생성)"
                    )
                    .to_owned()
                } else {
                    self.spotify_client_id.clone()
                }
            }
            Field::SpotifyRedirectPort => {
                if self.spotify_redirect_port.trim().is_empty() {
                    format!(
                        "({}: {})",
                        t!("default", "기본값"),
                        crate::config::SPOTIFY_REDIRECT_PORT_DEFAULT
                    )
                } else {
                    self.spotify_redirect_port.clone()
                }
            }
            Field::SpotifyConnect => {
                if !self.spotify_connected {
                    t!("↵ connect in browser", "↵ Enter로 브라우저 연결").to_owned()
                } else if self.spotify_stale {
                    t!(
                        "needs reconnect · ↵ reconnect in browser",
                        "재연결 필요 · ↵ Enter로 브라우저 재연결"
                    )
                    .to_owned()
                } else if self.spotify_username.trim().is_empty() {
                    t!("connected · ↵ disconnect", "연결됨 · ↵ 연결 해제").to_owned()
                } else if crate::i18n::is_korean() {
                    format!("{} 연결됨 · ↵ 연결 해제", self.spotify_username)
                } else {
                    format!("connected as {} · ↵ disconnect", self.spotify_username)
                }
            }
            Field::SpotifyImportMode => self.spotify_import_mode.label().to_owned(),
            Field::SpotifyImport => t!(
                "↵ pick a playlist (↵ again cancels a running import)",
                "↵ 플레이리스트 선택 (실행 중엔 ↵로 취소)"
            )
            .to_owned(),
        }
    }

    /// The raw buffer backing a text field, for the view's caret/masking math (so a secret
    /// field is masked by *its own* length rather than a hardcoded one). `None` for
    /// non-text fields. Mirrors the editable routing in `App::settings_text_buf`.
    pub fn text_value(&self, field: Field) -> Option<&str> {
        match field {
            Field::CookiesFile => Some(&self.cookies_file),
            Field::DownloadDir => Some(&self.download_dir),
            Field::LocalMusicRoot => Some(&self.local_music_root),
            Field::AudioMpvOutput => Some(&self.audio_mpv_output),
            Field::AudioMpvDevice => Some(&self.audio_mpv_device),
            Field::AudioMpvCacheForward => Some(&self.audio_mpv_cache_forward),
            Field::AudioMpvCacheBack => Some(&self.audio_mpv_cache_back),
            Field::AudiusAppName => self.search.audius_app_name.as_deref(),
            Field::JamendoClientId => self.search.jamendo_client_id.as_deref(),
            Field::ApiKey => Some(&self.gemini_api_key),
            Field::ListenBrainzToken => Some(&self.listenbrainz_token),
            Field::SpotifyClientId => Some(&self.spotify_client_id),
            Field::SpotifyRedirectPort => Some(&self.spotify_redirect_port),
            Field::ThemeColor(role) => self.theme.overrides.get(role.id()).map(String::as_str),
            _ => None,
        }
    }

    /// Merge the draft's persisted fields into `cfg` (called on save). Live audio fields
    /// are also written so they survive a restart.
    pub fn apply_to(&self, cfg: &mut Config) {
        cfg.cookies_file = blank_to_none(&self.cookies_file).map(PathBuf::from);
        cfg.download_dir = blank_to_none(&self.download_dir).map(PathBuf::from);
        cfg.local.include_download_dir = Some(self.local_include_download_dir);
        set_first_local_root(cfg, &self.local_music_root, self.local_music_root_recursive);
        cfg.search = self.search.clone().normalized();
        cfg.mouse = Some(self.mouse);
        cfg.album_art = Some(self.album_art);
        cfg.player_bar_position = Some(self.player_bar_position);
        cfg.autoplay_on_start = Some(self.autoplay_on_start);
        cfg.enqueue_next = Some(self.enqueue_next);
        cfg.speed = Some(self.speed);
        cfg.seek_seconds = Some(self.seek_seconds);
        // Large text: ON keeps an existing custom zoom level (Ctrl+wheel may have set
        // 250%, say) and otherwise enables the mode's big level; OFF always returns to
        // normal size.
        cfg.text_zoom = Some(if self.big_text {
            cfg.text_zoom
                .filter(|&p| p > 100)
                .unwrap_or(self.big_text_percent)
        } else {
            100
        });
        cfg.mouse_wheel_volume = Some(self.mouse_wheel_volume);
        cfg.gapless = Some(self.gapless);
        cfg.update_check_enabled = self.update_check_enabled;
        cfg.media_controls = Some(self.media_controls);
        cfg.auto_continue_videos = Some(self.auto_continue_videos);
        cfg.video_layout = self.video_layout;
        cfg.audio.backend = self.audio_backend;
        cfg.audio.mpv.output = blank_to_none(&self.audio_mpv_output);
        cfg.audio.mpv.device = blank_to_none(&self.audio_mpv_device);
        cfg.audio.mpv.cache_forward = blank_to_none(&self.audio_mpv_cache_forward)
            .unwrap_or_else(|| MPV_CACHE_FORWARD_DEFAULT.to_owned());
        cfg.audio.mpv.cache_back = blank_to_none(&self.audio_mpv_cache_back)
            .unwrap_or_else(|| MPV_CACHE_BACK_DEFAULT.to_owned());
        cfg.audio.mpv.mark_cache_defaults_current();
        // Radio recording. Keep max strictly above min so the two sliders can't invert.
        cfg.recording.mode = self.recording_mode;
        cfg.recording.min_duration_secs = self.recording_min_seconds;
        cfg.recording.max_duration_secs = self
            .recording_max_seconds
            .max(self.recording_min_seconds + 1);
        cfg.recording.track_directory =
            blank_to_none(&self.recording_dir).map(std::path::PathBuf::from);
        cfg.recording.past_tracks_count = self.recording_past_tracks;
        cfg.recording.notify = self.recording_notify;
        cfg.autoplay_streaming = Some(self.autoplay_streaming);
        cfg.streaming.mode = self.streaming_mode;
        cfg.streaming.ai.enabled = self.curating_mode.uses_ai();
        cfg.eq_preset = self.eq_preset;
        // Store the explicit band array only when it diverges from the preset's gains, so
        // a plain preset choice stays compact in config.json.
        cfg.eq_bands = if self.eq_bands == self.eq_preset.gains() {
            None
        } else {
            Some(self.eq_bands)
        };
        cfg.normalize = Some(self.normalize);
        cfg.gemini_model = self.gemini_model;
        cfg.gemini_api_key = blank_to_none(&self.gemini_api_key);
        cfg.ai_enabled = Some(self.ai_enabled);
        cfg.romanized_titles = Some(self.romanized_titles);
        // Persist the raw choice (including `Auto`); retro's English override lives in
        // `Config::effective_dj_gem_language`, so a save under retro never wipes the pick.
        cfg.dj_gem_language = self.dj_gem_language;
        cfg.retro_mode = self.retro_mode;
        // The theme commits as edited even in retro mode (enabling retro only *seeds* the
        // Retro preset in the draft); retro keeps forcing the English UI, since basic TTY
        // console fonts are the whole point of the mode.
        cfg.theme = self.theme.normalized();
        cfg.language = if self.retro_mode {
            Language::English
        } else {
            self.language
        };
        cfg.animations = self.animations;
        cfg.scrobble.lastfm.enabled = Some(self.lastfm_enabled);
        cfg.scrobble.lastfm.love_sync = Some(self.lastfm_love_sync);
        cfg.scrobble.lastfm.session_key = blank_to_none(&self.lastfm_session_key);
        cfg.scrobble.lastfm.username = blank_to_none(&self.lastfm_username);
        cfg.scrobble.listenbrainz.enabled = Some(self.listenbrainz_enabled);
        cfg.scrobble.listenbrainz.token = blank_to_none(&self.listenbrainz_token);
        cfg.scrobble.local_files = Some(self.scrobble_local_files);
        cfg.spotify.client_id = blank_to_none(&self.spotify_client_id);
        // Invalid port text falls back to the default rather than persisting garbage.
        cfg.spotify.redirect_port = self.spotify_redirect_port.trim().parse::<u16>().ok();
        cfg.spotify.import_mode = self.spotify_import_mode;
    }
}

fn display_path(path: &Path) -> String {
    if let Some(user_dirs) = directories::UserDirs::new()
        && let Ok(stripped) = path.strip_prefix(user_dirs.home_dir())
    {
        let mut shortened = PathBuf::from("~");
        shortened.push(stripped);
        return shortened.display().to_string();
    }
    path.display().to_string()
}

/// The whole settings-screen state, boxed in `App` so it costs nothing when closed.
// No `Debug`: carries `secret_restore` (a captured copy of the API key) and the `draft` above,
// so it must not be `{:?}`-printable.
#[derive(Clone)]
pub struct SettingsState {
    pub tab: SettingsTab,
    pub row: usize,
    pub draft: SettingsDraft,
    /// Whether the focused text field is in character-entry mode.
    pub editing_text: bool,
    /// The masked secret's value captured when its editor opened. The buffer is cleared
    /// on edit-start (blind-paste of a whole new key), so this lets a commit that typed
    /// nothing restore the prior key instead of wiping it. `None` outside a secret edit.
    pub secret_restore: Option<String>,
    /// A working copy of the keymap edited on the Keys tab; committed to `App.keymap` and
    /// persisted when settings closes.
    pub keymap: KeyMap,
    /// Working copy of the safe mouse gesture presets edited after the keyboard rows.
    pub mousemap: MouseMap,
    /// The binding being rebound (Keys tab), while waiting to capture its new key.
    pub capturing: Option<(KeyContext, Action)>,
    /// Open Settings dropdown for the Spotify import mode. Holds the highlighted option index.
    pub spotify_import_mode_dropdown: Option<usize>,
    /// Progress/result for the non-destructive personal-data export button.
    pub personal_data_export: PersonalDataExportStatus,
    /// Whether the user was in a radio context when Settings opened — dedicated Radio mode OR
    /// a radio station currently loaded/playing. Gates the radio-only `RadioRecording` item's
    /// visibility. Captured once at open (neither input can change while Settings is open), so
    /// it never goes stale mid-session.
    pub radio_mode: bool,
}

impl SettingsState {
    /// The current tab's fields after applying draft/mode-dependent visibility rules.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = self.tab.fields();
        if self.tab == SettingsTab::Ai && self.draft.retro_mode {
            fields.retain(|field| *field != Field::ClearRomanizedTitleCache);
        }
        if self.tab == SettingsTab::Playback && !self.radio_mode {
            fields.retain(|field| *field != Field::RadioRecording);
        }
        fields
    }

    /// Section headers whose counts partition [`Self::fields`] exactly, after applying the
    /// same visibility rules. `render_fields` and the mouse scroll-length math walk these in
    /// lockstep with `fields()`, so both MUST go through this (not `self.tab.sections()`) — a
    /// hidden field otherwise desyncs the counts and `fields[i]` panics. This also fixes the
    /// pre-existing Ai+retro desync (Assistant count) for free.
    pub fn sections(&self) -> Vec<(&'static str, usize)> {
        let mut sections = self.tab.sections();
        match self.tab {
            // `RadioRecording` lives in "Now Playing" (section 0); hidden outside radio mode.
            SettingsTab::Playback if !self.radio_mode => {
                if let Some((_, n)) = sections.first_mut() {
                    *n = n.saturating_sub(1);
                }
            }
            // `ClearRomanizedTitleCache` lives in "Assistant" (section 0); hidden in retro mode.
            SettingsTab::Ai if self.draft.retro_mode => {
                if let Some((_, n)) = sections.first_mut() {
                    *n = n.saturating_sub(1);
                }
            }
            _ => {}
        }
        sections
    }

    /// The field the cursor is on, or `None` when the tab has no `Field`s (the Keys tab, which
    /// edits bindings instead). `saturating_sub` matches the view's row clamp; `get` keeps this
    /// panic-free even for an empty tab, so callers reached on any tab (e.g. the per-keystroke
    /// "is this a color row?" check) stay sound.
    pub fn current_field(&self) -> Option<Field> {
        let fields = self.fields();
        fields
            .get(self.row.min(fields.len().saturating_sub(1)))
            .copied()
    }
}

fn toggle_str(on: bool) -> String {
    if on {
        "[x]".to_owned()
    } else {
        "[ ]".to_owned()
    }
}

fn audio_optional_display(value: &str) -> String {
    blank_to_none(value).unwrap_or_else(|| "auto".to_owned())
}

fn cache_display(value: &str, fallback: &str) -> String {
    blank_to_none(value).unwrap_or_else(|| fallback.to_owned())
}

pub(crate) fn blank_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

pub(crate) fn set_first_local_root(cfg: &mut Config, path: &str, recursive: bool) {
    let next = blank_to_none(path).map(|path| LocalRootConfig {
        path: PathBuf::from(path),
        enabled: Some(true),
        recursive: Some(recursive),
    });
    match (next, cfg.local.roots.is_empty()) {
        (Some(root), true) => cfg.local.roots.push(root),
        (Some(root), false) => cfg.local.roots[0] = root,
        (None, false) => {
            cfg.local.roots.remove(0);
        }
        (None, true) => {}
    }
}

// Band gain limits + clamp live in `eq`; speed/seek clamps live in `config`. Each is the
// single source both the TUI controls and the headless daemon apply. Re-exported so
// existing `settings::clamp_band` / `settings::clamp_speed` / `settings::clamp_seek_seconds`
// call sites keep resolving.
pub use crate::config::{clamp_seek_seconds, clamp_speed};
pub use crate::eq::{BAND_GAIN_MAX, BAND_GAIN_MIN, clamp_band};

#[cfg(test)]
mod tests;
