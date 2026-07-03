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

/// A semantic command, decoupled from the physical key that triggers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    // Player transport / playback.
    TogglePause,
    SeekBack,
    SeekForward,
    VolUp,
    VolDown,
    NextTrack,
    PrevTrack,
    Favorite,
    CycleRating,
    OpenLibrary,
    OpenQueue,
    ToggleLyrics,
    Download,
    ToggleShuffle,
    CycleRepeat,
    CycleEq,
    ToggleNormalize,
    SpeedUp,
    SpeedDown,
    OpenSettings,
    OpenAi,
    OpenSearch,
    Quit,
    Home,
    // Shared navigation (interpreted per context).
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    JumpTop,
    JumpBottom,
    Confirm,
    Enqueue,
    Back,
    FocusNext,
    FocusPrev,
    DeleteChar,
    SelectAll,
    ToggleSearchSourceMenu,
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
    // Global (active in any non-text-entry context).
    ToggleStreaming,
    ToggleRadioMode,
    ToggleHelp,
    ToggleAbout,
    ToggleAnimations,
    WhyAi,
    TextZoomIn,
    TextZoomOut,
    // Player extras: copy link + external mpv video overlay.
    CopyLink,
    PlayVideo,
    ToggleVideoLayout,
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
        Action::Download,
        "download",
        "Download track",
        "곡 다운로드",
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
    (Action::SelectAll, "select_all", "Select all", "전체 선택"),
    (
        Action::ToggleSearchSourceMenu,
        "toggle_search_source_menu",
        "Search source menu",
        "검색 소스 메뉴",
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
        "Toggle streaming",
        "스트리밍 켜기 / 끄기",
    ),
    (
        Action::ToggleRadioMode,
        "toggle_radio_mode",
        "Radio/Normal mode",
        "라디오/일반 모드",
    ),
    (
        Action::ToggleHelp,
        "toggle_help",
        "Toggle help",
        "도움말 켜기 / 끄기",
    ),
    (
        Action::ToggleAbout,
        "toggle_about",
        "About ytm-tui",
        "ytm-tui 정보",
    ),
    (
        Action::ToggleAnimations,
        "toggle_animations",
        "Toggle animations",
        "애니메이션 켜기 / 끄기",
    ),
    (
        Action::WhyAi,
        "why_ai",
        "Why these DJ Gem picks",
        "DJ Gem 선곡 이유 보기",
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

    fn from_id(id: &str) -> Option<Action> {
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
    Playlists,
    Queue,
    SearchInput,
    SearchResults,
    Settings,
    AiInput,
    AiSuggestions,
}

const CONTEXT_META: &[(KeyContext, &str, &str, &str)] = &[
    (KeyContext::Player, "player", "Player", "플레이어"),
    (
        KeyContext::Common,
        "common",
        "Navigation (all screens)",
        "탐색 (모든 화면)",
    ),
    (KeyContext::Global, "global", "Global", "전역"),
    (KeyContext::Library, "library", "Library", "라이브러리"),
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

    fn from_id(id: &str) -> Option<KeyContext> {
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
            let Some(chord) = parse_chord(val) else {
                tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                continue;
            };
            if ctx == KeyContext::Global && action == Action::ToggleRadioMode {
                ctx = KeyContext::Player;
            }
            labels.insert((ctx, action), chord);
        }
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
        (existing != action).then_some(Conflict {
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
            let cur = self.labels.get(&(ctx, action)).copied().unwrap_or(def);
            if cur != def {
                out.insert(
                    format!("{}.{}", ctx.id(), action.id()),
                    chord_to_config(cur),
                );
            }
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
    vec![
        // Player (the main screen; self-contained transport + screen switches).
        (C::Player, A::TogglePause, ch(' ')),
        (C::Player, A::ToggleRadioMode, alt_shift('r')),
        (C::Player, A::SeekBack, key(KeyCode::Left)),
        (C::Player, A::SeekForward, key(KeyCode::Right)),
        (C::Player, A::VolUp, key(KeyCode::Up)),
        (C::Player, A::VolDown, key(KeyCode::Down)),
        (C::Player, A::PrevTrack, ch('p')),
        (C::Player, A::NextTrack, ch('n')),
        (C::Player, A::CycleRating, ch('f')),
        (C::Player, A::OpenLibrary, ch('l')),
        (C::Player, A::OpenQueue, ch('c')),
        (C::Player, A::QueueRemove, key(KeyCode::Delete)),
        (C::Player, A::ToggleLyrics, ch('L')),
        (C::Player, A::Download, ch('d')),
        (C::Player, A::ToggleShuffle, ch('S')),
        (C::Player, A::CycleRepeat, ch('r')),
        (C::Player, A::CycleEq, ch('e')),
        (C::Player, A::ToggleNormalize, ch('N')),
        (C::Player, A::SpeedUp, ch('>')),
        (C::Player, A::SpeedDown, ch('<')),
        (C::Player, A::OpenSettings, ch(',')),
        (C::Player, A::OpenAi, ch('g')),
        (C::Player, A::OpenSearch, ch('s')),
        (C::Player, A::AddToPlaylist, ch('P')),
        (C::Player, A::CopyLink, ch('y')),
        (C::Player, A::PlayVideo, ch('v')),
        (C::Player, A::ToggleVideoLayout, ch('V')),
        (C::Player, A::Back, ch('q')),
        // Shared navigation (fallback for every list/text screen).
        (C::Common, A::MoveUp, key(KeyCode::Up)),
        (C::Common, A::MoveDown, key(KeyCode::Down)),
        (C::Common, A::PageUp, key(KeyCode::PageUp)),
        (C::Common, A::PageDown, key(KeyCode::PageDown)),
        (C::Common, A::JumpTop, key(KeyCode::Home)),
        (C::Common, A::JumpBottom, key(KeyCode::End)),
        (C::Common, A::Confirm, key(KeyCode::Enter)),
        (C::Common, A::FocusPrev, key(KeyCode::BackTab)),
        (C::Common, A::FocusNext, key(KeyCode::Tab)),
        (C::Common, A::DeleteChar, key(KeyCode::Backspace)),
        (C::Common, A::Back, ch('q')),
        // Global (active across screens; typeable globals are suppressed in text fields).
        (C::Global, A::Home, ctrl('h')),
        (C::Global, A::ToggleStreaming, ctrl('r')),
        (C::Global, A::ToggleHelp, ch('?')),
        (C::Global, A::ToggleAbout, key(KeyCode::F(1))),
        (C::Global, A::ToggleAnimations, ch('A')),
        (C::Global, A::WhyAi, ch('w')),
        // Browser-style text zoom (`=` is the unshifted `+` key). Works only on terminals
        // with the text sizing protocol; elsewhere the reducer answers with a hint toast.
        (C::Global, A::TextZoomIn, ctrl('=')),
        (C::Global, A::TextZoomOut, ctrl('-')),
        (C::Global, A::Quit, ctrl('q')),
        // Library list commands.
        (C::Library, A::Confirm, key(KeyCode::Enter)),
        (C::Library, A::Enqueue, ch('\\')),
        (C::Library, A::PlayAll, ch('a')),
        (C::Library, A::Favorite, ch('f')),
        (C::Library, A::Download, ch('d')),
        (C::Library, A::OpenAi, ch('g')),
        (C::Library, A::AddToPlaylist, ch('p')),
        (C::Library, A::LibraryRemove, key(KeyCode::Delete)),
        (C::Library, A::LibraryFilter, ch('/')),
        (C::Library, A::Back, ch('q')),
        // Playlists tab (root list of playlists + opened-playlist drill-down).
        (C::Playlists, A::Confirm, key(KeyCode::Enter)),
        (C::Playlists, A::PlayAll, ch('a')),
        (C::Playlists, A::Enqueue, ch('\\')),
        (C::Playlists, A::PlaylistCreate, ch('n')),
        (C::Playlists, A::Favorite, ch('f')),
        (C::Playlists, A::Download, ch('d')),
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
        (C::SearchInput, A::FocusPrev, key(KeyCode::BackTab)),
        // Search results list commands (Enter→play is fixed to the physical key in the
        // handler, so it's not listed here; the cheat-sheet shows it as a fixed row).
        (C::SearchResults, A::FocusPrev, key(KeyCode::BackTab)),
        (
            C::SearchResults,
            A::ToggleSearchSourceMenu,
            key(KeyCode::Tab),
        ),
        (C::SearchResults, A::Enqueue, ch('\\')),
        (C::SearchResults, A::Favorite, ch('f')),
        (C::SearchResults, A::Download, ch('d')),
        (C::SearchResults, A::AddToPlaylist, ch('p')),
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

pub fn format_chord_for_display(chord: Chord, retro: bool) -> String {
    if retro {
        format_chord_retro(chord)
    } else {
        format_chord(chord)
    }
}

/// Render a chord as a compact human-readable label for footers / cheat-sheet:
/// `␣`, `←/→/↑/↓`, `Enter`, `Esc`, `Tab`, `^r`, `M-x`, etc.
pub fn format_chord(chord: Chord) -> String {
    let mut s = String::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        s.push('^');
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        s.push_str("M-");
    }
    if chord.mods.contains(KeyModifiers::SHIFT) {
        s.push('⇧');
    }
    match chord.code {
        KeyCode::Char(' ') => s.push('␣'),
        KeyCode::Char(c) => s.push(c),
        KeyCode::Left => s.push('←'),
        KeyCode::Right => s.push('→'),
        KeyCode::Up => s.push('↑'),
        KeyCode::Down => s.push('↓'),
        KeyCode::Enter => s.push_str("Enter"),
        KeyCode::Esc => s.push_str("Esc"),
        KeyCode::Tab => s.push_str("Tab"),
        KeyCode::BackTab => s.push_str("⇧Tab"),
        KeyCode::Backspace => s.push('⌫'),
        KeyCode::Delete => s.push_str("Del"),
        KeyCode::Insert => s.push_str("Ins"),
        KeyCode::Home => s.push_str("Home"),
        KeyCode::End => s.push_str("End"),
        KeyCode::PageUp => s.push_str("PgUp"),
        KeyCode::PageDown => s.push_str("PgDn"),
        KeyCode::F(n) => s.push_str(&format!("F{n}")),
        KeyCode::Null => s.push_str("Null"),
        KeyCode::CapsLock => s.push_str("Caps"),
        KeyCode::ScrollLock => s.push_str("Scroll"),
        KeyCode::NumLock => s.push_str("Num"),
        KeyCode::PrintScreen => s.push_str("PrtSc"),
        KeyCode::Pause => s.push_str("Pause"),
        KeyCode::Menu => s.push_str("Menu"),
        KeyCode::KeypadBegin => s.push_str("Begin"),
        KeyCode::Media(media) => s.push_str(media_label(media)),
        KeyCode::Modifier(modifier) => s.push_str(modifier_label(modifier)),
    }
    s
}

/// Retro-mode key labels avoid glyphs outside the 256-cell console set. This keeps the
/// key editor and help sheet readable after the final retro frame scrubber runs.
pub fn format_chord_retro(chord: Chord) -> String {
    let mut parts = Vec::new();
    if chord.mods.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_owned());
    }
    if chord.mods.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_owned());
    }
    if chord.mods.contains(KeyModifiers::SHIFT) {
        parts.push("Shift".to_owned());
    }
    if chord.code == KeyCode::BackTab {
        if !chord.mods.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".to_owned());
        }
        parts.push("Tab".to_owned());
    } else {
        parts.push(retro_key_label(
            chord.code,
            chord.mods.contains(KeyModifiers::SHIFT)
                || chord
                    .mods
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        ));
    }
    parts.join("+")
}

fn retro_key_label(code: KeyCode, shifted: bool) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char('+') => "Plus".to_owned(),
        KeyCode::Char(c) if shifted && c.is_ascii_alphabetic() => {
            c.to_ascii_uppercase().to_string()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "Shift+Tab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Null => "Null".to_owned(),
        KeyCode::CapsLock => "CapsLock".to_owned(),
        KeyCode::ScrollLock => "ScrollLock".to_owned(),
        KeyCode::NumLock => "NumLock".to_owned(),
        KeyCode::PrintScreen => "PrintScreen".to_owned(),
        KeyCode::Pause => "Pause".to_owned(),
        KeyCode::Menu => "Menu".to_owned(),
        KeyCode::KeypadBegin => "KeypadBegin".to_owned(),
        KeyCode::Media(media) => media_label(media).replace(' ', ""),
        KeyCode::Modifier(modifier) => modifier_label(modifier).replace(' ', ""),
    }
}

fn media_label(media: MediaKeyCode) -> &'static str {
    match media {
        MediaKeyCode::Play => "Play",
        MediaKeyCode::Pause => "Pause",
        MediaKeyCode::PlayPause => "Play/Pause",
        MediaKeyCode::Reverse => "Reverse",
        MediaKeyCode::Stop => "Stop",
        MediaKeyCode::FastForward => "Fast Forward",
        MediaKeyCode::Rewind => "Rewind",
        MediaKeyCode::TrackNext => "Next Track",
        MediaKeyCode::TrackPrevious => "Previous Track",
        MediaKeyCode::Record => "Record",
        MediaKeyCode::LowerVolume => "Lower Volume",
        MediaKeyCode::RaiseVolume => "Raise Volume",
        MediaKeyCode::MuteVolume => "Mute Volume",
    }
}

fn modifier_label(modifier: ModifierKeyCode) -> &'static str {
    match modifier {
        ModifierKeyCode::LeftShift => "Left Shift",
        ModifierKeyCode::LeftControl => "Left Ctrl",
        ModifierKeyCode::LeftAlt => "Left Alt",
        ModifierKeyCode::LeftSuper => "Left Super",
        ModifierKeyCode::LeftHyper => "Left Hyper",
        ModifierKeyCode::LeftMeta => "Left Meta",
        ModifierKeyCode::RightShift => "Right Shift",
        ModifierKeyCode::RightControl => "Right Ctrl",
        ModifierKeyCode::RightAlt => "Right Alt",
        ModifierKeyCode::RightSuper => "Right Super",
        ModifierKeyCode::RightHyper => "Right Hyper",
        ModifierKeyCode::RightMeta => "Right Meta",
        ModifierKeyCode::IsoLevel3Shift => "Iso Level 3 Shift",
        ModifierKeyCode::IsoLevel5Shift => "Iso Level 5 Shift",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> Chord {
        Chord::from(KeyEvent::new(code, mods))
    }

    #[test]
    fn space_formats_as_symbol() {
        assert_eq!(format_chord(parse_chord("space").unwrap()), "␣");
        assert_eq!(
            chord_to_config(Chord::new(KeyCode::Char(' '), KeyModifiers::empty())),
            "space"
        );
    }

    #[test]
    fn ctrl_and_arrow_formatting() {
        assert_eq!(format_chord(parse_chord("ctrl+r").unwrap()), "^r");
        assert_eq!(format_chord(parse_chord("ctrl+shift+x").unwrap()), "^⇧x");
        assert_eq!(format_chord(parse_chord("ctrl+q").unwrap()), "^q");
        assert_eq!(format_chord(parse_chord("ctrl+h").unwrap()), "^h");
        assert_eq!(format_chord(parse_chord("left").unwrap()), "←");
        assert_eq!(format_chord(parse_chord("right").unwrap()), "→");
        assert_eq!(format_chord(parse_chord("up").unwrap()), "↑");
        assert_eq!(format_chord(parse_chord("down").unwrap()), "↓");
        assert_eq!(chord_to_config(parse_chord("ctrl+r").unwrap()), "ctrl+r");
        assert_eq!(
            chord_to_config(parse_chord("ctrl+shift+x").unwrap()),
            "ctrl+shift+x"
        );
    }

    #[test]
    fn retro_key_labels_use_words_and_plus_separators() {
        assert_eq!(format_chord_retro(parse_chord("space").unwrap()), "Space");
        assert_eq!(format_chord_retro(parse_chord("ctrl+r").unwrap()), "Ctrl+R");
        assert_eq!(
            format_chord_retro(parse_chord("ctrl+shift+x").unwrap()),
            "Ctrl+Shift+X"
        );
        assert_eq!(
            format_chord_retro(parse_chord("alt+shift+r").unwrap()),
            "Alt+Shift+R"
        );
        assert_eq!(format_chord_retro(parse_chord("left").unwrap()), "Left");
        assert_eq!(format_chord_retro(parse_chord("right").unwrap()), "Right");
        assert_eq!(format_chord_retro(parse_chord("up").unwrap()), "Up");
        assert_eq!(format_chord_retro(parse_chord("down").unwrap()), "Down");
        assert_eq!(
            format_chord_retro(parse_chord("backtab").unwrap()),
            "Shift+Tab"
        );
        assert_eq!(
            format_chord_for_display(parse_chord("space").unwrap(), true),
            "Space"
        );
        assert_eq!(
            format_chord_for_display(parse_chord("space").unwrap(), false),
            "␣"
        );
    }

    #[test]
    fn parse_format_round_trip() {
        for s in [
            "space",
            "ctrl+n",
            "ctrl+q",
            "ctrl+h",
            "ctrl+shift+x",
            "alt+shift+1",
            "L",
            ">",
            "/",
            "?",
            "enter",
            "esc",
            "backtab",
            "f5",
            "capslock",
            "printscreen",
            "media_play_pause",
            "left_shift",
        ] {
            let chord = parse_chord(s).unwrap();
            assert_eq!(
                parse_chord(&chord_to_config(chord)).unwrap(),
                chord,
                "round trip {s}"
            );
        }
    }

    #[test]
    fn shift_is_normalized_for_chars() {
        // Shift+L (uppercase char, with or without the SHIFT flag) is one chord.
        let a = ev(KeyCode::Char('L'), KeyModifiers::SHIFT);
        let b = ev(KeyCode::Char('L'), KeyModifiers::empty());
        assert_eq!(a, b);
        assert_eq!(
            parse_chord("shift+l").unwrap(),
            Chord::new(KeyCode::Char('L'), KeyModifiers::empty())
        );
        assert_eq!(chord_to_config(parse_chord("shift+l").unwrap()), "L");
        // Shift+Tab collapses to BackTab.
        assert_eq!(
            ev(KeyCode::Tab, KeyModifiers::SHIFT),
            ev(KeyCode::BackTab, KeyModifiers::empty())
        );
    }

    #[test]
    fn ctrl_char_case_is_normalized() {
        assert_eq!(
            ev(KeyCode::Char('Q'), KeyModifiers::CONTROL),
            ev(KeyCode::Char('q'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            chord_to_config(ev(KeyCode::Char('Q'), KeyModifiers::CONTROL)),
            "ctrl+q"
        );
    }

    #[test]
    fn ctrl_shift_char_chords_stay_distinct() {
        let ctrl_x = parse_chord("ctrl+x").unwrap();
        let ctrl_shift_x = parse_chord("ctrl+shift+x").unwrap();
        assert_ne!(ctrl_x, ctrl_shift_x);
        assert_eq!(
            ev(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            ctrl_shift_x
        );
        assert_eq!(
            ev(
                KeyCode::Char('X'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            ctrl_shift_x
        );
        // Some terminals report Ctrl+X as an uppercase char without the Shift modifier.
        assert_eq!(ev(KeyCode::Char('X'), KeyModifiers::CONTROL), ctrl_x);

        let mut km = KeyMap::default();
        km.rebind(KeyContext::Player, Action::TogglePause, ctrl_shift_x)
            .unwrap();
        assert_eq!(
            km.action(KeyContext::Player, ctrl_shift_x),
            Some(Action::TogglePause)
        );
        assert_eq!(km.action(KeyContext::Player, ctrl_x), None);
    }

    #[test]
    fn korean_2set_keys_normalize_to_qwerty() {
        assert_eq!(
            ev(KeyCode::Char('ㅂ'), KeyModifiers::empty()),
            parse_chord("q").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㅂ'), KeyModifiers::CONTROL),
            parse_chord("ctrl+q").unwrap()
        );
        assert_eq!(
            ev(
                KeyCode::Char('ㅂ'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            parse_chord("ctrl+shift+q").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㅃ'), KeyModifiers::CONTROL),
            parse_chord("ctrl+shift+q").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㄱ'), KeyModifiers::CONTROL),
            parse_chord("ctrl+r").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㅂ'), KeyModifiers::ALT),
            parse_chord("alt+q").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㅣ'), KeyModifiers::SHIFT),
            parse_chord("L").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㅇ'), KeyModifiers::SHIFT),
            parse_chord("D").unwrap()
        );
        assert_eq!(
            ev(KeyCode::Char('ㅆ'), KeyModifiers::empty()),
            parse_chord("T").unwrap()
        );
    }

    #[test]
    fn shifted_korean_2set_keys_without_distinct_jamo_normalize_to_uppercase_qwerty() {
        for (jamo, latin) in [
            ('ㅛ', 'Y'),
            ('ㅕ', 'U'),
            ('ㅑ', 'I'),
            ('ㅁ', 'A'),
            ('ㄴ', 'S'),
            ('ㅇ', 'D'),
            ('ㄹ', 'F'),
            ('ㅎ', 'G'),
            ('ㅗ', 'H'),
            ('ㅓ', 'J'),
            ('ㅏ', 'K'),
            ('ㅣ', 'L'),
            ('ㅋ', 'Z'),
            ('ㅌ', 'X'),
            ('ㅊ', 'C'),
            ('ㅍ', 'V'),
            ('ㅠ', 'B'),
            ('ㅜ', 'N'),
            ('ㅡ', 'M'),
        ] {
            assert_eq!(
                ev(KeyCode::Char(jamo), KeyModifiers::SHIFT),
                Chord::new(KeyCode::Char(latin), KeyModifiers::empty()),
                "{jamo} should normalize to {latin}",
            );
        }
    }

    #[test]
    fn defaults_resolve_to_actions() {
        let km = KeyMap::default();
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("space").unwrap()),
            Some(Action::TogglePause)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("up").unwrap()),
            Some(Action::VolUp)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("down").unwrap()),
            Some(Action::VolDown)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("n").unwrap()),
            Some(Action::NextTrack)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("s").unwrap()),
            Some(Action::OpenSearch)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("S").unwrap()),
            Some(Action::ToggleShuffle)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("l").unwrap()),
            Some(Action::OpenLibrary)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("L").unwrap()),
            Some(Action::ToggleLyrics)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("delete").unwrap()),
            Some(Action::QueueRemove)
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("alt+shift+r").unwrap()),
            Some(Action::ToggleRadioMode)
        );
        assert_eq!(km.global_action(parse_chord("alt+shift+r").unwrap()), None);
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("alt+shift+r").unwrap()),
            None
        );
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("q").unwrap()),
            Some(Action::Back)
        );
        // Common nav falls through in a list context.
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("up").unwrap()),
            Some(Action::MoveUp)
        );
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("down").unwrap()),
            Some(Action::MoveDown)
        );
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("q").unwrap()),
            Some(Action::Back)
        );
        assert_eq!(
            km.action(KeyContext::SearchResults, parse_chord("q").unwrap()),
            Some(Action::Back)
        );
        assert_eq!(
            km.global_action(parse_chord("ctrl+q").unwrap()),
            Some(Action::Quit)
        );
        assert_eq!(
            km.global_action(parse_chord("ctrl+h").unwrap()),
            Some(Action::Home)
        );
        assert_eq!(
            km.global_action(parse_chord("?").unwrap()),
            Some(Action::ToggleHelp)
        );
    }

    #[test]
    fn page_and_jump_keys_resolve_in_list_contexts() {
        let km = KeyMap::default();
        // The four new nav primitives live in Common, so they fall through into any list
        // context (Library, Search results, …) and onto their standard physical keys.
        for ctx in [KeyContext::Library, KeyContext::SearchResults] {
            assert_eq!(
                km.action(ctx, parse_chord("pageup").unwrap()),
                Some(Action::PageUp)
            );
            assert_eq!(
                km.action(ctx, parse_chord("pagedown").unwrap()),
                Some(Action::PageDown)
            );
            assert_eq!(
                km.action(ctx, parse_chord("home").unwrap()),
                Some(Action::JumpTop)
            );
            assert_eq!(
                km.action(ctx, parse_chord("end").unwrap()),
                Some(Action::JumpBottom)
            );
        }
        // They round-trip through ids/labels like every other action.
        for a in [
            Action::PageUp,
            Action::PageDown,
            Action::JumpTop,
            Action::JumpBottom,
        ] {
            assert_ne!(a.id(), "?");
            assert_ne!(a.human_label(), "?");
        }
    }

    #[test]
    fn korean_2set_keys_resolve_default_actions() {
        let km = KeyMap::default();
        assert_eq!(
            km.action(
                KeyContext::Player,
                ev(KeyCode::Char('ㅂ'), KeyModifiers::empty())
            ),
            Some(Action::Back)
        );
        assert_eq!(
            km.action(
                KeyContext::Player,
                ev(KeyCode::Char('ㅣ'), KeyModifiers::empty())
            ),
            Some(Action::OpenLibrary)
        );
        assert_eq!(
            km.action(
                KeyContext::Player,
                ev(KeyCode::Char('ㅣ'), KeyModifiers::SHIFT)
            ),
            Some(Action::ToggleLyrics)
        );
        assert_eq!(
            km.action(
                KeyContext::Player,
                ev(KeyCode::Char('ㅇ'), KeyModifiers::empty())
            ),
            Some(Action::Download)
        );
        assert_eq!(
            km.action(
                KeyContext::SearchResults,
                ev(KeyCode::Char('ㅂ'), KeyModifiers::empty())
            ),
            Some(Action::Back)
        );
        assert_eq!(
            km.global_action(ev(KeyCode::Char('ㅂ'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            km.global_action(ev(KeyCode::Char('ㅗ'), KeyModifiers::CONTROL)),
            Some(Action::Home)
        );
        assert_eq!(
            km.global_action(ev(KeyCode::Char('ㄱ'), KeyModifiers::CONTROL)),
            Some(Action::ToggleStreaming)
        );
    }

    #[test]
    fn contextual_labels_describe_close_and_global_targets() {
        let _guard = crate::i18n::lock_for_test();
        assert_eq!(
            Action::Back.human_label_for(KeyContext::Library),
            "Close Library"
        );
        assert_eq!(
            Action::Confirm.human_label_for(KeyContext::SearchInput),
            "Search"
        );
        assert_eq!(
            Action::Confirm.human_label_for(KeyContext::SearchResults),
            "Play selected"
        );
        assert_eq!(
            Action::FocusPrev.human_label_for(KeyContext::SearchInput),
            "Focus search results"
        );
        assert_eq!(
            Action::FocusPrev.human_label_for(KeyContext::SearchResults),
            "Focus search box"
        );
        assert_eq!(
            Action::Back.human_label_for(KeyContext::SearchResults),
            "Close Search Results"
        );
        assert_eq!(
            Action::SettingsCancel.human_label_for(KeyContext::Settings),
            "Save + quit"
        );
        assert_eq!(
            Action::QueueRemove.human_label_for(KeyContext::Player),
            "Remove current from queue"
        );
        assert_eq!(
            Action::QueueRemove.human_label_for(KeyContext::Queue),
            "Remove selected from queue"
        );
        assert_eq!(Action::Quit.human_label_for(KeyContext::Global), "Quit");
        assert_eq!(Action::Home.human_label_for(KeyContext::Global), "Go home");
    }

    #[test]
    fn enter_backslash_and_play_all_resolve_in_library_and_search_results() {
        let _guard = crate::i18n::lock_for_test();
        let km = KeyMap::default();
        // Library: Enter = play selected, `\` = add to queue, `a` = play the whole tab.
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("enter").unwrap()),
            Some(Action::Confirm)
        );
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("\\").unwrap()),
            Some(Action::Enqueue)
        );
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("a").unwrap()),
            Some(Action::PlayAll)
        );
        // Search results: `\` = add to queue (Enter stays fixed in the handler, not the keymap).
        assert_eq!(
            km.action(KeyContext::SearchResults, parse_chord("\\").unwrap()),
            Some(Action::Enqueue)
        );
        assert_eq!(
            km.action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.action(KeyContext::SearchResults, parse_chord("s").unwrap()),
            Some(Action::FocusInput)
        );
        // The unified play/queue labels read consistently across both surfaces.
        assert_eq!(
            Action::Confirm.human_label_for(KeyContext::Library),
            "Play selected"
        );
        assert_eq!(Action::Enqueue.human_label(), "Add to queue");
        assert_eq!(Action::PlayAll.human_label(), "Play whole tab");
    }

    #[test]
    fn settings_close_binding_is_last_in_group() {
        let settings_actions = groups()
            .into_iter()
            .find_map(|(ctx, actions)| (ctx == KeyContext::Settings).then_some(actions))
            .unwrap();
        assert_eq!(settings_actions.last(), Some(&Action::SettingsCancel));
    }

    #[test]
    fn settings_has_no_standalone_save_binding() {
        let km = KeyMap::default();
        assert_eq!(
            km.action(KeyContext::Settings, parse_chord("s").unwrap()),
            None
        );

        let mut o = BTreeMap::new();
        o.insert("settings.settings_save".to_owned(), "S".to_owned());
        let km = KeyMap::from_overrides(&o);
        assert_eq!(
            km.action(KeyContext::Settings, parse_chord("S").unwrap()),
            None
        );
    }

    #[test]
    fn typeable_detection() {
        assert!(parse_chord("a").unwrap().is_typeable());
        assert!(parse_chord("?").unwrap().is_typeable());
        assert!(!parse_chord("ctrl+r").unwrap().is_typeable());
        assert!(!parse_chord("enter").unwrap().is_typeable());
    }

    #[test]
    fn ctrl_a_selects_all_in_text_inputs() {
        let km = KeyMap::default();
        let ctrl_a = parse_chord("ctrl+a").unwrap();
        assert_eq!(
            km.action(KeyContext::SearchInput, ctrl_a),
            Some(Action::SelectAll)
        );
        assert_eq!(
            km.action(KeyContext::AiInput, ctrl_a),
            Some(Action::SelectAll)
        );
        // Ctrl+A isn't a typed character, so it won't leak into the field as text.
        assert!(!ctrl_a.is_typeable());
    }

    #[test]
    fn rebind_rejects_conflict() {
        let mut km = KeyMap::default();
        // `q` is already Back in Player → binding TogglePause to it is rejected, and the
        // rejection names the offending chord, the action holding it, and where.
        let q = parse_chord("q").unwrap();
        let err = km
            .rebind(KeyContext::Player, Action::TogglePause, q)
            .unwrap_err();
        assert_eq!(err.existing, Action::Back);
        assert_eq!(err.chord, q);
        assert_eq!(err.ctx, KeyContext::Player);
        // Space is still pause; q is still back/close.
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("space").unwrap()),
            Some(Action::TogglePause)
        );
        assert_eq!(km.action(KeyContext::Player, q), Some(Action::Back));
    }

    #[test]
    fn rebind_moves_binding() {
        let mut km = KeyMap::default();
        let p_upper = parse_chord("x").unwrap();
        km.rebind(KeyContext::Player, Action::TogglePause, p_upper)
            .unwrap();
        assert_eq!(
            km.action(KeyContext::Player, p_upper),
            Some(Action::TogglePause)
        );
        // The old space binding is gone.
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("space").unwrap()),
            None
        );
    }

    #[test]
    fn local_rebind_can_shadow_common_navigation() {
        let mut km = KeyMap::default();
        let page_up = parse_chord("pageup").unwrap();

        km.rebind(KeyContext::Player, Action::TogglePause, page_up)
            .unwrap();

        assert_eq!(
            km.action(KeyContext::Player, page_up),
            Some(Action::TogglePause)
        );
        assert_eq!(
            km.action(KeyContext::Library, page_up),
            Some(Action::PageUp)
        );
    }

    #[test]
    fn common_rebind_can_be_shadowed_by_player_binding() {
        let mut km = KeyMap::default();
        let n = parse_chord("n").unwrap();

        km.rebind(KeyContext::Common, Action::PageDown, n).unwrap();

        assert_eq!(km.action(KeyContext::Player, n), Some(Action::NextTrack));
        assert_eq!(
            km.action(KeyContext::SearchResults, n),
            Some(Action::PageDown)
        );
    }

    #[test]
    fn non_global_rebind_rejects_global_conflict() {
        let mut km = KeyMap::default();
        let help = parse_chord("?").unwrap();

        let err = km
            .rebind(KeyContext::Common, Action::Confirm, help)
            .unwrap_err();

        assert_eq!(err.ctx, KeyContext::Global);
        assert_eq!(err.existing, Action::ToggleHelp);
        assert_eq!(err.chord, help);
    }

    #[test]
    fn global_rebind_rejects_default_context_conflicts() {
        for (ctx, existing, chord) in [
            (
                KeyContext::Global,
                Action::Quit,
                parse_chord("ctrl+q").unwrap(),
            ),
            (
                KeyContext::Player,
                Action::TogglePause,
                parse_chord("space").unwrap(),
            ),
            (
                KeyContext::Common,
                Action::PageUp,
                parse_chord("pageup").unwrap(),
            ),
            (
                KeyContext::Library,
                Action::LibraryFilter,
                parse_chord("/").unwrap(),
            ),
            (
                KeyContext::SearchInput,
                Action::SelectAll,
                parse_chord("ctrl+a").unwrap(),
            ),
        ] {
            let mut km = KeyMap::default();
            let err = km
                .rebind(KeyContext::Global, Action::ToggleHelp, chord)
                .unwrap_err();
            assert_eq!(err.ctx, ctx);
            assert_eq!(err.existing, existing);
            assert_eq!(err.chord, chord);
        }
    }

    #[test]
    fn global_rebind_rejects_dynamically_bound_context_conflicts() {
        for (ctx, action, chord) in [
            (
                KeyContext::Queue,
                Action::QueueRemove,
                parse_chord("f5").unwrap(),
            ),
            (
                KeyContext::SearchResults,
                Action::Enqueue,
                parse_chord("f6").unwrap(),
            ),
            (
                KeyContext::Settings,
                Action::ChangeDecrease,
                parse_chord("f7").unwrap(),
            ),
            (
                KeyContext::AiInput,
                Action::SelectAll,
                parse_chord("f8").unwrap(),
            ),
        ] {
            let mut km = KeyMap::default();
            km.rebind(ctx, action, chord).unwrap();

            let err = km
                .rebind(KeyContext::Global, Action::ToggleHelp, chord)
                .unwrap_err();

            assert_eq!(err.ctx, ctx);
            assert_eq!(err.existing, action);
            assert_eq!(err.chord, chord);
        }
    }

    #[test]
    fn search_source_menu_tab_can_shadow_common_focus_next() {
        let mut km = KeyMap::default();
        let f7 = parse_chord("f7").unwrap();
        let tab = parse_chord("tab").unwrap();

        km.rebind(KeyContext::SearchInput, Action::ToggleSearchSourceMenu, f7)
            .unwrap();
        assert_eq!(
            km.action(KeyContext::SearchInput, f7),
            Some(Action::ToggleSearchSourceMenu)
        );
        assert_eq!(
            km.action(KeyContext::SearchResults, f7),
            Some(Action::ToggleSearchSourceMenu)
        );
        assert_eq!(
            km.action(KeyContext::SearchInput, tab),
            Some(Action::FocusNext)
        );

        km.reset(KeyContext::SearchInput, Action::ToggleSearchSourceMenu)
            .unwrap();
        assert_eq!(
            km.action(KeyContext::SearchInput, tab),
            Some(Action::ToggleSearchSourceMenu)
        );
        assert_eq!(
            km.action(KeyContext::SearchResults, tab),
            Some(Action::ToggleSearchSourceMenu)
        );
        assert!(
            !km.to_overrides()
                .contains_key("search_input.toggle_search_source_menu")
        );
    }

    #[test]
    fn rebinding_search_focus_toggle_updates_both_search_contexts() {
        let mut km = KeyMap::default();
        let f5 = parse_chord("f5").unwrap();
        km.rebind(KeyContext::SearchResults, Action::FocusPrev, f5)
            .unwrap();

        assert_eq!(
            km.context_action(KeyContext::SearchResults, f5),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchInput, f5),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
            None
        );
        assert_eq!(
            km.context_action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
            None
        );

        let overrides = km.to_overrides();
        assert_eq!(
            overrides
                .get("search_results.focus_prev")
                .map(String::as_str),
            Some("f5")
        );
        assert_eq!(
            overrides.get("search_input.focus_prev").map(String::as_str),
            Some("f5")
        );
    }

    #[test]
    fn search_focus_toggle_rebind_rejects_conflicts_on_either_side() {
        let mut km = KeyMap::default();
        let select_all = parse_chord("ctrl+a").unwrap();
        let err = km
            .rebind(KeyContext::SearchResults, Action::FocusPrev, select_all)
            .unwrap_err();

        assert_eq!(err.ctx, KeyContext::SearchInput);
        assert_eq!(err.existing, Action::SelectAll);
        assert_eq!(err.chord, select_all);
        assert_eq!(
            km.context_action(KeyContext::SearchInput, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
    }

    #[test]
    fn overrides_round_trip() {
        let mut km = KeyMap::default();
        km.rebind(
            KeyContext::Player,
            Action::TogglePause,
            parse_chord("x").unwrap(),
        )
        .unwrap();
        let overrides = km.to_overrides();
        assert_eq!(
            overrides.get("player.toggle_pause").map(String::as_str),
            Some("x")
        );
        let restored = KeyMap::from_overrides(&overrides);
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
    fn legacy_open_search_override_still_focuses_search_input() {
        let mut o = BTreeMap::new();
        o.insert("player.open_search".to_owned(), "E".to_owned());
        let km = KeyMap::from_overrides(&o);

        assert_eq!(
            km.action(KeyContext::Player, parse_chord("E").unwrap()),
            Some(Action::OpenSearch)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
        assert_eq!(
            km.action(KeyContext::SearchResults, parse_chord("E").unwrap()),
            Some(Action::FocusInput)
        );
    }

    #[test]
    fn legacy_search_results_focus_input_override_is_respected() {
        let mut o = BTreeMap::new();
        o.insert("search_results.focus_input".to_owned(), "I".to_owned());
        let km = KeyMap::from_overrides(&o);

        assert_eq!(
            km.action(KeyContext::SearchResults, parse_chord("I").unwrap()),
            Some(Action::FocusInput)
        );
        assert_eq!(
            km.context_action(KeyContext::SearchResults, parse_chord("backtab").unwrap()),
            Some(Action::FocusPrev)
        );
    }

    #[test]
    fn legacy_global_radio_mode_override_moves_to_player() {
        let mut o = BTreeMap::new();
        o.insert("global.toggle_radio_mode".to_owned(), "f8".to_owned());
        let km = KeyMap::from_overrides(&o);

        assert_eq!(
            km.action(KeyContext::Player, parse_chord("f8").unwrap()),
            Some(Action::ToggleRadioMode)
        );
        assert_eq!(km.global_action(parse_chord("f8").unwrap()), None);
        assert_eq!(
            km.action(KeyContext::Library, parse_chord("f8").unwrap()),
            None
        );
    }

    #[test]
    fn unknown_overrides_are_ignored() {
        let mut o = BTreeMap::new();
        o.insert("bogus.thing".to_owned(), "x".to_owned());
        o.insert(
            "player.toggle_pause".to_owned(),
            "not a real chord!!".to_owned(),
        );
        // Falls back to defaults; doesn't panic.
        let km = KeyMap::from_overrides(&o);
        assert_eq!(
            km.action(KeyContext::Player, parse_chord("space").unwrap()),
            Some(Action::TogglePause)
        );
    }

    #[test]
    fn text_zoom_defaults_bind_ctrl_equals_and_minus_globally() {
        let km = KeyMap::default();
        assert_eq!(
            km.global_action(parse_chord("ctrl+=").unwrap()),
            Some(Action::TextZoomIn)
        );
        assert_eq!(
            km.global_action(parse_chord("ctrl+-").unwrap()),
            Some(Action::TextZoomOut)
        );
        // Ctrl chords are non-typeable, so the zoom keys keep working inside the search
        // and DJ Gem text fields (`is_typeable` gates global suppression there).
        assert!(!parse_chord("ctrl+=").unwrap().is_typeable());
        assert!(!parse_chord("ctrl+-").unwrap().is_typeable());
        // And they survive a config round-trip like any rebindable chord.
        for spelled in ["ctrl+=", "ctrl+-"] {
            let chord = parse_chord(spelled).unwrap();
            assert_eq!(chord_to_config(chord), spelled);
        }
    }

    #[test]
    fn editable_entries_cover_every_binding() {
        assert_eq!(editable_entries().len(), default_bindings().len());
        assert!(
            editable_entries().contains(&(KeyContext::Player, Action::QueueRemove)),
            "Settings > Keys should list the player delete binding"
        );
        assert!(
            editable_entries().contains(&(KeyContext::Player, Action::ToggleRadioMode)),
            "Settings > Keys should list the Radio / Normal mode binding"
        );
        // Every action has a stable id and label.
        for (_, action, _) in default_bindings() {
            assert_ne!(action.id(), "?");
            assert_ne!(action.human_label(), "?");
        }
    }
}
