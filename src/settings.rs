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
    AnimationsConfig, Config, SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN,
    default_cookies_file, default_download_dir,
};
use crate::eq::{self, EqPreset};
use crate::i18n::Language;
use crate::keymap::{Action, KeyContext, KeyMap};
use crate::search_source::SearchConfig;
use crate::streaming::StreamingMode;
use crate::t;
use crate::theme::{ThemeConfig, ThemePreset, ThemeRole};

/// Per-band gain limits and keyboard step (dB), for the EQ sliders.
pub const BAND_GAIN_MIN: f64 = -12.0;
pub const BAND_GAIN_MAX: f64 = 12.0;
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
    /// Theme preset, color roles, and animation toggles, as three sub-categories.
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
                Field::Mouse,
                Field::AlbumArt,
                Field::AutoplayOnStart,
                Field::EnqueueNext,
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
                    Field::EqPreset,
                ];
                f.extend((0..eq::BANDS).map(Field::Band));
                f.push(Field::Normalize);
                f
            }
            // Theme (preset + transparent-bg toggle), then every color role, then the
            // animation toggles (master kill-switch first, then per-element effects).
            SettingsTab::Graphics => {
                let mut f = vec![Field::RetroMode, Field::ThemePreset, Field::BackgroundNone];
                f.extend(ThemeRole::ALL.iter().copied().map(Field::ThemeColor));
                f.extend([
                    Field::AnimMaster,
                    Field::AnimFps,
                    Field::AnimPauseUnfocused,
                    Field::AnimTitle,
                    Field::AnimHeart,
                    Field::AnimSeekbar,
                    Field::AnimSpinner,
                    Field::AnimEqBars,
                    Field::AnimControls,
                    Field::AnimBorder,
                    Field::AnimRain,
                    Field::AnimDonut,
                    Field::AnimVisualizer,
                    Field::AnimStarfield,
                    Field::AnimBounce,
                ]);
                f
            }
            SettingsTab::Ai => vec![
                Field::AiEnabled,
                Field::GeminiModel,
                Field::ApiKey,
                Field::RomanizedTitles,
                Field::ClearRomanizedTitleCache,
                Field::AutoplayStreaming,
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
                (t!("Now Playing", "현재 재생"), 5),
                (t!("EQ", "EQ"), eq::BANDS + 2),
            ],
            SettingsTab::Graphics => vec![
                (t!("Theme", "테마"), 3),
                (t!("Colors", "색상"), ThemeRole::ALL.len()),
                (t!("Animations", "애니메이션"), 15),
            ],
            SettingsTab::Accounts => vec![
                ("Last.fm", 3),
                ("ListenBrainz", 2),
                ("Spotify", 4),
                (t!("Scrobbling", "스크로블링"), 1),
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
    Mouse,
    AlbumArt,
    AutoplayOnStart,
    /// Manual "add to queue" inserts immediately after the current track instead of at the end.
    EnqueueNext,
    /// A button (not a value): restores all keybindings to their built-in defaults.
    ResetKeybindings,
    /// A button (not a value): activates "reset every setting to defaults".
    ResetAll,
    // Playback
    Speed,
    SeekInterval,
    MouseWheelVolume,
    Gapless,
    /// Publish playback to the OS media session (Now Playing / SMTC / MPRIS) and
    /// accept media keys / widget control. Applied live on save.
    MediaControls,
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
    /// Display Korean/Japanese/CJK track metadata as Latin-script overlays.
    RomanizedTitles,
    /// Remove cached Latin-script display overlays without touching source metadata.
    ClearRomanizedTitleCache,
    AutoplayStreaming,
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
    AnimRain,
    AnimDonut,
    AnimVisualizer,
    AnimStarfield,
    AnimBounce,
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
    /// A button: list Spotify playlists and import the picked one.
    SpotifyImport,
}

/// How a field is edited / rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text edited in-place (paths). Enter toggles edit mode.
    Text,
    /// On/off, flipped with ←/→ or Enter.
    Toggle,
    /// A value cycled through a set with ←/→.
    Select,
    /// A numeric value nudged with ←/→.
    Slider,
    /// A pressable action (no value); Enter/Confirm triggers it.
    Button,
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
        }
    }
}

impl Field {
    pub fn kind(self) -> FieldKind {
        match self {
            Field::CookiesFile
            | Field::DownloadDir
            | Field::AudiusAppName
            | Field::JamendoClientId
            | Field::ApiKey
            | Field::ListenBrainzToken
            | Field::SpotifyClientId
            | Field::SpotifyRedirectPort
            | Field::ThemeColor(_) => FieldKind::Text,
            Field::Mouse
            | Field::AlbumArt
            | Field::AutoplayOnStart
            | Field::EnqueueNext
            | Field::SearchYoutube
            | Field::SearchSoundCloud
            | Field::SearchAudius
            | Field::SearchJamendo
            | Field::SearchInternetArchive
            | Field::SearchRadioBrowser
            | Field::MouseWheelVolume
            | Field::Gapless
            | Field::MediaControls
            | Field::AutoplayStreaming
            | Field::Normalize
            | Field::AiEnabled
            | Field::RomanizedTitles
            | Field::RetroMode
            | Field::BackgroundNone
            | Field::AnimPauseUnfocused
            | Field::AnimMaster
            | Field::AnimTitle
            | Field::AnimHeart
            | Field::AnimSeekbar
            | Field::AnimSpinner
            | Field::AnimEqBars
            | Field::AnimControls
            | Field::AnimBorder
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce
            | Field::LastfmEnabled
            | Field::LastfmLoveSync
            | Field::ListenBrainzEnabled
            | Field::ScrobbleLocalFiles => FieldKind::Toggle,
            Field::Language
            | Field::SearchSource
            | Field::StreamingSource
            | Field::EqPreset
            | Field::GeminiModel
            | Field::ThemePreset
            | Field::StreamingMode => FieldKind::Select,
            Field::Speed | Field::SeekInterval | Field::Band(_) | Field::AnimFps => {
                FieldKind::Slider
            }
            Field::ResetKeybindings
            | Field::ResetAll
            | Field::ClearRomanizedTitleCache
            | Field::LastfmConnect
            | Field::SpotifyConnect
            | Field::SpotifyImport => FieldKind::Button,
        }
    }

    /// For an animation toggle field, a mutable handle to its flag inside an
    /// [`AnimationsConfig`]; `None` for any non-animation field. This single mapping is the
    /// source of truth used for both rendering the checkbox and flipping it on input — so the
    /// 13 effects never drift out of sync across the display / toggle / persist paths.
    pub(crate) fn anim_flag(self, a: &mut AnimationsConfig) -> Option<&mut bool> {
        Some(match self {
            Field::AnimMaster => &mut a.master,
            Field::AnimTitle => &mut a.title,
            Field::AnimHeart => &mut a.heart,
            Field::AnimSeekbar => &mut a.seekbar,
            Field::AnimSpinner => &mut a.spinner,
            Field::AnimEqBars => &mut a.eq_bars,
            Field::AnimControls => &mut a.controls,
            Field::AnimBorder => &mut a.border,
            Field::AnimRain => &mut a.rain,
            Field::AnimDonut => &mut a.donut,
            Field::AnimVisualizer => &mut a.visualizer,
            Field::AnimStarfield => &mut a.starfield,
            Field::AnimBounce => &mut a.bounce,
            _ => return None,
        })
    }

    pub fn label(self) -> String {
        match self {
            Field::Language => t!("Language", "언어").to_owned(),
            Field::SearchSource => t!("Search source", "검색 소스").to_owned(),
            Field::StreamingSource => t!("Streaming source", "추천 소스").to_owned(),
            Field::SearchYoutube => t!("Source: YouTube", "소스: YouTube").to_owned(),
            Field::SearchSoundCloud => t!("Source: SoundCloud", "소스: SoundCloud").to_owned(),
            Field::SearchAudius => t!("Source: Audius", "소스: Audius").to_owned(),
            Field::AudiusAppName => t!("Audius app name", "Audius 앱 이름").to_owned(),
            Field::SearchJamendo => t!("Source: Jamendo", "소스: Jamendo").to_owned(),
            Field::JamendoClientId => t!("Jamendo client_id", "Jamendo client_id").to_owned(),
            Field::SearchInternetArchive => {
                t!("Source: Internet Archive", "소스: Internet Archive").to_owned()
            }
            Field::SearchRadioBrowser => {
                t!("Source: Radio Browser", "소스: Radio Browser").to_owned()
            }
            Field::CookiesFile => t!("Cookies file", "쿠키 파일").to_owned(),
            Field::DownloadDir => t!("Download dir", "다운로드 폴더").to_owned(),
            Field::Mouse => t!("Mouse (next launch)", "마우스 (재시작 후 적용)").to_owned(),
            Field::AlbumArt => {
                t!("Album art (next launch)", "앨범 아트 (재시작 후 적용)").to_owned()
            }
            Field::AutoplayOnStart => t!("Autoplay on launch", "앱 시작 시 자동재생").to_owned(),
            Field::EnqueueNext => t!("Enqueue as next", "큐 추가: 다음 곡").to_owned(),
            Field::ResetKeybindings => t!("Reset keybindings", "단축키 초기화").to_owned(),
            Field::ResetAll => t!("Reset all settings", "모든 설정 초기화").to_owned(),
            Field::Speed => t!("Playback speed", "재생 속도").to_owned(),
            Field::SeekInterval => t!("Seek interval", "탐색 간격").to_owned(),
            Field::MouseWheelVolume => t!("Wheel volume", "휠 볼륨 조절").to_owned(),
            Field::Gapless => t!("Gapless (next launch)", "갭리스 (재시작 후 적용)").to_owned(),
            Field::MediaControls => t!("OS media controls", "OS 미디어 컨트롤").to_owned(),
            Field::AutoplayStreaming => t!("Autoplay streaming", "자동 스트리밍").to_owned(),
            Field::StreamingMode => t!("Streaming mode", "스트리밍 모드").to_owned(),
            Field::EqPreset => t!("Preset", "프리셋").to_owned(),
            Field::Band(i) => format!("{:>5}", freq_label(i)),
            Field::Normalize => t!("Normalize (loudness)", "음량 평준화").to_owned(),
            Field::AiEnabled => t!("Enable DJ Gem", "DJ Gem 사용").to_owned(),
            Field::GeminiModel => t!("Model", "모델").to_owned(),
            Field::ApiKey => t!("API key", "API 키").to_owned(),
            Field::RomanizedTitles => t!("Romanized titles", "제목 로마자 표기").to_owned(),
            Field::ClearRomanizedTitleCache => {
                t!("Clear romanized title cache", "로마자 제목 캐시 삭제").to_owned()
            }
            Field::RetroMode => t!("Retro mode", "레트로 모드").to_owned(),
            Field::ThemePreset => t!("Preset", "프리셋").to_owned(),
            Field::BackgroundNone => t!("Background: None", "배경 없음").to_owned(),
            Field::ThemeColor(role) => role.label().to_owned(),
            Field::AnimMaster => t!("Enable animations", "애니메이션 켜기").to_owned(),
            Field::AnimFps => t!("Frame rate", "프레임 레이트").to_owned(),
            Field::AnimPauseUnfocused => {
                t!("Pause when unfocused", "포커스 없을 때 정지").to_owned()
            }
            Field::AnimTitle => t!("Title shimmer", "제목 반짝임").to_owned(),
            Field::AnimHeart => t!("Beating heart", "하트 박동").to_owned(),
            Field::AnimSeekbar => t!("Seekbar glow", "탐색바 반짝임").to_owned(),
            Field::AnimSpinner => t!("Now-playing spinner", "재생 스피너").to_owned(),
            Field::AnimEqBars => t!("EQ bars", "EQ 막대").to_owned(),
            Field::AnimControls => t!("Control pulse", "컨트롤 펄스").to_owned(),
            Field::AnimBorder => t!("Breathing border", "테두리 호흡").to_owned(),
            Field::AnimRain => t!("Matrix rain", "매트릭스 비").to_owned(),
            Field::AnimDonut => t!("Spinning donut", "회전 도넛").to_owned(),
            Field::AnimVisualizer => t!("Visualizer", "비주얼라이저").to_owned(),
            Field::AnimStarfield => t!("Starfield / notes", "별·음표").to_owned(),
            Field::AnimBounce => t!("Bouncing logo", "튕기는 로고").to_owned(),
            Field::LastfmEnabled => t!("Scrobble to Last.fm", "Last.fm 스크로블").to_owned(),
            Field::LastfmConnect => t!("Last.fm account", "Last.fm 계정").to_owned(),
            Field::LastfmLoveSync => t!("Sync likes as loves", "좋아요를 love로 동기화").to_owned(),
            Field::ListenBrainzEnabled => {
                t!("Scrobble to ListenBrainz", "ListenBrainz 스크로블").to_owned()
            }
            Field::ListenBrainzToken => t!("User token", "사용자 토큰").to_owned(),
            Field::ScrobbleLocalFiles => {
                t!("Scrobble local files", "로컬 파일 스크로블").to_owned()
            }
            Field::SpotifyClientId => t!("Client ID", "클라이언트 ID").to_owned(),
            Field::SpotifyRedirectPort => t!("Redirect port", "리다이렉트 포트").to_owned(),
            Field::SpotifyConnect => t!("Spotify account", "Spotify 계정").to_owned(),
            Field::SpotifyImport => t!("Import from Spotify…", "Spotify에서 가져오기…").to_owned(),
        }
    }

    /// Whether the field's value must be hidden when displayed (keys / tokens).
    pub fn is_secret(self) -> bool {
        matches!(self, Field::ApiKey | Field::ListenBrainzToken)
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
    pub search: SearchConfig,
    pub mouse: bool,
    pub album_art: bool,
    pub autoplay_on_start: bool,
    pub enqueue_next: bool,
    pub speed: f64,
    /// Seek step (seconds) for the seek-back/-forward keys.
    pub seek_seconds: f64,
    /// Whether wheel events over the player volume cluster nudge volume.
    pub mouse_wheel_volume: bool,
    pub gapless: bool,
    /// Publish playback to the OS media session (Now Playing / SMTC / MPRIS).
    pub media_controls: bool,
    pub autoplay_streaming: bool,
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
    /// Display-only: whether a saved token exists (checked when the screen opens) /
    /// the display name once a connect flow completes this session.
    pub spotify_connected: bool,
    pub spotify_username: String,
}

impl SettingsDraft {
    /// Render the current value of `field` for display.
    pub fn value_display(&self, field: Field) -> String {
        match field {
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
                .unwrap_or_else(|| t!("(default: ytm-tui)", "(기본값: ytm-tui)").to_owned()),
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
            Field::Mouse => toggle_str(self.mouse),
            Field::AlbumArt => toggle_str(self.album_art),
            Field::AutoplayOnStart => toggle_str(self.autoplay_on_start),
            Field::EnqueueNext => toggle_str(self.enqueue_next),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::SeekInterval => format!("{:.0}s", self.seek_seconds),
            Field::MouseWheelVolume => toggle_str(self.mouse_wheel_volume),
            // Buttons, not values: these rows show how to trigger them.
            Field::ResetKeybindings | Field::ResetAll | Field::ClearRomanizedTitleCache => {
                t!("↵ press Enter", "↵ Enter로 실행").to_owned()
            }
            Field::Gapless => toggle_str(self.gapless),
            Field::MediaControls => toggle_str(self.media_controls),
            Field::AutoplayStreaming => toggle_str(self.autoplay_streaming),
            Field::StreamingMode => self.streaming_mode.label().to_owned(),
            Field::EqPreset => self.eq_preset.label().to_owned(),
            Field::Band(i) => format!("{:+.0} dB", self.eq_bands[i]),
            Field::Normalize => toggle_str(self.normalize),
            Field::AiEnabled => toggle_str(self.ai_enabled),
            Field::GeminiModel => self.gemini_model.label().to_owned(),
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
            // All 13 animation toggles render as a checkbox; one mapping (`anim_flag`) reads
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
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce => {
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
                } else if self.spotify_username.trim().is_empty() {
                    t!("connected · ↵ disconnect", "연결됨 · ↵ 연결 해제").to_owned()
                } else if crate::i18n::is_korean() {
                    format!("{} 연결됨 · ↵ 연결 해제", self.spotify_username)
                } else {
                    format!("connected as {} · ↵ disconnect", self.spotify_username)
                }
            }
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
        cfg.search = self.search.clone().normalized();
        cfg.mouse = Some(self.mouse);
        cfg.album_art = Some(self.album_art);
        cfg.autoplay_on_start = Some(self.autoplay_on_start);
        cfg.enqueue_next = Some(self.enqueue_next);
        cfg.speed = Some(self.speed);
        cfg.seek_seconds = Some(self.seek_seconds);
        cfg.mouse_wheel_volume = Some(self.mouse_wheel_volume);
        cfg.gapless = Some(self.gapless);
        cfg.media_controls = Some(self.media_controls);
        cfg.autoplay_streaming = Some(self.autoplay_streaming);
        cfg.streaming.mode = self.streaming_mode;
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
        cfg.retro_mode = self.retro_mode;
        if self.retro_mode {
            let mut retro = ThemeConfig::default();
            retro.set_preset(ThemePreset::Retro);
            cfg.theme = retro;
            cfg.language = Language::English;
        } else {
            cfg.theme = self.theme.normalized();
            cfg.language = self.language;
        }
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
    /// The binding being rebound (Keys tab), while waiting to capture its new key.
    pub capturing: Option<(KeyContext, Action)>,
}

impl SettingsState {
    /// The current tab's fields after applying draft-dependent visibility rules.
    pub fn fields(&self) -> Vec<Field> {
        let mut fields = self.tab.fields();
        if self.tab == SettingsTab::Ai && self.draft.retro_mode {
            fields.retain(|field| *field != Field::ClearRomanizedTitleCache);
        }
        fields
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

pub(crate) fn blank_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

/// Clamp a band gain to range and round to a whole dB.
pub fn clamp_band(g: f64) -> f64 {
    g.clamp(BAND_GAIN_MIN, BAND_GAIN_MAX).round()
}

/// Clamp/round a speed value the way the controls do.
pub fn clamp_speed(s: f64) -> f64 {
    ((s * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX)
}

/// Clamp/round a seek step to whole seconds within the supported range.
pub fn clamp_seek_seconds(s: f64) -> f64 {
    s.round().clamp(SEEK_SECONDS_MIN, SEEK_SECONDS_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_source::SearchSource;

    /// A neutral draft the value/apply tests can tweak one field at a time.
    fn base_draft() -> SettingsDraft {
        SettingsDraft {
            cookies_file: String::new(),
            download_dir: String::new(),
            search: SearchConfig::default(),
            mouse: true,
            album_art: false,
            autoplay_on_start: false,
            enqueue_next: false,
            speed: 1.0,
            seek_seconds: 10.0,
            mouse_wheel_volume: true,
            gapless: true,
            media_controls: true,
            autoplay_streaming: false,
            streaming_mode: StreamingMode::Balanced,
            eq_preset: EqPreset::Flat,
            eq_bands: EqPreset::Flat.gains(),
            normalize: false,
            gemini_model: GeminiModel::default(),
            gemini_api_key: String::new(),
            ai_enabled: true,
            romanized_titles: false,
            theme: ThemeConfig::default(),
            retro_mode: false,
            language: Language::English,
            animations: AnimationsConfig::default(),
            lastfm_enabled: true,
            lastfm_love_sync: true,
            lastfm_session_key: String::new(),
            lastfm_username: String::new(),
            listenbrainz_enabled: true,
            listenbrainz_token: String::new(),
            scrobble_local_files: true,
            spotify_client_id: String::new(),
            spotify_redirect_port: String::new(),
            spotify_connected: false,
            spotify_username: String::new(),
        }
    }

    #[test]
    fn tabs_step_and_wrap() {
        assert_eq!(SettingsTab::General.stepped(true), SettingsTab::Playback);
        assert_eq!(SettingsTab::Playback.stepped(true), SettingsTab::Keys);
        assert_eq!(SettingsTab::Keys.stepped(true), SettingsTab::Graphics);
        assert_eq!(SettingsTab::Graphics.stepped(true), SettingsTab::Ai);
        assert_eq!(SettingsTab::Ai.stepped(true), SettingsTab::Accounts);
        assert_eq!(SettingsTab::Accounts.stepped(true), SettingsTab::General); // wraps
        assert_eq!(SettingsTab::General.stepped(false), SettingsTab::Accounts); // wraps back
    }

    #[test]
    fn accounts_tab_sections_partition_its_fields() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::Accounts.fields();
        let sections = SettingsTab::Accounts.sections();
        // Section counts must partition the field list exactly (the view walks them in lockstep).
        assert_eq!(sections.iter().map(|(_, n)| n).sum::<usize>(), f.len());
        assert_eq!(f[0], Field::LastfmEnabled);
        assert_eq!(Field::LastfmConnect.kind(), FieldKind::Button);
        assert_eq!(Field::ListenBrainzToken.kind(), FieldKind::Text);
        assert!(Field::ListenBrainzToken.is_secret());
        // The connect row doubles as the status line: it must reflect the session state.
        let mut d = base_draft();
        assert!(d.value_display(Field::LastfmConnect).contains('↵'));
        d.lastfm_session_key = "sk".to_owned();
        d.lastfm_username = "listener".to_owned();
        assert!(d.value_display(Field::LastfmConnect).contains("listener"));
    }

    #[test]
    fn graphics_tab_groups_theme_colors_and_animations() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::Graphics.fields();
        // ThemePreset + BackgroundNone + every color role + 15 animation fields
        // plus the RetroMode fallback toggle.
        // (master enable + fps slider + pause-when-unfocused toggle + 12 per-effect toggles).
        assert_eq!(f.len(), 3 + ThemeRole::ALL.len() + 15);
        assert_eq!(f[0], Field::RetroMode);
        assert_eq!(f[1], Field::ThemePreset);
        assert_eq!(f[2], Field::BackgroundNone);
        assert!(matches!(f[3], Field::ThemeColor(_)));
        let anim_start = 3 + ThemeRole::ALL.len();
        assert_eq!(f[anim_start], Field::AnimMaster);
        // The fps slider sits right after the master enable; it's the one non-toggle here.
        assert_eq!(f[anim_start + 1], Field::AnimFps);
        assert_eq!(Field::AnimFps.kind(), FieldKind::Slider);
        assert!(
            f[anim_start..]
                .iter()
                .filter(|fld| **fld != Field::AnimFps)
                .all(|fld| fld.kind() == FieldKind::Toggle)
        );

        // Section counts partition the field list exactly.
        let total: usize = SettingsTab::Graphics
            .sections()
            .iter()
            .map(|(_, n)| n)
            .sum();
        assert_eq!(total, f.len());

        // Default draft: every animation toggle reads as off.
        let mut draft = base_draft();
        assert_eq!(draft.value_display(Field::AnimMaster), "[ ]");
        assert_eq!(draft.value_display(Field::AnimRain), "[ ]");

        // Flipping a couple through the shared mapping shows + persists.
        *Field::AnimMaster.anim_flag(&mut draft.animations).unwrap() = true;
        *Field::AnimDonut.anim_flag(&mut draft.animations).unwrap() = true;
        assert_eq!(draft.value_display(Field::AnimMaster), "[x]");
        assert_eq!(draft.value_display(Field::AnimDonut), "[x]");
        assert_eq!(draft.value_display(Field::AnimRain), "[ ]");

        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert!(cfg.animations.master);
        assert!(cfg.animations.donut);
        assert!(!cfg.animations.rain);
        assert!(cfg.animations.active());

        // Non-animation fields map to no flag.
        assert!(
            Field::Mouse
                .anim_flag(&mut AnimationsConfig::default())
                .is_none()
        );
    }

    #[test]
    fn background_none_toggle_tracks_transparency() {
        let _guard = crate::i18n::lock_for_test();
        assert_eq!(Field::BackgroundNone.kind(), FieldKind::Toggle);
        let mut draft = base_draft();
        // A preset with a concrete background reads as "not none".
        draft.theme.set_preset(crate::theme::ThemePreset::Midnight);
        assert!(!draft.theme.is_role_transparent(ThemeRole::Background));
        assert_eq!(draft.value_display(Field::BackgroundNone), "[ ]");
        // Forcing the override transparent flips the toggle on and persists.
        draft
            .theme
            .set_override(ThemeRole::Background, crate::theme::TRANSPARENT)
            .unwrap();
        assert!(draft.theme.is_role_transparent(ThemeRole::Background));
        assert_eq!(draft.value_display(Field::BackgroundNone), "[x]");
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert!(
            cfg.theme
                .normalized()
                .is_role_transparent(ThemeRole::Background)
        );
    }

    #[test]
    fn playback_tab_groups_now_playing_and_eq() {
        let f = SettingsTab::Playback.fields();
        // Speed + SeekInterval + WheelVolume + Gapless + MediaControls, then
        // EqPreset + ten bands + Normalize.
        assert_eq!(f.len(), 5 + 1 + eq::BANDS + 1);
        assert_eq!(f[0], Field::Speed);
        assert_eq!(f[1], Field::SeekInterval);
        assert_eq!(f[2], Field::MouseWheelVolume);
        assert_eq!(f[3], Field::Gapless);
        assert_eq!(f[4], Field::MediaControls);
        assert_eq!(f[5], Field::EqPreset);
        assert_eq!(f[5 + eq::BANDS + 1], Field::Normalize);
        assert_eq!(Field::MouseWheelVolume.kind(), FieldKind::Toggle);
        assert_eq!(base_draft().value_display(Field::MouseWheelVolume), "[x]");
        let total: usize = SettingsTab::Playback
            .sections()
            .iter()
            .map(|(_, n)| n)
            .sum();
        assert_eq!(total, f.len());
    }

    #[test]
    fn general_tab_has_search_options_and_autoplay_toggle() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::General.fields();
        assert_eq!(
            f,
            vec![
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
                Field::Mouse,
                Field::AlbumArt,
                Field::AutoplayOnStart,
                Field::EnqueueNext,
                Field::ResetKeybindings,
                Field::ResetAll,
            ]
        );
        assert_eq!(Field::ResetKeybindings.kind(), FieldKind::Button);
        assert_eq!(Field::ResetAll.kind(), FieldKind::Button);
        assert_eq!(Field::SearchSource.kind(), FieldKind::Select);
        assert_eq!(Field::StreamingSource.kind(), FieldKind::Select);
        assert_eq!(Field::SearchSoundCloud.kind(), FieldKind::Toggle);
        assert_eq!(Field::JamendoClientId.kind(), FieldKind::Text);
        assert_eq!(Field::AutoplayOnStart.kind(), FieldKind::Toggle);
        assert_eq!(Field::EnqueueNext.kind(), FieldKind::Toggle);
        assert_eq!(Field::AlbumArt.kind(), FieldKind::Toggle);
        // Off by default, and the toggle renders as an empty checkbox.
        let draft = base_draft();
        assert_eq!(
            draft.value_display(Field::ResetKeybindings),
            "↵ press Enter"
        );
        assert_eq!(draft.value_display(Field::SearchSource), "YouTube");
        assert_eq!(draft.value_display(Field::StreamingSource), "YouTube");
        assert_eq!(draft.value_display(Field::SearchSoundCloud), "[x]");
        assert_eq!(draft.value_display(Field::JamendoClientId), "(none)");
        assert!(!draft.autoplay_on_start);
        assert_eq!(draft.value_display(Field::AutoplayOnStart), "[ ]");
        assert!(!draft.enqueue_next);
        assert_eq!(draft.value_display(Field::EnqueueNext), "[ ]");
    }

    #[test]
    fn ai_tab_has_model_key_autoplay_and_streaming_mode() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::Ai.fields();
        assert_eq!(
            f,
            vec![
                Field::AiEnabled,
                Field::GeminiModel,
                Field::ApiKey,
                Field::RomanizedTitles,
                Field::ClearRomanizedTitleCache,
                Field::AutoplayStreaming,
                Field::StreamingMode,
            ]
        );
        assert_eq!(Field::AiEnabled.kind(), FieldKind::Toggle);
        assert!(!Field::AiEnabled.is_secret());
        // Enabled by default in a fresh draft.
        assert_eq!(base_draft().value_display(Field::AiEnabled), "[x]");
        assert_eq!(Field::GeminiModel.kind(), FieldKind::Select);
        assert_eq!(Field::ApiKey.kind(), FieldKind::Text);
        assert!(Field::ApiKey.is_secret());
        assert!(!Field::GeminiModel.is_secret());
        assert_eq!(Field::RomanizedTitles.kind(), FieldKind::Toggle);
        assert_eq!(base_draft().value_display(Field::RomanizedTitles), "[ ]");
        assert_eq!(Field::ClearRomanizedTitleCache.kind(), FieldKind::Button);
        assert_eq!(
            base_draft().value_display(Field::ClearRomanizedTitleCache),
            "↵ press Enter"
        );
        // The streaming mode is a non-secret cycle field.
        assert_eq!(Field::StreamingMode.kind(), FieldKind::Select);
        assert!(!Field::StreamingMode.is_secret());
        assert_eq!(base_draft().value_display(Field::StreamingMode), "Balanced");
    }

    #[test]
    fn theme_and_colors_are_editable_and_persistent() {
        // Retro mode + theme preset + transparent-bg toggle lead the Graphics tab; colors follow.
        let f = SettingsTab::Graphics.fields();
        assert_eq!(f[0], Field::RetroMode);
        assert_eq!(f[1], Field::ThemePreset);
        assert_eq!(f[2], Field::BackgroundNone);
        let color_fields: Vec<Field> = f
            .into_iter()
            .filter(|fld| matches!(fld, Field::ThemeColor(_)))
            .collect();
        assert_eq!(color_fields.len(), ThemeRole::ALL.len());
        assert!(matches!(
            color_fields[0],
            Field::ThemeColor(ThemeRole::Background)
        ));

        let mut draft = base_draft();
        draft.theme.set_preset(crate::theme::ThemePreset::Midnight);
        draft
            .theme
            .set_override(ThemeRole::Accent, "#123456")
            .unwrap();
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.theme.preset, "midnight");
        assert_eq!(
            cfg.theme.overrides.get("accent").map(String::as_str),
            Some("#123456")
        );
    }

    #[test]
    fn apply_to_persists_every_settings_field() {
        let mut bands = EqPreset::Flat.gains();
        bands[2] = 4.0;
        let mut theme = ThemeConfig::default();
        theme.set_preset(crate::theme::ThemePreset::HighContrast);
        theme
            .set_override(ThemeRole::BorderPrimary, "#123456")
            .unwrap();

        let draft = SettingsDraft {
            cookies_file: "/tmp/cookies.txt".to_owned(),
            download_dir: "/tmp/downloads".to_owned(),
            search: SearchConfig {
                source: SearchSource::SoundCloud,
                streaming_source: SearchSource::All,
                jamendo_client_id: Some("jam-id".to_owned()),
                ..SearchConfig::default()
            },
            mouse: false,
            album_art: true,
            autoplay_on_start: true,
            enqueue_next: true,
            speed: 1.7,
            seek_seconds: 25.0,
            mouse_wheel_volume: false,
            gapless: false,
            media_controls: false,
            autoplay_streaming: true,
            streaming_mode: StreamingMode::Discovery,
            eq_preset: EqPreset::Custom,
            eq_bands: bands,
            normalize: true,
            gemini_model: GeminiModel::Latest,
            gemini_api_key: "  AIzaPersist  ".to_owned(),
            ai_enabled: false,
            romanized_titles: true,
            theme,
            retro_mode: false,
            language: Language::Korean,
            animations: AnimationsConfig {
                master: true,
                border: true,
                fps: 45,
                pause_unfocused: false,
                ..Default::default()
            },
            lastfm_enabled: false,
            lastfm_love_sync: false,
            lastfm_session_key: "sk-abc".to_owned(),
            lastfm_username: "listener".to_owned(),
            listenbrainz_enabled: true,
            listenbrainz_token: "lb-tok".to_owned(),
            scrobble_local_files: false,
            spotify_client_id: "  spotify-cid  ".to_owned(),
            spotify_redirect_port: "9333".to_owned(),
            spotify_connected: true,
            spotify_username: "listener".to_owned(),
        };

        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.language, Language::Korean);
        assert_eq!(cfg.ai_enabled, Some(false));
        assert_eq!(cfg.romanized_titles, Some(true));
        assert!(cfg.animations.master);
        assert!(cfg.animations.border);
        assert!(!cfg.animations.rain);
        assert_eq!(cfg.animations.fps, 45);
        assert!(!cfg.animations.pause_unfocused);
        assert_eq!(cfg.cookies_file, Some(PathBuf::from("/tmp/cookies.txt")));
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/tmp/downloads")));
        assert_eq!(cfg.search.source, SearchSource::SoundCloud);
        assert_eq!(cfg.search.streaming_source, SearchSource::All);
        assert_eq!(cfg.search.jamendo_client_id.as_deref(), Some("jam-id"));
        assert_eq!(cfg.mouse, Some(false));
        assert_eq!(cfg.album_art, Some(true));
        assert_eq!(cfg.autoplay_on_start, Some(true));
        assert_eq!(cfg.enqueue_next, Some(true));
        assert_eq!(cfg.speed, Some(1.7));
        assert_eq!(cfg.seek_seconds, Some(25.0));
        assert_eq!(cfg.mouse_wheel_volume, Some(false));
        assert_eq!(cfg.gapless, Some(false));
        assert_eq!(cfg.media_controls, Some(false));
        assert_eq!(cfg.autoplay_streaming, Some(true));
        assert_eq!(cfg.streaming.mode, StreamingMode::Discovery);
        assert_eq!(cfg.scrobble.lastfm.enabled, Some(false));
        assert_eq!(cfg.scrobble.lastfm.love_sync, Some(false));
        assert_eq!(cfg.scrobble.lastfm.session_key.as_deref(), Some("sk-abc"));
        assert_eq!(cfg.scrobble.lastfm.username.as_deref(), Some("listener"));
        assert_eq!(cfg.scrobble.listenbrainz.enabled, Some(true));
        assert_eq!(cfg.scrobble.listenbrainz.token.as_deref(), Some("lb-tok"));
        assert_eq!(cfg.scrobble.local_files, Some(false));
        assert_eq!(cfg.spotify.client_id.as_deref(), Some("spotify-cid"));
        assert_eq!(cfg.spotify.redirect_port, Some(9333));
        assert_eq!(cfg.eq_preset, EqPreset::Custom);
        assert_eq!(cfg.eq_bands, Some(bands));
        assert_eq!(cfg.normalize, Some(true));
        assert_eq!(cfg.gemini_model, GeminiModel::Latest);
        assert_eq!(cfg.gemini_api_key.as_deref(), Some("AIzaPersist"));
        assert_eq!(cfg.theme.preset, "high_contrast");
        assert_eq!(
            cfg.theme
                .overrides
                .get("border_primary")
                .map(String::as_str),
            Some("#123456")
        );
    }

    #[test]
    fn api_key_display_is_masked() {
        let _guard = crate::i18n::lock_for_test();
        let mut draft = base_draft();
        assert_eq!(draft.value_display(Field::ApiKey), "(none)");
        draft.gemini_api_key = "AIzaSuperSecret".to_owned();
        assert_eq!(draft.value_display(Field::ApiKey), "***configured***");
    }

    #[test]
    fn apply_to_persists_ai_fields() {
        let mut draft = base_draft();
        draft.gemini_model = GeminiModel::Latest;
        draft.gemini_api_key = "  AIzaKey  ".to_owned();
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.gemini_model, GeminiModel::Latest);
        assert_eq!(cfg.gemini_api_key.as_deref(), Some("AIzaKey")); // trimmed
    }

    #[test]
    fn eq_section_lives_under_playback() {
        // EQ moved under Playback: preset, ten bands, then normalize, contiguous.
        let f = SettingsTab::Playback.fields();
        let preset_at = f.iter().position(|fld| *fld == Field::EqPreset).unwrap();
        for (k, fld) in f[preset_at + 1..=preset_at + eq::BANDS].iter().enumerate() {
            assert_eq!(*fld, Field::Band(k));
        }
        assert_eq!(f[preset_at + eq::BANDS + 1], Field::Normalize);
    }

    #[test]
    fn apply_to_stores_preset_without_band_array_when_unmodified() {
        let draft = SettingsDraft {
            download_dir: "  ".to_owned(),
            eq_preset: EqPreset::BassBoost,
            eq_bands: EqPreset::BassBoost.gains(),
            ..base_draft()
        };
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.eq_preset, EqPreset::BassBoost);
        assert_eq!(cfg.eq_bands, None); // matches preset → not stored explicitly
        assert_eq!(cfg.download_dir, None); // whitespace → none
        assert_eq!(cfg.cookies_file, None);
    }

    #[test]
    fn apply_to_stores_custom_band_array() {
        let mut bands = EqPreset::Flat.gains();
        bands[0] = 5.0;
        let draft = SettingsDraft {
            cookies_file: "/c.txt".to_owned(),
            mouse: false,
            autoplay_on_start: true,
            speed: 1.5,
            gapless: false,
            autoplay_streaming: true,
            eq_preset: EqPreset::Custom,
            eq_bands: bands,
            normalize: true,
            ..base_draft()
        };
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.eq_bands, Some(bands));
        assert_eq!(cfg.speed, Some(1.5));
        assert_eq!(cfg.cookies_file, Some(PathBuf::from("/c.txt")));
        assert_eq!(cfg.mouse, Some(false));
        assert_eq!(cfg.autoplay_on_start, Some(true));
    }

    #[test]
    fn band_and_speed_clamps() {
        assert_eq!(clamp_band(99.0), BAND_GAIN_MAX);
        assert_eq!(clamp_band(-99.0), BAND_GAIN_MIN);
        assert_eq!(clamp_speed(9.0), SPEED_MAX);
        assert_eq!(clamp_speed(0.0), SPEED_MIN);
    }

    #[test]
    fn freq_labels_read_naturally() {
        assert_eq!(freq_label(0), "31 Hz");
        assert_eq!(freq_label(5), "1 kHz");
        assert_eq!(freq_label(9), "16 kHz");
    }
}
