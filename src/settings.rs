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
use crate::config::{AnimationsConfig, Config, default_cookies_file, default_download_dir};
use crate::eq::{self, EqPreset};
use crate::i18n::Language;
use crate::keymap::{Action, KeyContext, KeyMap};
use crate::search_source::SearchConfig;
use crate::streaming::{CuratingMode, StreamingMode};
use crate::t;
use crate::theme::{ThemeConfig, ThemeRole};

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
                Field::BigText,
                Field::AutoplayOnStart,
                Field::EnqueueNext,
                Field::UpdateCheck,
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
                    Field::EqPreset,
                ];
                f.extend((0..eq::BANDS).map(Field::Band));
                f.push(Field::Normalize);
                f
            }
            // Theme (preset + transparent-bg toggle), then every color role, then the
            // animation toggles (master kill-switch first, then the behaviour knobs, the
            // player element effects, the player one-shots, the UI-wide effects, and finally
            // the filler-canvas set — the same grouping as `AnimationsConfig`).
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
                    Field::AnimTrackIntro,
                    Field::AnimLyrics,
                    Field::AnimToast,
                    Field::AnimVolumeFlash,
                    Field::AnimLikeBurst,
                    Field::AnimSeekFlash,
                    Field::AnimSelection,
                    Field::AnimStagger,
                    Field::AnimCaret,
                    Field::AnimTabs,
                    Field::AnimPopupFade,
                    Field::AnimActivity,
                    Field::AnimAboutFx,
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
                (t!("EQ", "EQ"), eq::BANDS + 2),
            ],
            SettingsTab::Graphics => vec![
                (t!("Theme", "테마"), 3),
                (t!("Colors", "색상"), ThemeRole::ALL.len()),
                (t!("Animations", "애니메이션"), 28),
            ],
            SettingsTab::Accounts => vec![
                ("Last.fm", 3),
                ("ListenBrainz", 2),
                ("Spotify", 4),
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
    Mouse,
    AlbumArt,
    AutoplayOnStart,
    /// Manual "add to queue" inserts immediately after the current track instead of at the end.
    EnqueueNext,
    /// Check GitHub on startup for a newer ytm-tui release (the About-card update notice).
    UpdateCheck,
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
    // UI-wide effects (Search / Library / Settings / DJ Gem / popups).
    AnimSelection,
    AnimStagger,
    AnimCaret,
    AnimTabs,
    AnimPopupFade,
    AnimActivity,
    AnimAboutFx,
    // Filler-canvas effects.
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
            | Field::BigText
            | Field::MouseWheelVolume
            | Field::Gapless
            | Field::MediaControls
            | Field::AutoContinueVideos
            | Field::UpdateCheck
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
            | Field::AnimTrackIntro
            | Field::AnimLyrics
            | Field::AnimToast
            | Field::AnimVolumeFlash
            | Field::AnimLikeBurst
            | Field::AnimSeekFlash
            | Field::AnimSelection
            | Field::AnimStagger
            | Field::AnimCaret
            | Field::AnimTabs
            | Field::AnimPopupFade
            | Field::AnimActivity
            | Field::AnimAboutFx
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
            | Field::CuratingMode
            | Field::DjGemLanguage
            | Field::VideoLayout
            | Field::StreamingMode => FieldKind::Select,
            Field::Speed | Field::SeekInterval | Field::Band(_) | Field::AnimFps => {
                FieldKind::Slider
            }
            Field::ResetKeybindings
            | Field::ResetAll
            | Field::ClearRomanizedTitleCache
            | Field::RadioRecording
            | Field::LastfmConnect
            | Field::SpotifyConnect
            | Field::SpotifyImport => FieldKind::Button,
        }
    }

    /// For an animation toggle field, a mutable handle to its flag inside an
    /// [`AnimationsConfig`]; `None` for any non-animation field. This single mapping is the
    /// source of truth used for both rendering the checkbox and flipping it on input — so the
    /// 26 effects never drift out of sync across the display / toggle / persist paths.
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
            Field::AnimTrackIntro => &mut a.track_intro,
            Field::AnimLyrics => &mut a.lyrics,
            Field::AnimToast => &mut a.toast,
            Field::AnimVolumeFlash => &mut a.volume_flash,
            Field::AnimLikeBurst => &mut a.like_burst,
            Field::AnimSeekFlash => &mut a.seek_flash,
            Field::AnimSelection => &mut a.selection,
            Field::AnimStagger => &mut a.stagger,
            Field::AnimCaret => &mut a.caret,
            Field::AnimTabs => &mut a.tabs,
            Field::AnimPopupFade => &mut a.popup_fade,
            Field::AnimActivity => &mut a.activity,
            Field::AnimAboutFx => &mut a.about_fx,
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
            Field::AlbumArt => t!("Album art", "앨범 아트").to_owned(),
            Field::AutoplayOnStart => t!("Autoplay on launch", "앱 시작 시 자동재생").to_owned(),
            Field::EnqueueNext => t!("Enqueue as next", "큐 추가: 다음 곡").to_owned(),
            Field::UpdateCheck => t!("Check for updates", "업데이트 확인").to_owned(),
            Field::ResetKeybindings => t!("Reset keybindings", "단축키 초기화").to_owned(),
            Field::ResetAll => t!("Reset all settings", "모든 설정 초기화").to_owned(),
            Field::BigText => t!("Large text", "큰 글자 모드").to_owned(),
            Field::Speed => t!("Playback speed", "재생 속도").to_owned(),
            Field::SeekInterval => t!("Seek interval", "탐색 간격").to_owned(),
            Field::MouseWheelVolume => t!("Wheel volume", "휠 볼륨 조절").to_owned(),
            Field::Gapless => t!("Gapless (next launch)", "갭리스 (재시작 후 적용)").to_owned(),
            Field::MediaControls => t!("OS media controls", "OS 미디어 컨트롤").to_owned(),
            Field::AutoContinueVideos => {
                t!("Auto-continue videos", "영상 자동 이어재생").to_owned()
            }
            Field::VideoLayout => t!("Video window", "영상 창").to_owned(),
            Field::RadioRecording => t!("Radio recording", "라디오 녹음").to_owned(),
            Field::AutoplayStreaming => t!("Autoplay", "자동재생").to_owned(),
            Field::CuratingMode => t!("Curating mode", "큐레이팅 방식").to_owned(),
            Field::StreamingMode => t!("Curating style", "큐레이팅 스타일").to_owned(),
            Field::EqPreset => t!("Preset", "프리셋").to_owned(),
            Field::Band(i) => format!("{:>5}", freq_label(i)),
            Field::Normalize => t!("Normalize (loudness)", "음량 평준화").to_owned(),
            Field::AiEnabled => t!("DJ Gem chat", "DJ Gem 채팅").to_owned(),
            Field::GeminiModel => t!("Model", "모델").to_owned(),
            Field::ApiKey => t!("API key", "API 키").to_owned(),
            Field::DjGemLanguage => t!("Reply language", "답변 언어").to_owned(),
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
            Field::AnimTrackIntro => t!("Track intro reveal", "곡 시작 타이틀 등장").to_owned(),
            Field::AnimLyrics => t!("Lyrics glow", "가사 글로우").to_owned(),
            Field::AnimToast => t!("Status typewriter", "상태 메시지 타자기").to_owned(),
            Field::AnimVolumeFlash => t!("Volume flash", "볼륨 플래시").to_owned(),
            Field::AnimLikeBurst => t!("Like heart burst", "좋아요 하트 팡").to_owned(),
            Field::AnimSeekFlash => t!("Seek ripple", "탐색 물결").to_owned(),
            Field::AnimSelection => t!("Selection breathing", "선택 줄 숨쉬기").to_owned(),
            Field::AnimStagger => t!("List cascade", "목록 캐스케이드").to_owned(),
            Field::AnimCaret => t!("Caret blink", "커서 깜빡임").to_owned(),
            Field::AnimTabs => t!("Tab pop", "탭 강조 팝").to_owned(),
            Field::AnimPopupFade => t!("Popup fade-in", "팝업 페이드인").to_owned(),
            Field::AnimActivity => t!("Activity dots", "진행 표시 점").to_owned(),
            Field::AnimAboutFx => t!("About sparkles", "정보 카드 반짝임").to_owned(),
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
            Field::UpdateCheck => toggle_str(self.update_check_enabled),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::SeekInterval => format!("{:.0}s", self.seek_seconds),
            Field::BigText => toggle_str(self.big_text),
            Field::MouseWheelVolume => toggle_str(self.mouse_wheel_volume),
            // Buttons, not values: these rows show how to trigger them.
            Field::ResetKeybindings | Field::ResetAll | Field::ClearRomanizedTitleCache => {
                t!("↵ press Enter", "↵ Enter로 실행").to_owned()
            }
            Field::Gapless => toggle_str(self.gapless),
            Field::MediaControls => toggle_str(self.media_controls),
            Field::AutoContinueVideos => toggle_str(self.auto_continue_videos),
            Field::VideoLayout => self.video_layout.label().to_owned(),
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
            // All 26 animation toggles render as a checkbox; one mapping (`anim_flag`) reads
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
            | Field::AnimSelection
            | Field::AnimStagger
            | Field::AnimCaret
            | Field::AnimTabs
            | Field::AnimPopupFade
            | Field::AnimActivity
            | Field::AnimAboutFx
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

pub(crate) fn blank_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

// Band gain limits + clamp live in `eq`; speed/seek clamps live in `config`. Each is the
// single source both the TUI controls and the headless daemon apply. Re-exported so
// existing `settings::clamp_band` / `settings::clamp_speed` / `settings::clamp_seek_seconds`
// call sites keep resolving.
pub use crate::config::{clamp_seek_seconds, clamp_speed};
pub use crate::eq::{BAND_GAIN_MAX, BAND_GAIN_MIN, clamp_band};

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
            update_check_enabled: true,
            speed: 1.0,
            seek_seconds: 10.0,
            big_text: false,
            big_text_percent: 150,
            mouse_wheel_volume: true,
            gapless: true,
            media_controls: true,
            auto_continue_videos: false,
            video_layout: crate::config::VideoOverlay::Compact,
            autoplay_streaming: false,
            curating_mode: CuratingMode::DjGem,
            streaming_mode: StreamingMode::Balanced,
            eq_preset: EqPreset::Flat,
            eq_bands: EqPreset::Flat.gains(),
            normalize: false,
            gemini_model: GeminiModel::default(),
            gemini_api_key: String::new(),
            ai_enabled: true,
            romanized_titles: false,
            dj_gem_language: crate::i18n::DjGemLanguage::Auto,
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
            spotify_stale: false,
            spotify_username: String::new(),
            recording_mode: crate::recorder::RecordingMode::Nothing,
            recording_min_seconds: 30,
            recording_max_seconds: 900,
            recording_dir: String::new(),
            recording_past_tracks: 10,
            recording_notify: true,
        }
    }

    /// A `SettingsState` on the given tab for partition tests.
    fn state_on(tab: SettingsTab, radio_mode: bool, retro: bool) -> SettingsState {
        let mut draft = base_draft();
        draft.retro_mode = retro;
        SettingsState {
            tab,
            row: 0,
            draft,
            editing_text: false,
            secret_restore: None,
            keymap: KeyMap::default(),
            capturing: None,
            radio_mode,
        }
    }

    #[test]
    fn playback_sections_partition_in_both_radio_states() {
        let _guard = crate::i18n::lock_for_test();
        for radio in [false, true] {
            let st = state_on(SettingsTab::Playback, radio, false);
            let sum: usize = st.sections().iter().map(|(_, n)| n).sum();
            assert_eq!(sum, st.fields().len(), "radio={radio}");
            assert_eq!(st.fields().contains(&Field::RadioRecording), radio);
        }
    }

    #[test]
    fn ai_sections_partition_under_retro() {
        let _guard = crate::i18n::lock_for_test();
        for retro in [false, true] {
            let st = state_on(SettingsTab::Ai, false, retro);
            let sum: usize = st.sections().iter().map(|(_, n)| n).sum();
            assert_eq!(sum, st.fields().len(), "retro={retro}");
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
        // ThemePreset + BackgroundNone + every color role + 28 animation fields
        // plus the RetroMode fallback toggle.
        // (master enable + fps slider + pause-when-unfocused toggle + 25 per-effect toggles).
        assert_eq!(f.len(), 3 + ThemeRole::ALL.len() + 28);
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
        // Speed + SeekInterval + WheelVolume + Gapless + MediaControls + AutoContinueVideos +
        // VideoLayout + RadioRecording (radio-only), then EqPreset + ten bands + Normalize.
        assert_eq!(f.len(), 8 + 1 + eq::BANDS + 1);
        assert_eq!(f[0], Field::Speed);
        assert_eq!(f[1], Field::SeekInterval);
        assert_eq!(f[2], Field::MouseWheelVolume);
        assert_eq!(f[3], Field::Gapless);
        assert_eq!(f[4], Field::MediaControls);
        assert_eq!(f[5], Field::AutoContinueVideos);
        assert_eq!(f[6], Field::VideoLayout);
        assert_eq!(f[7], Field::RadioRecording);
        assert_eq!(f[8], Field::EqPreset);
        assert_eq!(f[8 + eq::BANDS + 1], Field::Normalize);
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
                Field::BigText,
                Field::AutoplayOnStart,
                Field::EnqueueNext,
                Field::UpdateCheck,
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
                Field::DjGemLanguage,
                Field::RomanizedTitles,
                Field::ClearRomanizedTitleCache,
                Field::AutoplayStreaming,
                Field::CuratingMode,
                Field::StreamingMode,
            ]
        );
        // Section header counts must partition the fields exactly, or the renderer drops the tail.
        let secs: usize = SettingsTab::Ai.sections().iter().map(|(_, n)| n).sum();
        assert_eq!(secs, SettingsTab::Ai.fields().len());
        assert_eq!(Field::AiEnabled.kind(), FieldKind::Toggle);
        assert!(!Field::AiEnabled.is_secret());
        // Enabled by default in a fresh draft.
        assert_eq!(base_draft().value_display(Field::AiEnabled), "[x]");
        assert_eq!(Field::GeminiModel.kind(), FieldKind::Select);
        assert_eq!(Field::ApiKey.kind(), FieldKind::Text);
        assert!(Field::ApiKey.is_secret());
        assert!(!Field::GeminiModel.is_secret());
        // Reply language is a non-secret cycle field defaulting to Auto.
        assert_eq!(Field::DjGemLanguage.kind(), FieldKind::Select);
        assert!(!Field::DjGemLanguage.is_secret());
        assert_eq!(
            base_draft().value_display(Field::DjGemLanguage),
            "Auto (interface)"
        );
        assert_eq!(Field::RomanizedTitles.kind(), FieldKind::Toggle);
        assert_eq!(base_draft().value_display(Field::RomanizedTitles), "[ ]");
        assert_eq!(Field::ClearRomanizedTitleCache.kind(), FieldKind::Button);
        assert_eq!(
            base_draft().value_display(Field::ClearRomanizedTitleCache),
            "↵ press Enter"
        );
        // Curating mode + style are non-secret cycle fields; both default to DJ Gem / Balanced.
        assert_eq!(Field::CuratingMode.kind(), FieldKind::Select);
        assert!(!Field::CuratingMode.is_secret());
        assert_eq!(base_draft().value_display(Field::CuratingMode), "DJ Gem");
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
    fn retro_mode_commits_the_edited_theme_but_keeps_english() {
        // Retro used to overwrite the committed theme with a fresh Retro preset (wiping
        // the user's preset + color overrides on disk). Now the theme commits as edited;
        // only the UI language stays forced to English.
        let mut draft = base_draft();
        draft.retro_mode = true;
        draft.language = crate::i18n::Language::Korean;
        draft.theme.set_preset(crate::theme::ThemePreset::Nord);
        draft
            .theme
            .set_override(ThemeRole::Accent, "#ABCDEF")
            .unwrap();
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert!(cfg.retro_mode);
        assert_eq!(cfg.theme.preset, "nord");
        assert_eq!(
            cfg.theme.overrides.get("accent").map(String::as_str),
            Some("#ABCDEF")
        );
        assert_eq!(cfg.language, crate::i18n::Language::English);
        // And the runtime theme honors the user's choice under retro mode too.
        assert_eq!(cfg.effective_theme().preset, "nord");
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
            update_check_enabled: false,
            speed: 1.7,
            seek_seconds: 25.0,
            big_text: false,
            big_text_percent: 150,
            mouse_wheel_volume: false,
            gapless: false,
            media_controls: false,
            auto_continue_videos: true,
            video_layout: crate::config::VideoOverlay::Fullscreen,
            autoplay_streaming: true,
            curating_mode: CuratingMode::YtNative,
            streaming_mode: StreamingMode::Discovery,
            eq_preset: EqPreset::Custom,
            eq_bands: bands,
            normalize: true,
            gemini_model: GeminiModel::Latest,
            gemini_api_key: "  AIzaPersist  ".to_owned(),
            ai_enabled: false,
            romanized_titles: true,
            dj_gem_language: crate::i18n::DjGemLanguage::Japanese,
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
            spotify_stale: false,
            spotify_username: "listener".to_owned(),
            recording_mode: crate::recorder::RecordingMode::Decide,
            recording_min_seconds: 20,
            recording_max_seconds: 1200,
            recording_dir: "/tmp/recs".to_owned(),
            recording_past_tracks: 25,
            recording_notify: false,
        };

        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.recording.mode, crate::recorder::RecordingMode::Decide);
        assert_eq!(cfg.recording.min_duration_secs, 20);
        assert_eq!(cfg.recording.max_duration_secs, 1200);
        assert_eq!(
            cfg.recording.track_directory,
            Some(PathBuf::from("/tmp/recs"))
        );
        assert_eq!(cfg.recording.past_tracks_count, 25);
        assert!(!cfg.recording.notify);
        assert_eq!(cfg.language, Language::Korean);
        // The raw pick round-trips (not retro-forced here); `Auto` would too.
        assert_eq!(cfg.dj_gem_language, crate::i18n::DjGemLanguage::Japanese);
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
        assert!(!cfg.update_check_enabled);
        assert_eq!(cfg.speed, Some(1.7));
        assert_eq!(cfg.seek_seconds, Some(25.0));
        assert_eq!(cfg.mouse_wheel_volume, Some(false));
        assert_eq!(cfg.gapless, Some(false));
        assert_eq!(cfg.media_controls, Some(false));
        assert_eq!(cfg.auto_continue_videos, Some(true));
        assert_eq!(cfg.video_layout, crate::config::VideoOverlay::Fullscreen);
        assert_eq!(cfg.autoplay_streaming, Some(true));
        assert_eq!(cfg.streaming.mode, StreamingMode::Discovery);
        // Curating mode = YT Native → the AI rerank flag persists as false.
        assert!(!cfg.streaming.ai.enabled);
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
    fn big_text_toggle_maps_to_the_zoom_level() {
        // ON with no prior zoom: enables the mode-preferred level.
        let mut draft = base_draft();
        draft.big_text = true;
        draft.big_text_percent = 200;
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.text_zoom, Some(200));

        // ON with a custom wheel-set level already in the config: keeps it.
        let mut cfg = Config {
            text_zoom: Some(250),
            ..Config::default()
        };
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.text_zoom, Some(250));

        // OFF always returns to normal size.
        draft.big_text = false;
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.text_zoom, Some(100));
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
        assert_eq!(clamp_speed(9.0), crate::config::SPEED_MAX);
        assert_eq!(clamp_speed(0.0), crate::config::SPEED_MIN);
    }

    #[test]
    fn freq_labels_read_naturally() {
        assert_eq!(freq_label(0), "31 Hz");
        assert_eq!(freq_label(5), "1 kHz");
        assert_eq!(freq_label(9), "16 kHz");
    }
}
