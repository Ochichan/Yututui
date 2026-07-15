//! Central keybinding map: the single source of truth for which key triggers which
//! semantic [`Action`], per input [`KeyContext`].
//!
//! Key handling used to be inline `match k.code` literals scattered across the five
//! `on_key_*` methods, and the on-screen hints were hand-synced string constants. This
//! module decouples *intent* (`Action`) from the physical key ([`Chord`]): handlers
//! resolve an `Action` for their context and act on it, while footers and the `?`
//! cheat-sheet render the bound chords back out — so hints can never drift from behavior.
//!
//! Bindings are user-remappable (the Settings → Keys tab) and persisted to `config.json`
//! as `"<context>.<action>" -> "<chord>"`, storing only entries that differ from the
//! built-in defaults so old configs and future new actions keep working.

use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MediaKeyCode, ModifierKeyCode};

use crate::t;

mod compat;
mod display;
pub use display::{format_chord, format_chord_for_display, format_chord_retro};

/// A semantic command, decoupled from the physical key that triggers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // Player transport / playback.
    TogglePause,
    SeekBack,
    SeekForward,
    VolUp,
    VolDown,
    ToggleMute,
    NextTrack,
    PrevTrack,
    Favorite,
    CycleRating,
    OpenLibrary,
    OpenQueue,
    ToggleLyrics,
    LyricsDelayEarlier,
    LyricsDelayLater,
    Download,
    /// Download every song in the current list/playlist at once (deduped), distinct from the
    /// single-track `Download`.
    DownloadAll,
    AcceptAllImportReview,
    ToggleShuffle,
    CycleRepeat,
    CycleEq,
    ToggleNormalize,
    SpeedUp,
    SpeedDown,
    OpenSettings,
    OpenAi,
    OpenSearch,
    /// Open the collection-wide Local Find surface while dedicated Local Deck mode is active.
    OpenLocalFind,
    Quit,
    Home,
    // Shared navigation (interpreted per context).
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    // Shift+nav range multi-select: extend the anchor..=cursor selection instead of
    // collapsing it (the keyboard mirror of a mouse drag-select). Only the Library and
    // Queue surfaces act on these; other list contexts ignore them.
    SelectUp,
    SelectDown,
    SelectPageUp,
    SelectPageDown,
    SelectToTop,
    SelectToBottom,
    Confirm,
    Enqueue,
    Back,
    FocusNext,
    FocusPrev,
    DeleteChar,
    DeleteWord,
    SelectAll,
    ToggleSearchSourceMenu,
    /// Search box: flip between searching tracks and public YouTube playlists.
    ToggleSearchKind,
    // Queue window.
    QueueRemove,
    // Library list.
    LibraryRemove,
    LibraryFilter,
    PlayAll,
    // Playlists tab.
    PlaylistCreate,
    AddToPlaylist,
    // Settings screen.
    SettingsCancel,
    ChangeDecrease,
    ChangeIncrease,
    // Search / DJ Gem results.
    FocusInput,
    /// Search results: open the results-filter popup.
    SearchFilter,
    // Global (active in any non-text-entry context).
    ToggleStreaming,
    ToggleRadioMode,
    ToggleLocalMode,
    ToggleHelp,
    /// Open the selected row's context menu (keyboard accessibility fallback).
    OpenContextMenu,
    ToggleAbout,
    ToggleAnimations,
    /// Collapse/expand the docked control box on non-Player screens (Bottom bar mode).
    ToggleControlBox,
    WhyAi,
    TextZoomIn,
    TextZoomOut,
    ToggleZoomWheelLock,
    // Player extras: copy link + external mpv video overlay.
    CopyLink,
    PlayVideo,
    ToggleVideoLayout,
    VideoTogglePause,
    VideoNext,
    VideoPrev,
    VideoClose,
    VideoToggleFullscreen,
    VideoToggleMute,
    /// The "what's playing" (지듣노) radio identify overlay.
    IdentifyNowPlaying,
    /// Open the radio recordings browser (Decide-mode save/discard/play).
    ToggleRecordings,
    // Inside the "what's playing" card.
    NowPlayingFavorite,
    NowPlayingAskAi,
}

/// Stable id (for config keys) + English + Korean human label (for the editor and
/// cheat-sheet), in a single table so they never fall out of sync. The `id` is never
/// translated — it is the persisted config key.
const ACTION_META: &[(Action, &str, &str, &str)] = &[
    (
        Action::TogglePause,
        "toggle_pause",
        "Play / pause",
        "재생 / 일시정지",
    ),
    (Action::SeekBack, "seek_back", "Seek backward", "뒤로 이동"),
    (
        Action::SeekForward,
        "seek_forward",
        "Seek forward",
        "앞으로 이동",
    ),
    (Action::VolUp, "vol_up", "Volume up", "볼륨 올리기"),
    (Action::VolDown, "vol_down", "Volume down", "볼륨 내리기"),
    (
        Action::ToggleMute,
        "toggle_mute",
        "Mute / unmute",
        "음소거 / 해제",
    ),
    (Action::NextTrack, "next_track", "Next track", "다음 곡"),
    (Action::PrevTrack, "prev_track", "Previous track", "이전 곡"),
    (
        Action::Favorite,
        "favorite",
        "Favorite / unfavorite",
        "즐겨찾기 추가 / 해제",
    ),
    (
        Action::CycleRating,
        "cycle_rating",
        "Rate: like / dislike",
        "평가: 좋아요 / 싫어요",
    ),
    (
        Action::OpenLibrary,
        "open_library",
        "Open library",
        "라이브러리 열기",
    ),
    (Action::OpenQueue, "open_queue", "Open queue", "대기열 열기"),
    (
        Action::ToggleLyrics,
        "toggle_lyrics",
        "Toggle lyrics",
        "가사 켜기 / 끄기",
    ),
    (
        Action::LyricsDelayEarlier,
        "lyrics_delay_earlier",
        "Lyrics earlier",
        "가사 앞당기기",
    ),
    (
        Action::LyricsDelayLater,
        "lyrics_delay_later",
        "Lyrics later",
        "가사 늦추기",
    ),
    (
        Action::Download,
        "download",
        "Download track",
        "곡 다운로드",
    ),
    (
        Action::DownloadAll,
        "download_all",
        "Download all",
        "전체 다운로드",
    ),
    (
        Action::AcceptAllImportReview,
        "accept_all_import_review",
        "Mark all import tracks Ready",
        "임포트 곡 전체 준비 완료",
    ),
    (
        Action::ToggleShuffle,
        "toggle_shuffle",
        "Toggle shuffle",
        "셔플 켜기 / 끄기",
    ),
    (
        Action::CycleRepeat,
        "cycle_repeat",
        "Cycle repeat",
        "반복 모드 전환",
    ),
    (
        Action::CycleEq,
        "cycle_eq",
        "Cycle EQ preset",
        "EQ 프리셋 전환",
    ),
    (
        Action::ToggleNormalize,
        "toggle_normalize",
        "Toggle normalization",
        "음량 평준화 켜기 / 끄기",
    ),
    (Action::SpeedUp, "speed_up", "Speed up", "재생 속도 올리기"),
    (
        Action::SpeedDown,
        "speed_down",
        "Speed down",
        "재생 속도 내리기",
    ),
    (
        Action::OpenSettings,
        "open_settings",
        "Open settings",
        "설정 열기",
    ),
    (
        Action::OpenAi,
        "open_ai",
        "Open DJ Gem assistant",
        "DJ Gem 어시스턴트 열기",
    ),
    (
        Action::OpenSearch,
        "open_search",
        "Open search",
        "검색 열기",
    ),
    (Action::Quit, "quit", "Quit", "종료"),
    (Action::Home, "home", "Go home", "홈으로"),
    (Action::MoveUp, "move_up", "Move up", "위로 이동"),
    (Action::MoveDown, "move_down", "Move down", "아래로 이동"),
    (Action::PageUp, "page_up", "Page up", "페이지 위로"),
    (Action::PageDown, "page_down", "Page down", "페이지 아래로"),
    (Action::JumpTop, "jump_top", "Jump to top", "맨 위로"),
    (
        Action::JumpBottom,
        "jump_bottom",
        "Jump to bottom",
        "맨 아래로",
    ),
    (
        Action::SelectUp,
        "select_up",
        "Extend selection up",
        "선택 위로 확장",
    ),
    (
        Action::SelectDown,
        "select_down",
        "Extend selection down",
        "선택 아래로 확장",
    ),
    (
        Action::SelectPageUp,
        "select_page_up",
        "Extend selection a page up",
        "선택 페이지 위로",
    ),
    (
        Action::SelectPageDown,
        "select_page_down",
        "Extend selection a page down",
        "선택 페이지 아래로",
    ),
    (
        Action::SelectToTop,
        "select_to_top",
        "Extend selection to top",
        "선택 맨 위까지",
    ),
    (
        Action::SelectToBottom,
        "select_to_bottom",
        "Extend selection to bottom",
        "선택 맨 아래까지",
    ),
    (
        Action::Confirm,
        "confirm",
        "Confirm / select",
        "확인 / 선택",
    ),
    (Action::Enqueue, "enqueue", "Add to queue", "큐에 추가"),
    (Action::Back, "back", "Back / close", "뒤로 / 닫기"),
    (
        Action::FocusNext,
        "focus_next",
        "Next tab / focus",
        "다음 탭 / 포커스",
    ),
    (
        Action::FocusPrev,
        "focus_prev",
        "Previous tab / focus",
        "이전 탭 / 포커스",
    ),
    (
        Action::DeleteChar,
        "delete_char",
        "Delete character",
        "문자 삭제",
    ),
    (
        Action::DeleteWord,
        "delete_word",
        "Delete previous word (text inputs)",
        "이전 단어 삭제 (텍스트 입력)",
    ),
    (Action::SelectAll, "select_all", "Select all", "전체 선택"),
    (
        Action::ToggleSearchSourceMenu,
        "toggle_search_source_menu",
        "Search source menu",
        "검색 소스 메뉴",
    ),
    (
        Action::ToggleSearchKind,
        "toggle_search_kind",
        "Search songs / playlists",
        "검색: 곡 / 플레이리스트",
    ),
    (
        Action::OpenLocalFind,
        "open_local_find",
        "Open Local Find",
        "로컬 찾기 열기",
    ),
    (
        Action::QueueRemove,
        "queue_remove",
        "Remove from queue",
        "대기열에서 제거",
    ),
    (
        Action::LibraryRemove,
        "library_remove",
        "Remove / delete",
        "제거 / 삭제",
    ),
    (
        Action::LibraryFilter,
        "library_filter",
        "Filter library",
        "라이브러리 필터",
    ),
    (
        Action::SearchFilter,
        "search_filter",
        "Filter results (popup)",
        "결과 필터 (팝업)",
    ),
    (
        Action::PlayAll,
        "play_all",
        "Play whole tab",
        "탭 전체 재생",
    ),
    (
        Action::PlaylistCreate,
        "playlist_create",
        "New playlist",
        "새 플레이리스트",
    ),
    (
        Action::AddToPlaylist,
        "add_to_playlist",
        "Add to playlist",
        "플레이리스트에 추가",
    ),
    (
        Action::SettingsCancel,
        "settings_cancel",
        "Close settings",
        "설정 저장 후 닫기",
    ),
    (
        Action::ChangeDecrease,
        "change_decrease",
        "Decrease value",
        "값 낮추기",
    ),
    (
        Action::ChangeIncrease,
        "change_increase",
        "Increase value",
        "값 높이기",
    ),
    (
        Action::FocusInput,
        "focus_input",
        "Focus input box",
        "입력창으로 이동",
    ),
    (
        Action::ToggleStreaming,
        "toggle_streaming",
        "Toggle autoplay",
        "자동재생 켜기 / 끄기",
    ),
    (
        Action::ToggleRadioMode,
        "toggle_radio_mode",
        "Radio/Normal mode",
        "라디오/일반 모드",
    ),
    (
        Action::ToggleLocalMode,
        "toggle_local_mode",
        "Local Deck mode",
        "로컬 덱 모드",
    ),
    (
        Action::ToggleHelp,
        "toggle_help",
        "Toggle help",
        "도움말 켜기 / 끄기",
    ),
    (
        Action::OpenContextMenu,
        "open_context_menu",
        "Open context menu",
        "문맥 메뉴 열기",
    ),
    (
        Action::ToggleAbout,
        "toggle_about",
        "About YuTuTui!",
        "YuTuTui! 정보",
    ),
    (
        Action::ToggleAnimations,
        "toggle_animations",
        "Toggle animations",
        "애니메이션 켜기 / 끄기",
    ),
    (
        Action::ToggleControlBox,
        "toggle_control_box",
        "Collapse / expand player bar",
        "플레이어 바 접기 / 펼치기",
    ),
    (
        Action::WhyAi,
        "why_ai",
        "Why these DJ Gem picks",
        "DJ Gem 선곡 이유 보기",
    ),
    (
        Action::IdentifyNowPlaying,
        "identify_now_playing",
        "What's playing (radio)",
        "지금 듣는 노래 (라디오)",
    ),
    (
        Action::ToggleRecordings,
        "toggle_recordings",
        "Radio recordings",
        "라디오 녹음 목록",
    ),
    (
        Action::NowPlayingFavorite,
        "now_playing_favorite",
        "Save to music favorites",
        "음악 즐겨찾기에 추가",
    ),
    (
        Action::NowPlayingAskAi,
        "now_playing_ask_ai",
        "Tell me more (DJ Gem)",
        "DJ Gem에게 더 알아보기",
    ),
    (
        Action::TextZoomIn,
        "text_zoom_in",
        "Text size up",
        "글자 확대",
    ),
    (
        Action::TextZoomOut,
        "text_zoom_out",
        "Text size down",
        "글자 축소",
    ),
    (
        Action::ToggleZoomWheelLock,
        "toggle_zoom_wheel_lock",
        "Ctrl+wheel zoom lock",
        "Ctrl+휠 확대 잠금",
    ),
    (
        Action::CopyLink,
        "copy_link",
        "Copy track link",
        "트랙 링크 복사",
    ),
    (
        Action::PlayVideo,
        "play_video",
        "Video overlay (mpv)",
        "영상 오버레이 (mpv)",
    ),
    (
        Action::ToggleVideoLayout,
        "toggle_video_layout",
        "Video size / position",
        "영상 크기 / 위치",
    ),
    (
        Action::VideoTogglePause,
        "video_toggle_pause",
        "Video play / pause",
        "영상 재생 / 일시정지",
    ),
    (Action::VideoNext, "video_next", "Next video", "다음 영상"),
    (
        Action::VideoPrev,
        "video_prev",
        "Previous video",
        "이전 영상",
    ),
    (
        Action::VideoClose,
        "video_close",
        "Close video",
        "영상 닫기",
    ),
    (
        Action::VideoToggleFullscreen,
        "video_toggle_fullscreen",
        "Fullscreen",
        "전체 화면",
    ),
    (
        Action::VideoToggleMute,
        "video_toggle_mute",
        "Mute / unmute",
        "음소거 / 해제",
    ),
];

impl Action {
    /// The stable identifier used in `config.json` keys.
    pub fn id(self) -> &'static str {
        ACTION_META
            .iter()
            .find(|(a, ..)| *a == self)
            .map(|(_, id, ..)| *id)
            .unwrap_or("?")
    }

    /// A human-readable name for the editor / cheat-sheet, in the active UI language.
    pub fn human_label(self) -> &'static str {
        ACTION_META
            .iter()
            .find(|(a, ..)| *a == self)
            .map(|(_, _, en, ko)| if crate::i18n::is_korean() { *ko } else { *en })
            .unwrap_or("?")
    }

    /// A human-readable label when the same action needs screen-specific wording.
    pub fn human_label_for(self, ctx: KeyContext) -> &'static str {
        match (ctx, self) {
            (KeyContext::Player, Action::QueueRemove) => {
                t!("Remove current from queue", "현재 곡 큐에서 제거")
            }
            (KeyContext::Library, Action::Confirm) => t!("Play selected", "선택 항목 재생"),
            (KeyContext::Library, Action::Back) => t!("Close Library", "라이브러리 닫기"),
            (KeyContext::Library, Action::LibraryRemove) => t!("Remove / delete", "제거 / 삭제"),
            (KeyContext::Library, Action::ToggleLocalMode) => {
                t!("Enter / exit Local Deck", "로컬 덱 들어가기 / 나가기")
            }
            (KeyContext::LocalDeck, Action::OpenLocalFind) => {
                t!("Find across Local Deck", "로컬 덱 전체에서 찾기")
            }
            (KeyContext::Playlists, Action::Confirm) => {
                t!("Open / play selected", "열기 / 선택 재생")
            }
            (KeyContext::Playlists, Action::PlayAll) => {
                t!("Play playlist", "플레이리스트 재생")
            }
            (KeyContext::Playlists, Action::Enqueue) => {
                t!("Enqueue playlist / song", "플레이리스트 / 곡 큐에 추가")
            }
            (KeyContext::Playlists, Action::LibraryRemove) => {
                t!(
                    "Delete playlist / remove song",
                    "플레이리스트 삭제 / 곡 제거"
                )
            }
            (KeyContext::Library, Action::DownloadAll) => {
                t!("Download whole list", "목록 전체 다운로드")
            }
            (KeyContext::Playlists, Action::DownloadAll) => {
                t!("Download playlist", "플레이리스트 다운로드")
            }
            (KeyContext::Playlists, Action::Back) => t!("Back / close", "뒤로 / 닫기"),
            (KeyContext::Queue, Action::Confirm) => t!("Play / jump to track", "곡 재생 / 이동"),
            (KeyContext::Queue, Action::Back) => t!("Close queue", "대기열 닫기"),
            (KeyContext::Queue, Action::QueueRemove) => {
                t!("Remove selected from queue", "선택 곡 큐에서 제거")
            }
            (KeyContext::SearchInput, Action::Confirm) => t!("Search", "검색"),
            (KeyContext::SearchInput, Action::ToggleSearchSourceMenu)
            | (KeyContext::SearchResults, Action::ToggleSearchSourceMenu) => {
                t!("Open source menu", "소스 메뉴 열기")
            }
            (KeyContext::AiInput, Action::Confirm) => t!("Send", "보내기"),
            (KeyContext::SearchResults, Action::Confirm) => t!("Play selected", "선택 항목 재생"),
            (KeyContext::SearchInput, Action::FocusPrev) => {
                t!("Focus search results", "검색 결과로 이동")
            }
            (KeyContext::SearchResults, Action::FocusPrev) => {
                t!("Focus search box", "검색창으로 이동")
            }
            (KeyContext::SearchResults, Action::Back) => {
                t!("Close Search Results", "검색 결과 닫기")
            }
            (KeyContext::Settings, Action::SettingsCancel) => t!("Save + quit", "저장하고 닫기"),
            _ => self.human_label(),
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        if id == "toggle_radio" {
            return Some(Action::ToggleStreaming);
        }
        ACTION_META
            .iter()
            .find(|(_, i, ..)| *i == id)
            .map(|(a, ..)| *a)
    }
}

/// Which input surface a binding applies to. Mirrors the handler / focus structure in
/// [`crate::app`]. `Common` is a fallback consulted for every screen (shared navigation);
/// `Global` holds bindings active regardless of mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyContext {
    Player,
    Common,
    Global,
    Library,
    LocalDeck,
    Playlists,
    Queue,
    SearchInput,
    SearchResults,
    Settings,
    AiInput,
    AiSuggestions,
    /// The "what's playing" (지듣노) identify card over the player.
    NowPlaying,
    /// Keybindings installed into the external mpv music-video overlay window.
    MpvOverlay,
}

const CONTEXT_META: &[(KeyContext, &str, &str, &str)] = &[
    (KeyContext::Player, "player", "Player", "플레이어"),
    (
        KeyContext::NowPlaying,
        "now_playing",
        "What's playing card",
        "지금 듣는 노래 카드",
    ),
    (
        KeyContext::MpvOverlay,
        "mpv_overlay",
        "mpv video overlay",
        "mpv 영상 창",
    ),
    (
        KeyContext::Common,
        "common",
        "Common navigation & text editing",
        "공통 탐색 및 텍스트 편집",
    ),
    (KeyContext::Global, "global", "Global", "전역"),
    (KeyContext::Library, "library", "Library", "라이브러리"),
    (KeyContext::LocalDeck, "local_deck", "Local Deck", "로컬 덱"),
    (
        KeyContext::Playlists,
        "playlists",
        "Playlists",
        "플레이리스트",
    ),
    (KeyContext::Queue, "queue", "Queue window", "대기열 창"),
    (
        KeyContext::SearchInput,
        "search_input",
        "Search box",
        "검색창",
    ),
    (
        KeyContext::SearchResults,
        "search_results",
        "Search results",
        "검색 결과",
    ),
    (KeyContext::Settings, "settings", "Settings", "설정"),
    (
        KeyContext::AiInput,
        "ai_input",
        "DJ Gem box",
        "DJ Gem 입력창",
    ),
    (
        KeyContext::AiSuggestions,
        "ai_suggestions",
        "DJ Gem results",
        "DJ Gem 결과",
    ),
];

impl KeyContext {
    pub fn id(self) -> &'static str {
        CONTEXT_META
            .iter()
            .find(|(c, ..)| *c == self)
            .map(|(_, id, ..)| *id)
            .unwrap_or("?")
    }

    /// The group title for the help cheat-sheet / Keys tab, in the active UI language.
    pub fn title(self) -> &'static str {
        CONTEXT_META
            .iter()
            .find(|(c, ..)| *c == self)
            .map(|(_, _, en, ko)| if crate::i18n::is_korean() { *ko } else { *en })
            .unwrap_or("?")
    }

    pub fn from_id(id: &str) -> Option<KeyContext> {
        CONTEXT_META
            .iter()
            .find(|(_, i, ..)| *i == id)
            .map(|(c, ..)| *c)
    }
}

/// A normalized key combination: a [`KeyCode`] plus the ctrl/alt/shift modifiers.
///
/// Equality is normalized so terminal quirks don't cause misses: 2-beolsik Korean IME
/// jamo are mapped back to their physical QWERTY keys, plain shifted `Char` keys are
/// represented by the produced character (an uppercase `'L'` already encodes shift), while
/// Ctrl/Alt character chords keep `SHIFT` so `Ctrl+X` and `Ctrl+Shift+X` remain distinct.
/// Ctrl/Alt letters ignore case, and `Shift+Tab` collapses to `BackTab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Chord {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        let mut mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        // Normalize Shift+Tab → BackTab (terminals report either).
        let mut code = if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
            KeyCode::BackTab
        } else {
            code
        };
        let modified_char = mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let KeyCode::Char(c) = code
            && let Some(latin) = korean_2set_key(c)
        {
            if modified_char {
                mods.set(
                    KeyModifiers::SHIFT,
                    mods.contains(KeyModifiers::SHIFT) || latin.is_ascii_uppercase(),
                );
                code = KeyCode::Char(latin.to_ascii_lowercase());
            } else {
                code = KeyCode::Char(
                    if mods.contains(KeyModifiers::SHIFT) && latin.is_ascii_lowercase() {
                        latin.to_ascii_uppercase()
                    } else {
                        latin
                    },
                );
            }
        }
        if let KeyCode::Char(c) = code
            && !modified_char
            && mods.contains(KeyModifiers::SHIFT)
            && c.is_ascii_lowercase()
        {
            code = KeyCode::Char(c.to_ascii_uppercase());
        }
        // Plain char case already encodes shift; BackTab is inherently shifted. Preserve
        // Shift on Ctrl/Alt chars so enhanced terminals can bind Ctrl+Shift+letter separately.
        if matches!(code, KeyCode::BackTab) || (matches!(code, KeyCode::Char(_)) && !modified_char)
        {
            mods.remove(KeyModifiers::SHIFT);
        }
        // Terminals can report Ctrl+Q as either Char('q') or Char('Q'); persisted chord
        // labels use lowercase modifiers, so normalize modified ASCII letters.
        if let KeyCode::Char(c) = code
            && mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && c.is_ascii_alphabetic()
        {
            code = KeyCode::Char(c.to_ascii_lowercase());
        }
        Chord { code, mods }
    }

    /// Whether this chord would normally produce a typed character (so it must not be
    /// swallowed as a command while a text field is focused).
    pub fn is_typeable(self) -> bool {
        matches!(self.code, KeyCode::Char(_))
            && !self.mods.contains(KeyModifiers::CONTROL)
            && !self.mods.contains(KeyModifiers::ALT)
    }
}

fn korean_2set_key(c: char) -> Option<char> {
    Some(match c {
        'ㅂ' => 'q',
        'ㅈ' => 'w',
        'ㄷ' => 'e',
        'ㄱ' => 'r',
        'ㅅ' => 't',
        'ㅛ' => 'y',
        'ㅕ' => 'u',
        'ㅑ' => 'i',
        'ㅐ' => 'o',
        'ㅔ' => 'p',
        'ㅁ' => 'a',
        'ㄴ' => 's',
        'ㅇ' => 'd',
        'ㄹ' => 'f',
        'ㅎ' => 'g',
        'ㅗ' => 'h',
        'ㅓ' => 'j',
        'ㅏ' => 'k',
        'ㅣ' => 'l',
        'ㅋ' => 'z',
        'ㅌ' => 'x',
        'ㅊ' => 'c',
        'ㅍ' => 'v',
        'ㅠ' => 'b',
        'ㅜ' => 'n',
        'ㅡ' => 'm',
        'ㅃ' => 'Q',
        'ㅉ' => 'W',
        'ㄸ' => 'E',
        'ㄲ' => 'R',
        'ㅆ' => 'T',
        'ㅒ' => 'O',
        'ㅖ' => 'P',
        _ => return None,
    })
}

impl From<KeyEvent> for Chord {
    fn from(k: KeyEvent) -> Self {
        Chord::new(k.code, k.modifiers)
    }
}

/// Why a rebind was rejected: `chord` is already bound to `existing` in context `ctx`
/// (the screen where it would have fired). Surfaced to the user as a warning popup so a
/// conflicting remap is reported loudly rather than silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    pub ctx: KeyContext,
    pub existing: Action,
    pub chord: Chord,
}

/// The resolved keybindings: chord → action (for dispatch) and action → chord (for
/// rendering hints), both keyed by context.
#[derive(Debug, Clone)]
pub struct KeyMap {
    bindings: HashMap<(KeyContext, Chord), Action>,
    labels: HashMap<(KeyContext, Action), Chord>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::from_overrides(&BTreeMap::new())
    }
}

impl KeyMap {
    /// Build from a context/action → chord table, deriving the reverse lookup.
    fn from_labels(labels: HashMap<(KeyContext, Action), Chord>) -> Self {
        let mut bindings = HashMap::with_capacity(labels.len());
        for (&(ctx, action), &chord) in &labels {
            bindings.insert((ctx, chord), action);
        }
        KeyMap { bindings, labels }
    }

    /// Build from persisted overrides layered over the built-in defaults.
    pub fn from_overrides(overrides: &BTreeMap<String, String>) -> Self {
        let mut labels: HashMap<(KeyContext, Action), Chord> = default_bindings()
            .into_iter()
            .map(|(c, a, ch)| ((c, a), ch))
            .collect();
        for (key, val) in overrides {
            let Some((ctx_id, action_id)) = key.split_once('.') else {
                tracing::warn!(key, "ignoring malformed keybinding override");
                continue;
            };
            let Some(mut ctx) = KeyContext::from_id(ctx_id) else {
                tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                continue;
            };
            let Some(action) = Action::from_id(action_id) else {
                if !(ctx_id == "settings" && action_id == "settings_save") {
                    tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                }
                continue;
            };
            if val.is_empty() {
                // Explicitly unbound (the GUI's per-row unbind): drop the default.
                labels.remove(&(ctx, action));
                continue;
            }
            let Some(chord) = parse_chord(val) else {
                tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                continue;
            };
            if ctx == KeyContext::Global && action == Action::ToggleRadioMode {
                ctx = KeyContext::Player;
            }
            labels.insert((ctx, action), chord);
        }
        compat::preserve_legacy_lyrics_delay_overrides(overrides, &mut labels);
        compat::preserve_legacy_shuffle_override(overrides, &mut labels);
        compat::preserve_legacy_delete_word_overrides(overrides, &mut labels);
        // Preserve the old Search-results shortcut as an unlisted compatibility binding:
        // the Player search key also focuses the query box from results. The new advertised
        // bidirectional binding is SearchInput/SearchResults FocusPrev (Shift+Tab).
        if !overrides.contains_key("search_results.focus_input")
            && let Some(&chord) = labels.get(&(KeyContext::Player, Action::OpenSearch))
        {
            let candidate = Self::from_labels(labels.clone());
            if let Some(conflict) =
                candidate.conflict(KeyContext::SearchResults, Action::FocusInput, chord)
            {
                tracing::warn!(
                    chord = %chord_to_config(chord),
                    conflict_ctx = ?conflict.ctx,
                    conflict_action = ?conflict.existing,
                    "not mirroring player.open_search to search_results.focus_input"
                );
            } else {
                labels.insert((KeyContext::SearchResults, Action::FocusInput), chord);
            }
        }
        Self::from_labels(labels)
    }

    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self::from_overrides(&cfg.keybindings)
    }

    /// Resolve the action bound to `chord` in `ctx`, falling back to the shared `Common`
    /// navigation bindings. Used by the per-screen key handlers.
    pub fn action(&self, ctx: KeyContext, chord: Chord) -> Option<Action> {
        self.bindings
            .get(&(ctx, chord))
            .or_else(|| self.bindings.get(&(KeyContext::Common, chord)))
            .copied()
    }

    /// Resolve only bindings declared directly on `ctx`, without the shared `Common`
    /// fallback. Text/list hybrids use this when a context-specific key intentionally
    /// shadows a common navigation key.
    pub fn context_action(&self, ctx: KeyContext, chord: Chord) -> Option<Action> {
        self.bindings.get(&(ctx, chord)).copied()
    }

    /// Resolve a `Global` action (help, streaming), independent of the active screen.
    pub fn global_action(&self, chord: Chord) -> Option<Action> {
        self.bindings.get(&(KeyContext::Global, chord)).copied()
    }

    /// Resolve the shared text-editing command before a focused editor considers typed input.
    /// Text fields capture their keyboard while active, so this deliberately bypasses a
    /// screen-specific binding that would otherwise shadow the Common editing action.
    pub fn text_edit_action(&self, chord: Chord) -> Option<Action> {
        self.bindings
            .get(&(KeyContext::Common, chord))
            .copied()
            .filter(|action| matches!(action, Action::DeleteWord))
    }

    /// The chord bound to `action` in `ctx`, formatted for the current display mode.
    pub fn label_for_display(&self, ctx: KeyContext, action: Action, retro: bool) -> String {
        let chord = self
            .labels
            .get(&(ctx, action))
            .or_else(|| self.labels.get(&(KeyContext::Common, action)))
            .or_else(|| self.labels.get(&(KeyContext::Global, action)))
            .copied();
        chord.map_or_else(
            || "?".to_owned(),
            |chord| format_chord_for_display(chord, retro),
        )
    }

    /// The chord currently bound to `(ctx, action)`, if any (for the editor).
    pub fn chord(&self, ctx: KeyContext, action: Action) -> Option<Chord> {
        self.labels.get(&(ctx, action)).copied()
    }

    /// If `chord` is already used by a *different* action that would win in the same
    /// routing scope, return the [`Conflict`] describing it. `Global` bindings are special:
    /// because they are consulted before every screen handler, they may not overlap any
    /// other context. Local contexts may shadow `Common` navigation, matching dispatch.
    fn conflict(&self, ctx: KeyContext, action: Action, chord: Chord) -> Option<Conflict> {
        if ctx == KeyContext::Global {
            return self.conflict_in_contexts(all_contexts(), action, chord);
        }

        self.conflict_in_context(ctx, action, chord)
            .or_else(|| self.conflict_in_context(KeyContext::Global, action, chord))
    }

    fn conflict_in_context(
        &self,
        ctx: KeyContext,
        action: Action,
        chord: Chord,
    ) -> Option<Conflict> {
        let existing = self.bindings.get(&(ctx, chord)).copied()?;
        let animation_shadow = chord == Chord::new(KeyCode::Char('A'), KeyModifiers::empty())
            && match ctx {
                KeyContext::Global => {
                    (existing, action) == (Action::ToggleAnimations, Action::AcceptAllImportReview)
                }
                KeyContext::LocalDeck => {
                    (existing, action) == (Action::AcceptAllImportReview, Action::ToggleAnimations)
                }
                _ => false,
            };
        if existing == action || animation_shadow {
            return None;
        }
        Some(Conflict {
            ctx,
            existing,
            chord,
        })
    }

    fn conflict_in_contexts(
        &self,
        contexts: impl IntoIterator<Item = KeyContext>,
        action: Action,
        chord: Chord,
    ) -> Option<Conflict> {
        contexts
            .into_iter()
            .find_map(|ctx| self.conflict_in_context(ctx, action, chord))
    }

    /// Rebind `(ctx, action)` to `chord`. Rejects (returns the [`Conflict`]) if the chord
    /// is already in use; otherwise drops the action's old chord and installs the new.
    pub fn rebind(
        &mut self,
        ctx: KeyContext,
        action: Action,
        chord: Chord,
    ) -> Result<(), Conflict> {
        for (target_ctx, target_action) in
            std::iter::once((ctx, action)).chain(linked_rebinds(ctx, action).iter().copied())
        {
            if let Some(conflict) = self.conflict(target_ctx, target_action, chord) {
                return Err(conflict);
            }
        }
        for (target_ctx, target_action) in
            std::iter::once((ctx, action)).chain(linked_rebinds(ctx, action).iter().copied())
        {
            if let Some(old) = self.labels.get(&(target_ctx, target_action)).copied() {
                self.bindings.remove(&(target_ctx, old));
            }
            self.bindings.insert((target_ctx, chord), target_action);
            self.labels.insert((target_ctx, target_action), chord);
        }
        Ok(())
    }

    /// Restore `(ctx, action)` to its built-in default chord. Returns the [`Conflict`] if
    /// the default chord is currently taken by something else.
    pub fn reset(&mut self, ctx: KeyContext, action: Action) -> Result<(), Conflict> {
        match default_chord(ctx, action) {
            Some(def) => self.rebind(ctx, action, def),
            None => Ok(()),
        }
    }

    /// Only the bindings that differ from the defaults, keyed `"<context>.<action>"`, for
    /// compact, forward-compatible persistence.
    pub fn to_overrides(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (ctx, action, def) in default_bindings() {
            match self.labels.get(&(ctx, action)).copied() {
                // Explicitly unbound persists as "" (see `from_overrides`).
                None => {
                    out.insert(format!("{}.{}", ctx.id(), action.id()), String::new());
                }
                Some(cur) if cur != def => {
                    out.insert(
                        format!("{}.{}", ctx.id(), action.id()),
                        chord_to_config(cur),
                    );
                }
                Some(_) => {}
            }
        }
        out
    }

    /// Remove `(ctx, action)`'s binding entirely (the GUI's per-row unbind). The action
    /// simply stops dispatching until rebound or reset.
    pub fn unbind(&mut self, ctx: KeyContext, action: Action) {
        if let Some(old) = self.labels.remove(&(ctx, action)) {
            self.bindings.remove(&(ctx, old));
        }
    }

    /// The full effective wire bindings for the settings model
    /// (`"<ctx>.<action>"` → config chord string; `""` = unbound).
    pub fn wire_bindings(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (ctx, action, _) in default_bindings() {
            let chord = self
                .labels
                .get(&(ctx, action))
                .map(|chord| chord_to_config(*chord))
                .unwrap_or_default();
            out.insert(format!("{}.{}", ctx.id(), action.id()), chord);
        }
        out
    }
}

fn all_contexts() -> impl Iterator<Item = KeyContext> {
    CONTEXT_META.iter().map(|(ctx, ..)| *ctx)
}

fn linked_rebinds(ctx: KeyContext, action: Action) -> &'static [(KeyContext, Action)] {
    match (ctx, action) {
        (KeyContext::Player, Action::OpenSearch) => {
            &[(KeyContext::SearchResults, Action::FocusInput)]
        }
        (KeyContext::SearchInput, Action::FocusPrev) => {
            &[(KeyContext::SearchResults, Action::FocusPrev)]
        }
        (KeyContext::SearchResults, Action::FocusPrev) => {
            &[(KeyContext::SearchInput, Action::FocusPrev)]
        }
        (KeyContext::SearchInput, Action::ToggleSearchSourceMenu) => {
            &[(KeyContext::SearchResults, Action::ToggleSearchSourceMenu)]
        }
        (KeyContext::SearchResults, Action::ToggleSearchSourceMenu) => {
            &[(KeyContext::SearchInput, Action::ToggleSearchSourceMenu)]
        }
        (KeyContext::SearchInput, Action::ToggleSearchKind) => {
            &[(KeyContext::SearchResults, Action::ToggleSearchKind)]
        }
        (KeyContext::SearchResults, Action::ToggleSearchKind) => {
            &[(KeyContext::SearchInput, Action::ToggleSearchKind)]
        }
        _ => &[],
    }
}

/// The built-in default bindings, ordered by context (which also drives the cheat-sheet /
/// editor grouping). Mirrors the keys the app shipped with before remapping existed.
pub fn default_bindings() -> Vec<(KeyContext, Action, Chord)> {
    use Action as A;
    use KeyContext as C;
    let key = |code| Chord::new(code, KeyModifiers::empty());
    let ch = |c| Chord::new(KeyCode::Char(c), KeyModifiers::empty());
    let ctrl = |c| Chord::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let alt_shift = |c| Chord::new(KeyCode::Char(c), KeyModifiers::ALT | KeyModifiers::SHIFT);
    // Shift + a non-`Char` key (arrows / Page / Home / End). `Chord::new` preserves Shift
    // for these, so `Shift+Up` stays distinct from `Up` and can bind range-select.
    let shift = |code| Chord::new(code, KeyModifiers::SHIFT);
    vec![
        // Player (the main screen; self-contained transport + screen switches).
        (C::Player, A::TogglePause, ch(' ')),
        (C::Player, A::ToggleRadioMode, alt_shift('r')),
        (C::Player, A::ToggleRecordings, alt_shift('e')),
        (C::Player, A::SeekBack, key(KeyCode::Left)),
        (C::Player, A::SeekForward, key(KeyCode::Right)),
        (C::Player, A::VolUp, key(KeyCode::Up)),
        (C::Player, A::VolDown, key(KeyCode::Down)),
        (C::Player, A::ToggleMute, ch('m')),
        // mpv-style transport: `,`/`.` skip tracks (mpv's `<`/`>`), since a music player has
        // no use for mpv's frame-step on `,`/`.`.
        (C::Player, A::PrevTrack, ch(',')),
        (C::Player, A::NextTrack, ch('.')),
        (C::Player, A::CycleRating, ch('f')),
        (C::Player, A::OpenLibrary, ch('l')),
        (C::Player, A::OpenQueue, ch('c')),
        (C::Player, A::QueueRemove, key(KeyCode::Delete)),
        (C::Player, A::ToggleLyrics, ch('L')),
        (C::Player, A::LyricsDelayEarlier, ch('z')),
        (C::Player, A::LyricsDelayLater, ch('Z')),
        (C::Player, A::Download, ch('d')),
        (C::Player, A::ToggleShuffle, ch('x')),
        (C::Player, A::CycleRepeat, ch('r')),
        (C::Player, A::IdentifyNowPlaying, ch('i')),
        (C::Player, A::CycleEq, ch('e')),
        (C::Player, A::ToggleNormalize, ch('N')),
        // Playback speed on `[`/`]` to match mpv (frees `<`/`>`).
        (C::Player, A::SpeedUp, ch(']')),
        (C::Player, A::SpeedDown, ch('[')),
        (C::Player, A::OpenSettings, ch('o')),
        (C::Player, A::OpenAi, ch('g')),
        (C::Player, A::OpenSearch, ch('s')),
        (C::Player, A::AddToPlaylist, ch('P')),
        (C::Player, A::CopyLink, ch('y')),
        (C::Player, A::PlayVideo, ch('v')),
        (C::Player, A::ToggleVideoLayout, ch('V')),
        (C::Player, A::Back, ch('q')),
        // The "what's playing" card's own actions (modal; `i`/Esc/Enter close it). `f`/`g`
        // deliberately mirror the player's favorite / DJ Gem keys.
        (C::NowPlaying, A::NowPlayingFavorite, ch('f')),
        (C::NowPlaying, A::NowPlayingAskAi, ch('g')),
        // External mpv video window controls. These are installed into mpv on the next
        // overlay open; compatibility aliases (`<`, `>`, `p`) stay fixed in video.rs.
        (C::MpvOverlay, A::VideoTogglePause, ch(' ')),
        (C::MpvOverlay, A::VideoNext, ch('.')),
        (C::MpvOverlay, A::VideoPrev, ch(',')),
        (C::MpvOverlay, A::VideoClose, ch('q')),
        (C::MpvOverlay, A::VideoToggleFullscreen, ch('f')),
        (C::MpvOverlay, A::VideoToggleMute, ch('m')),
        // Shared navigation (fallback for every list/text screen).
        (C::Common, A::MoveUp, key(KeyCode::Up)),
        (C::Common, A::MoveDown, key(KeyCode::Down)),
        (C::Common, A::PageUp, key(KeyCode::PageUp)),
        (C::Common, A::PageDown, key(KeyCode::PageDown)),
        (C::Common, A::JumpTop, key(KeyCode::Home)),
        (C::Common, A::JumpBottom, key(KeyCode::End)),
        // Shift+nav range-select (extends the anchor..=cursor selection in Library/Queue).
        (C::Common, A::SelectUp, shift(KeyCode::Up)),
        (C::Common, A::SelectDown, shift(KeyCode::Down)),
        (C::Common, A::SelectPageUp, shift(KeyCode::PageUp)),
        (C::Common, A::SelectPageDown, shift(KeyCode::PageDown)),
        (C::Common, A::SelectToTop, shift(KeyCode::Home)),
        (C::Common, A::SelectToBottom, shift(KeyCode::End)),
        (C::Common, A::Confirm, key(KeyCode::Enter)),
        (C::Common, A::FocusPrev, key(KeyCode::BackTab)),
        (C::Common, A::FocusNext, key(KeyCode::Tab)),
        (C::Common, A::DeleteChar, key(KeyCode::Backspace)),
        (
            C::Common,
            A::DeleteWord,
            Chord::new(KeyCode::Backspace, KeyModifiers::CONTROL),
        ),
        (C::Common, A::Back, ch('q')),
        // Global (active across screens; typeable globals are suppressed in text fields).
        (C::Global, A::Home, ctrl('h')),
        (C::Global, A::ToggleStreaming, ctrl('r')),
        (C::Global, A::ToggleHelp, ch('?')),
        (C::Global, A::OpenContextMenu, shift(KeyCode::F(10))),
        (C::Global, A::ToggleAbout, key(KeyCode::F(1))),
        (C::Global, A::ToggleAnimations, ch('A')),
        (C::Global, A::ToggleControlBox, ch('B')),
        (C::Global, A::WhyAi, ch('w')),
        // Browser-style text zoom (`=` is the unshifted `+` key). Works only on terminals
        // with the text sizing protocol; elsewhere the reducer answers with a hint toast.
        (C::Global, A::TextZoomIn, ctrl('=')),
        (C::Global, A::TextZoomOut, ctrl('-')),
        // Freezes the Ctrl+wheel zoom gesture (an easy thing to fire by accident while
        // scrolling with a modifier held); the Ctrl+-/= keys stay live either way.
        (C::Global, A::ToggleZoomWheelLock, ctrl('l')),
        (C::Global, A::Quit, ctrl('q')),
        // Library list commands.
        (C::Library, A::Confirm, key(KeyCode::Enter)),
        (C::Library, A::ToggleLocalMode, alt_shift('l')),
        (C::Library, A::Enqueue, ch('\\')),
        (C::Library, A::PlayAll, ch('a')),
        (C::Library, A::Favorite, ch('f')),
        (C::Library, A::Download, ch('d')),
        (C::Library, A::DownloadAll, ch('D')),
        (C::Library, A::OpenAi, ch('g')),
        (C::Library, A::AddToPlaylist, ch('p')),
        (C::Library, A::LibraryRemove, key(KeyCode::Delete)),
        (C::Library, A::LibraryFilter, ch('/')),
        (C::Library, A::Back, ch('q')),
        (C::LocalDeck, A::AcceptAllImportReview, ch('A')),
        (C::LocalDeck, A::OpenLocalFind, ctrl('f')),
        // Playlists tab (root list of playlists + opened-playlist drill-down).
        (C::Playlists, A::Confirm, key(KeyCode::Enter)),
        (C::Playlists, A::PlayAll, ch('a')),
        (C::Playlists, A::Enqueue, ch('\\')),
        (C::Playlists, A::PlaylistCreate, ch('n')),
        (C::Playlists, A::Favorite, ch('f')),
        (C::Playlists, A::Download, ch('d')),
        (C::Playlists, A::DownloadAll, ch('D')),
        (C::Playlists, A::OpenAi, ch('g')),
        (C::Playlists, A::AddToPlaylist, ch('p')),
        (C::Playlists, A::LibraryRemove, key(KeyCode::Delete)),
        (C::Playlists, A::LibraryFilter, ch('/')),
        (C::Playlists, A::Back, ch('q')),
        // Queue window (overlay on the player; up/down nav comes from Common).
        (C::Queue, A::Confirm, key(KeyCode::Enter)),
        (C::Queue, A::QueueRemove, key(KeyCode::Delete)),
        (C::Queue, A::Back, ch('q')),
        // Search box (text entry; Enter→search is handled in the input handler).
        (C::SearchInput, A::SelectAll, ctrl('a')),
        (C::SearchInput, A::ToggleSearchSourceMenu, key(KeyCode::Tab)),
        (C::SearchInput, A::ToggleSearchKind, ctrl('p')),
        (C::SearchInput, A::FocusPrev, key(KeyCode::BackTab)),
        // Search results list commands (Enter→play is fixed to the physical key in the
        // handler, so it's not listed here; the cheat-sheet shows it as a fixed row).
        (C::SearchResults, A::FocusPrev, key(KeyCode::BackTab)),
        (
            C::SearchResults,
            A::ToggleSearchSourceMenu,
            key(KeyCode::Tab),
        ),
        (C::SearchResults, A::ToggleSearchKind, ctrl('p')),
        (C::SearchResults, A::Enqueue, ch('\\')),
        (C::SearchResults, A::Favorite, ch('f')),
        (C::SearchResults, A::Download, ch('d')),
        (C::SearchResults, A::AddToPlaylist, ch('p')),
        (C::SearchResults, A::SearchFilter, ch('/')),
        (C::SearchResults, A::Back, ch('q')),
        // DJ Gem box (text entry; Enter→send is handled in the input handler).
        (C::AiInput, A::SelectAll, ctrl('a')),
        // Settings screen commands (nav comes from Common).
        (C::Settings, A::ChangeDecrease, key(KeyCode::Left)),
        (C::Settings, A::ChangeIncrease, key(KeyCode::Right)),
        (C::Settings, A::SettingsCancel, ch('q')),
    ]
}

/// The default chord for `(ctx, action)`, if it has one.
fn default_chord(ctx: KeyContext, action: Action) -> Option<Chord> {
    default_bindings()
        .into_iter()
        .find(|(c, a, _)| *c == ctx && *a == action)
        .map(|(.., ch)| ch)
}

/// The editable bindings grouped by context, in display order, for the editor and the
/// `?` cheat-sheet (headers + rows).
pub fn groups() -> Vec<(KeyContext, Vec<Action>)> {
    let mut out: Vec<(KeyContext, Vec<Action>)> = Vec::new();
    for (ctx, action, _) in default_bindings() {
        match out.last_mut() {
            Some((c, v)) if *c == ctx => v.push(action),
            _ => out.push((ctx, vec![action])),
        }
    }
    out
}

/// A flat, header-free list of every editable `(context, action)`, in display order. The
/// Keys-tab cursor indexes directly into this.
/// One editable action row for the wire keymap catalog (docs/gui/05 Hotkeys tab).
pub struct WireAction {
    pub context: &'static str,
    pub id: &'static str,
    pub label: String,
    pub default_chord: String,
}

/// Every editable action with its context, human label, and factory chord — the
/// settings model's `keymap.actions` catalog.
pub fn wire_actions() -> Vec<WireAction> {
    default_bindings()
        .into_iter()
        .map(|(ctx, action, def)| WireAction {
            context: ctx.id(),
            id: action.id(),
            label: action.human_label_for(ctx).to_string(),
            default_chord: chord_to_config(def),
        })
        .collect()
}

pub fn editable_entries() -> Vec<(KeyContext, Action)> {
    default_bindings()
        .into_iter()
        .map(|(c, a, _)| (c, a))
        .collect()
}

/// Parse a config chord string like `"space"`, `"ctrl+n"`, `"L"`, `">"` into a [`Chord`].
pub fn parse_chord(s: &str) -> Option<Chord> {
    let mut rest = s.trim();
    let mut mods = KeyModifiers::empty();
    loop {
        if let Some(r) = strip_ci(rest, "ctrl+").or_else(|| strip_ci(rest, "control+")) {
            mods |= KeyModifiers::CONTROL;
            rest = r;
        } else if let Some(r) = strip_ci(rest, "alt+") {
            mods |= KeyModifiers::ALT;
            rest = r;
        } else if let Some(r) = strip_ci(rest, "shift+") {
            mods |= KeyModifiers::SHIFT;
            rest = r;
        } else {
            break;
        }
    }
    parse_code(rest).map(|code| Chord::new(code, mods))
}

fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn parse_code(t: &str) -> Option<KeyCode> {
    let lower = t.to_ascii_lowercase();
    let code = match lower.as_str() {
        "space" => KeyCode::Char(' '),
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "null" => KeyCode::Null,
        "capslock" | "caps_lock" => KeyCode::CapsLock,
        "scrolllock" | "scroll_lock" => KeyCode::ScrollLock,
        "numlock" | "num_lock" => KeyCode::NumLock,
        "printscreen" | "print_screen" | "prtsc" => KeyCode::PrintScreen,
        "pause" => KeyCode::Pause,
        "menu" => KeyCode::Menu,
        "keypadbegin" | "keypad_begin" | "begin" => KeyCode::KeypadBegin,
        "media_play" => KeyCode::Media(MediaKeyCode::Play),
        "media_pause" => KeyCode::Media(MediaKeyCode::Pause),
        "media_play_pause" | "media_playpause" => KeyCode::Media(MediaKeyCode::PlayPause),
        "media_reverse" => KeyCode::Media(MediaKeyCode::Reverse),
        "media_stop" => KeyCode::Media(MediaKeyCode::Stop),
        "media_fast_forward" | "media_fastforward" => KeyCode::Media(MediaKeyCode::FastForward),
        "media_rewind" => KeyCode::Media(MediaKeyCode::Rewind),
        "media_track_next" | "media_next" => KeyCode::Media(MediaKeyCode::TrackNext),
        "media_track_previous" | "media_previous" | "media_prev" => {
            KeyCode::Media(MediaKeyCode::TrackPrevious)
        }
        "media_record" => KeyCode::Media(MediaKeyCode::Record),
        "media_lower_volume" | "media_volume_down" => KeyCode::Media(MediaKeyCode::LowerVolume),
        "media_raise_volume" | "media_volume_up" => KeyCode::Media(MediaKeyCode::RaiseVolume),
        "media_mute_volume" | "media_mute" => KeyCode::Media(MediaKeyCode::MuteVolume),
        "left_shift" => KeyCode::Modifier(ModifierKeyCode::LeftShift),
        "left_ctrl" | "left_control" => KeyCode::Modifier(ModifierKeyCode::LeftControl),
        "left_alt" => KeyCode::Modifier(ModifierKeyCode::LeftAlt),
        "left_super" => KeyCode::Modifier(ModifierKeyCode::LeftSuper),
        "left_hyper" => KeyCode::Modifier(ModifierKeyCode::LeftHyper),
        "left_meta" => KeyCode::Modifier(ModifierKeyCode::LeftMeta),
        "right_shift" => KeyCode::Modifier(ModifierKeyCode::RightShift),
        "right_ctrl" | "right_control" => KeyCode::Modifier(ModifierKeyCode::RightControl),
        "right_alt" => KeyCode::Modifier(ModifierKeyCode::RightAlt),
        "right_super" => KeyCode::Modifier(ModifierKeyCode::RightSuper),
        "right_hyper" => KeyCode::Modifier(ModifierKeyCode::RightHyper),
        "right_meta" => KeyCode::Modifier(ModifierKeyCode::RightMeta),
        "iso_level3_shift" | "iso_level_3_shift" => {
            KeyCode::Modifier(ModifierKeyCode::IsoLevel3Shift)
        }
        "iso_level5_shift" | "iso_level_5_shift" => {
            KeyCode::Modifier(ModifierKeyCode::IsoLevel5Shift)
        }
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|d| d.parse::<u8>().ok())
                && (1..=12).contains(&n)
            {
                KeyCode::F(n)
            } else {
                // A single literal character, taking the *original* case (so `L` ≠ `l`).
                let mut chars = t.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                KeyCode::Char(c)
            }
        }
    };
    Some(code)
}

/// The canonical persisted form of a chord (inverse of [`parse_chord`]).
pub fn chord_to_config(chord: Chord) -> String {
    let mut s = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl+");
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        s.push_str("alt+");
    }
    if chord.mods.contains(KeyModifiers::SHIFT) {
        s.push_str("shift+");
    }
    match chord.code {
        KeyCode::Char(' ') => s.push_str("space"),
        KeyCode::Char(c) => s.push(c),
        KeyCode::F(n) => s.push_str(&format!("f{n}")),
        other => s.push_str(code_token(other)),
    }
    s
}

/// Convert a TUI chord into mpv `input.conf` key-name syntax for the video overlay.
/// Unsupported terminal-only keys return `None` so Settings can reject them up front.
pub fn chord_to_mpv_input(chord: Chord) -> Option<String> {
    let (base, inherent_shift) = match chord.code {
        KeyCode::Char(' ') => ("SPACE".to_owned(), false),
        KeyCode::Char(c) if c.is_ascii() && !c.is_ascii_control() => (c.to_string(), false),
        KeyCode::Esc => ("ESC".to_owned(), false),
        KeyCode::Left => ("LEFT".to_owned(), false),
        KeyCode::Right => ("RIGHT".to_owned(), false),
        KeyCode::Up => ("UP".to_owned(), false),
        KeyCode::Down => ("DOWN".to_owned(), false),
        KeyCode::Enter => ("ENTER".to_owned(), false),
        KeyCode::Tab => ("TAB".to_owned(), false),
        KeyCode::BackTab => ("TAB".to_owned(), true),
        KeyCode::Backspace => ("BS".to_owned(), false),
        KeyCode::Delete => ("DEL".to_owned(), false),
        KeyCode::Home => ("HOME".to_owned(), false),
        KeyCode::End => ("END".to_owned(), false),
        KeyCode::PageUp => ("PGUP".to_owned(), false),
        KeyCode::PageDown => ("PGDWN".to_owned(), false),
        KeyCode::F(n) if (1..=12).contains(&n) => (format!("F{n}"), false),
        _ => return None,
    };
    let mut out = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if inherent_shift || chord.mods.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    out.push_str(&base);
    Some(out)
}

/// Fixed mpv-compatibility aliases that remain active in the overlay even though the
/// primary displayed bindings are the remappable YuTuTui defaults.
pub fn mpv_overlay_fixed_alias(chord: Chord) -> Option<Action> {
    let ch = |c| Chord::new(KeyCode::Char(c), KeyModifiers::empty());
    if chord == ch('p') {
        Some(Action::VideoTogglePause)
    } else if chord == ch('>') {
        Some(Action::VideoNext)
    } else if chord == ch('<') {
        Some(Action::VideoPrev)
    } else {
        None
    }
}

fn code_token(code: KeyCode) -> &'static str {
    match code {
        KeyCode::Enter => "enter",
        KeyCode::Esc => "esc",
        KeyCode::Tab => "tab",
        KeyCode::BackTab => "backtab",
        KeyCode::Backspace => "backspace",
        KeyCode::Delete => "delete",
        KeyCode::Insert => "insert",
        KeyCode::Up => "up",
        KeyCode::Down => "down",
        KeyCode::Left => "left",
        KeyCode::Right => "right",
        KeyCode::Home => "home",
        KeyCode::End => "end",
        KeyCode::PageUp => "pageup",
        KeyCode::PageDown => "pagedown",
        KeyCode::Null => "null",
        KeyCode::CapsLock => "capslock",
        KeyCode::ScrollLock => "scrolllock",
        KeyCode::NumLock => "numlock",
        KeyCode::PrintScreen => "printscreen",
        KeyCode::Pause => "pause",
        KeyCode::Menu => "menu",
        KeyCode::KeypadBegin => "keypadbegin",
        KeyCode::Media(media) => media_token(media),
        KeyCode::Modifier(modifier) => modifier_token(modifier),
        KeyCode::F(_) | KeyCode::Char(_) => "?",
    }
}

fn media_token(media: MediaKeyCode) -> &'static str {
    match media {
        MediaKeyCode::Play => "media_play",
        MediaKeyCode::Pause => "media_pause",
        MediaKeyCode::PlayPause => "media_play_pause",
        MediaKeyCode::Reverse => "media_reverse",
        MediaKeyCode::Stop => "media_stop",
        MediaKeyCode::FastForward => "media_fast_forward",
        MediaKeyCode::Rewind => "media_rewind",
        MediaKeyCode::TrackNext => "media_track_next",
        MediaKeyCode::TrackPrevious => "media_track_previous",
        MediaKeyCode::Record => "media_record",
        MediaKeyCode::LowerVolume => "media_lower_volume",
        MediaKeyCode::RaiseVolume => "media_raise_volume",
        MediaKeyCode::MuteVolume => "media_mute_volume",
    }
}

fn modifier_token(modifier: ModifierKeyCode) -> &'static str {
    match modifier {
        ModifierKeyCode::LeftShift => "left_shift",
        ModifierKeyCode::LeftControl => "left_ctrl",
        ModifierKeyCode::LeftAlt => "left_alt",
        ModifierKeyCode::LeftSuper => "left_super",
        ModifierKeyCode::LeftHyper => "left_hyper",
        ModifierKeyCode::LeftMeta => "left_meta",
        ModifierKeyCode::RightShift => "right_shift",
        ModifierKeyCode::RightControl => "right_ctrl",
        ModifierKeyCode::RightAlt => "right_alt",
        ModifierKeyCode::RightSuper => "right_super",
        ModifierKeyCode::RightHyper => "right_hyper",
        ModifierKeyCode::RightMeta => "right_meta",
        ModifierKeyCode::IsoLevel3Shift => "iso_level3_shift",
        ModifierKeyCode::IsoLevel5Shift => "iso_level5_shift",
    }
}

#[cfg(test)]
mod tests;
