use crate::t;

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
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorWordLeft,
    MoveCursorWordRight,
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
    (
        Action::MoveCursorLeft,
        "move_cursor_left",
        "Move cursor left",
        "커서 왼쪽 이동",
    ),
    (
        Action::MoveCursorRight,
        "move_cursor_right",
        "Move cursor right",
        "커서 오른쪽 이동",
    ),
    (
        Action::MoveCursorWordLeft,
        "move_cursor_word_left",
        "Move cursor to previous word",
        "커서 이전 단어로 이동",
    ),
    (
        Action::MoveCursorWordRight,
        "move_cursor_word_right",
        "Move cursor to next word",
        "커서 다음 단어로 이동",
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

pub(super) fn all_contexts() -> impl Iterator<Item = KeyContext> {
    CONTEXT_META.iter().map(|(ctx, ..)| *ctx)
}
