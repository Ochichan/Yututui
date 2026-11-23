//! Fuzzy Command & Jump Palette (`Ctrl+K`).
//!
//! Pure TUI-layer modal that indexes current-track chapters, bookmarks, local playlists,
//! DJ presets, context-aware actions, Settings fields, Theme presets, EQ presets, and
//! animation toggles.
//!
//! Preview policy:
//! - Theme presets and animation toggles preview strictly in memory while the palette is
//!   open (zero disk writes, zero sync writes); `Esc` restores the exact pre-open snapshot.
//! - EQ presets are Enter-apply only (never previewed on arrow movement, avoiding daemon
//!   IPC chatter, remote broadcast noise, and mid-listening sound jumps).

use super::*;
use crate::config::AnimationsConfig;
use crate::listening::{BookmarkRecord, DjPreset, ListeningProjection};
use crate::settings::{Field, SettingsTab};
use crate::theme::ThemePreset;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCategory {
    Chapter,
    Bookmark,
    Action,
    Playlist,
    Preset,
    Setting,
    Theme,
    Eq,
    Animation,
}

impl PaletteCategory {
    pub fn badge(self) -> &'static str {
        match self {
            Self::Chapter => t!("Chapter", "챕터", "チャプター"),
            Self::Bookmark => t!("Bookmark", "북마크", "ブックマーク"),
            Self::Action => t!("Action", "명령", "操作"),
            Self::Playlist => t!("Playlist", "플레이리스트", "プレイリスト"),
            Self::Preset => t!("DJ Preset", "DJ 프리셋", "DJプリセット"),
            Self::Setting => t!("Setting", "설정", "設定"),
            Self::Theme => t!("Theme", "테마", "テーマ"),
            Self::Eq => "EQ",
            Self::Animation => t!("Anim", "애니", "アニメ"),
        }
    }

    fn search_aliases(self) -> &'static str {
        match self {
            Self::Chapter => "chapter 챕터 チャプター",
            Self::Bookmark => "bookmark 북마크 ブックマーク",
            Self::Action => "action command 명령 コマンド",
            Self::Playlist => "playlist 플레이리스트 プレイリスト",
            Self::Preset => "preset dj 프리셋 プリセット",
            Self::Setting => "setting config 설정 設定",
            Self::Theme => "theme color 테마 テーマ",
            Self::Eq => "eq equalizer 이퀄라이저 イコライザー",
            Self::Animation => "animation anim fx 애니메이션 アニメ",
        }
    }
}

#[derive(Clone)]
pub(crate) enum PaletteTarget {
    Chapter { start_secs: f64, title: String },
    Bookmark(BookmarkRecord),
    DjPreset(DjPreset),
    Playlist { id: String },
    Setting { tab: SettingsTab, field: Field },
    GlobalAction(Action),
    PlayerAction(Action),
    ContextAction { ctx: KeyContext, action: Action },
    Theme(ThemePreset),
    Eq(EqPreset),
    Animation(Field),
}

#[derive(Clone)]
pub struct PaletteEntry {
    pub category: PaletteCategory,
    pub title: String,
    pub subtitle: Option<String>,
    pub shortcut: Option<String>,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    priority: u8,
    search_text: String,
    pub(crate) target: PaletteTarget,
}

#[derive(Clone)]
struct PalettePreviewSnapshot {
    theme: ThemeConfig,
    animations: AnimationsConfig,
    active: bool,
}

pub struct CommandPaletteState {
    pub query: String,
    pub cursor: TextCursor,
    pub selected: usize,
    pub scroll: crate::ui::scroll::ScrollState,
    pub entries: Vec<PaletteEntry>,
    pub filtered: Vec<usize>,
    pub error_hint: Option<String>,
    preview: PalettePreviewSnapshot,
}

impl CommandPaletteState {
    pub fn preview_active(&self) -> bool {
        self.preview.active
    }

    pub fn filtered_entry(&self, visible_idx: usize) -> Option<&PaletteEntry> {
        let entry_idx = *self.filtered.get(visible_idx)?;
        self.entries.get(entry_idx)
    }

    pub fn selected_entry(&self) -> Option<&PaletteEntry> {
        self.filtered_entry(self.selected)
    }

    fn rebuild_filter(&mut self) {
        let query = self.query.trim();
        let mut scored: Vec<(i32, bool, u8, usize)> = Vec::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            if query.is_empty() {
                scored.push((0, entry.enabled, entry.priority, idx));
            } else if let Some(score) = fuzzy_score(query, &entry.search_text) {
                scored.push((score, entry.enabled, entry.priority, idx));
            }
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.cmp(&b.3))
        });
        self.filtered = scored.into_iter().map(|(_, _, _, idx)| idx).collect();
        self.selected = self
            .filtered
            .iter()
            .position(|&idx| self.entries[idx].enabled)
            .unwrap_or(0);
        self.scroll.reset();
        self.error_hint = None;
    }
}

/// Multi-token case-insensitive fuzzy scorer supporting ASCII, Korean, and Japanese text.
/// Returns `None` if any whitespace-separated token fails to match `haystack`.
pub(crate) fn fuzzy_score(query: &str, haystack: &str) -> Option<i32> {
    let q_lower = query.to_lowercase();
    let mut total = 0i32;
    for token in q_lower.split_whitespace() {
        total += score_token(token, haystack)?;
    }
    Some(total)
}

fn score_token(token: &str, haystack: &str) -> Option<i32> {
    if token.is_empty() {
        return Some(0);
    }
    if let Some(pos) = haystack.find(token) {
        let boundary = pos == 0
            || haystack[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_alphanumeric());
        let bonus = if pos == 0 {
            120
        } else if boundary {
            95
        } else {
            70
        };
        return Some(bonus - (pos.min(40) as i32));
    }
    // Ordered subsequence fallback for abbreviated queries like "slpt" or "ab".
    let mut hay_chars = haystack.char_indices().peekable();
    let mut score = 20i32;
    let mut prev_end: Option<usize> = None;
    for tc in token.chars() {
        let mut found = None;
        for (idx, hc) in hay_chars.by_ref() {
            if hc == tc {
                found = Some(idx);
                break;
            }
        }
        let pos = found?;
        if prev_end == Some(pos) {
            score += 6;
        }
        prev_end = Some(pos + tc.len_utf8());
    }
    Some(score)
}

impl App {
    pub(in crate::app) fn open_command_palette(&mut self) {
        // Close transient dropdowns/menus before opening the palette so z-order stays clean.
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.overlays.context_menu = None;
        let entries = self.build_palette_entries();
        let preview = PalettePreviewSnapshot {
            theme: self.theme.clone(),
            animations: self.config.animations,
            active: false,
        };
        let mut state = CommandPaletteState {
            query: String::new(),
            cursor: TextCursor::default(),
            selected: 0,
            scroll: crate::ui::scroll::ScrollState::default(),
            entries,
            filtered: Vec::new(),
            error_hint: None,
            preview,
        };
        state.rebuild_filter();
        self.overlays.command_palette = Some(state);
        self.sync_command_palette_preview();
        self.dirty = true;
    }

    /// Close the palette and restore any in-memory theme/animation preview.
    pub(in crate::app) fn close_command_palette_revert(&mut self) {
        if let Some(state) = self.overlays.command_palette.take()
            && state.preview.active
        {
            self.theme = state.preview.theme;
            self.config.animations = state.preview.animations;
        }
        self.dirty = true;
    }

    /// Strictly in-memory preview for Theme presets and Animation toggles.
    /// Returns `()` so it can never emit `PersistCmd` or daemon/player commands.
    fn sync_command_palette_preview(&mut self) {
        let (orig_theme, orig_anim, target) = {
            let Some(state) = self.overlays.command_palette.as_ref() else {
                return;
            };
            let target = state
                .selected_entry()
                .filter(|entry| entry.enabled)
                .map(|entry| entry.target.clone());
            (
                state.preview.theme.clone(),
                state.preview.animations,
                target,
            )
        };

        let mut active = false;
        match target {
            Some(PaletteTarget::Theme(preset)) => {
                let mut next = orig_theme;
                next.set_preset(preset);
                self.theme = next.normalized();
                self.config.animations = orig_anim;
                active = true;
            }
            Some(PaletteTarget::Animation(field)) => {
                self.theme = orig_theme;
                let mut next_anim = orig_anim;
                // Radio renders `radio_master`, not the music-mode `master` flag.
                toggle_palette_animation_field(&mut next_anim, field, self.radio_dedicated_mode);
                self.config.animations = next_anim;
                active = true;
            }
            // EQ presets and all other entries deliberately do NOT live-preview.
            _ => {
                self.theme = orig_theme;
                self.config.animations = orig_anim;
            }
        }
        if let Some(state) = self.overlays.command_palette.as_mut() {
            state.preview.active = active;
        }
        self.dirty = true;
    }

    pub(in crate::app) fn on_key_command_palette(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = Chord::from(k);
        if k.code == KeyCode::Esc
            || matches!(
                self.keymap.global_action(chord),
                Some(Action::OpenCommandPalette)
            )
        {
            self.close_command_palette_revert();
            return Vec::new();
        }

        match k.code {
            KeyCode::Enter => return self.execute_command_palette_selected(),
            KeyCode::Up => {
                self.move_command_palette_selection(-1);
                return Vec::new();
            }
            KeyCode::Down => {
                self.move_command_palette_selection(1);
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.move_command_palette_selection(-8);
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.move_command_palette_selection(8);
                return Vec::new();
            }
            KeyCode::Home if !k.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(state) = self.overlays.command_palette.as_mut() {
                    state.selected = 0;
                    state.error_hint = None;
                }
                self.sync_command_palette_preview();
                return Vec::new();
            }
            KeyCode::End if !k.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(state) = self.overlays.command_palette.as_mut() {
                    state.selected = state.filtered.len().saturating_sub(1);
                    state.error_hint = None;
                }
                self.sync_command_palette_preview();
                return Vec::new();
            }
            _ => {}
        }

        // Standard text-edit keys (Backspace, Ctrl+Backspace, Left/Right, word movement).
        if let Some(action) = self.keymap.action(KeyContext::Common, chord)
            && matches!(
                action,
                Action::DeleteChar
                    | Action::DeleteWord
                    | Action::MoveCursorLeft
                    | Action::MoveCursorRight
                    | Action::MoveCursorWordLeft
                    | Action::MoveCursorWordRight
            )
        {
            let changed = if let Some(state) = self.overlays.command_palette.as_mut() {
                let res = apply_text_edit_action(action, &mut state.cursor, &mut state.query);
                if matches!(res, Some(TextEditResult::BufferChanged(true))) {
                    state.rebuild_filter();
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if changed {
                self.sync_command_palette_preview();
            }
            self.dirty = true;
            return Vec::new();
        }

        if let KeyCode::Char(c) = k.code
            && !k.modifiers.contains(KeyModifiers::CONTROL)
            && !k.modifiers.contains(KeyModifiers::ALT)
        {
            if let Some(state) = self.overlays.command_palette.as_mut() {
                state.cursor.insert_char(&mut state.query, c);
                state.rebuild_filter();
            }
            self.sync_command_palette_preview();
            self.dirty = true;
        }
        Vec::new()
    }

    fn move_command_palette_selection(&mut self, delta: isize) {
        if let Some(state) = self.overlays.command_palette.as_mut() {
            let len = state.filtered.len();
            if len == 0 {
                state.selected = 0;
            } else if delta < 0 {
                state.selected = state.selected.saturating_sub(delta.unsigned_abs());
            } else {
                state.selected = state
                    .selected
                    .saturating_add(delta as usize)
                    .min(len.saturating_sub(1));
            }
            state.error_hint = None;
        }
        self.sync_command_palette_preview();
    }

    pub(in crate::app) fn overlay_blocks_pointer_drag(&self) -> bool {
        self.overlays.why_gem_video_id.is_some()
            || self.overlays.station_card.is_some()
            || self.overlays.listening.is_some()
            || self.overlays.command_palette.is_some()
    }

    pub(in crate::app) fn scroll_command_palette_if_open(&mut self, up: bool) -> bool {
        if self.overlays.command_palette.is_none() {
            return false;
        }
        self.move_command_palette_selection(if up { -3 } else { 3 });
        true
    }

    pub(in crate::app) fn on_mouse_command_palette(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        match self.mouse_target_at(col, row) {
            Some(MouseTarget::CommandPaletteRow(visible_idx)) => {
                if let Some(state) = self.overlays.command_palette.as_mut()
                    && visible_idx < state.filtered.len()
                {
                    state.selected = visible_idx;
                }
                self.execute_command_palette_selected()
            }
            Some(MouseTarget::CommandPaletteSurface) => Vec::new(),
            _ => {
                self.close_command_palette_revert();
                Vec::new()
            }
        }
    }

    fn execute_command_palette_selected(&mut self) -> Vec<Cmd> {
        let Some(state) = self.overlays.command_palette.as_mut() else {
            return Vec::new();
        };
        let Some(entry) = state.selected_entry().cloned() else {
            return Vec::new();
        };
        if !entry.enabled {
            state.error_hint = entry.disabled_reason.or_else(|| {
                Some(
                    t!(
                        "Not available in the current view",
                        "현재 화면에서는 실행할 수 없어요",
                        "現在の画面では実行できません"
                    )
                    .to_owned(),
                )
            });
            self.dirty = true;
            return Vec::new();
        }

        // Restore any transient preview before executing, then commit the chosen target cleanly.
        let snapshot = self
            .overlays
            .command_palette
            .take()
            .expect("checked above")
            .preview;
        if snapshot.active {
            self.theme = snapshot.theme;
            self.config.animations = snapshot.animations;
        }
        self.dirty = true;

        match entry.target {
            PaletteTarget::Chapter { start_secs, title } => {
                self.set_status_info(format!(
                    "{}: {}",
                    t!("Chapter", "챕터", "チャプター"),
                    title
                ));
                self.player_intent(
                    "seek_chapter",
                    PlayerCmd::SeekAbsolute {
                        seconds: start_secs,
                        precision: crate::player::SeekPrecision::InteractiveFast,
                    },
                    PlayerCommit::Seek {
                        optimistic_position: Some(start_secs),
                    },
                )
            }
            PaletteTarget::Bookmark(bookmark) => {
                self.jump_listening(bookmark.track, bookmark.position_ms)
            }
            PaletteTarget::DjPreset(preset) => self.apply_dj_preset(preset),
            PaletteTarget::Playlist { id } => self.jump_to_playlist(id),
            PaletteTarget::Setting { tab, field } => {
                self.open_settings_at(tab, field);
                Vec::new()
            }
            PaletteTarget::GlobalAction(action) => self.execute_global_palette_action(action),
            PaletteTarget::PlayerAction(action) => self.on_player_action(action),
            PaletteTarget::ContextAction { ctx, action } => {
                self.execute_context_palette_action(ctx, action)
            }
            PaletteTarget::Theme(preset) => self.commit_palette_theme(preset),
            PaletteTarget::Eq(preset) => self.eq_preset_intent(preset, true),
            PaletteTarget::Animation(field) => self.commit_palette_animation(field),
        }
    }

    fn jump_to_playlist(&mut self, id: String) -> Vec<Cmd> {
        let songs = self
            .playlists
            .list()
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.songs.clone());
        let Some(songs) = songs else {
            return Vec::new();
        };
        self.mode = Mode::Library;
        self.library_ui.tab = LibraryTab::Playlists;
        self.clear_library_filter();
        self.library_ui.open_playlist = Some(id);
        self.library_ui.selected = 0;
        self.library_ui.anchor = 0;
        self.dirty = true;
        self.request_romanization_for_songs(&songs)
    }

    fn open_settings_at(&mut self, tab: SettingsTab, field: Field) {
        self.open_settings();
        if let Some(st) = self.settings.as_deref_mut() {
            st.tab = tab;
            if let Some(row) = st.fields().iter().position(|f| *f == field) {
                st.row = row;
            }
        }
        self.bridges.reset_settings_scroll();
        self.dirty = true;
    }

    fn commit_palette_theme(&mut self, preset: ThemePreset) -> Vec<Cmd> {
        let mut next = self.theme.clone();
        next.set_preset(preset);
        self.theme = next.normalized();
        if self.radio_dedicated_mode {
            self.radio_mode.radio_mode_theme = Some(self.theme.clone());
            self.config.radio_theme = Some(self.theme.clone());
        } else if self.local_dedicated_mode {
            self.local_mode.local_mode_theme = Some(self.theme.clone());
            self.config.local_theme = Some(self.theme.clone());
        } else {
            self.config.theme = self.theme.clone();
        }
        // An open Settings draft is what Save writes back. Mirror the committed theme or
        // closing Settings restores the pre-palette preset.
        let committed = self.theme.clone();
        if let Some(settings) = self.settings.as_mut() {
            settings.draft.theme = committed;
        }
        self.set_status_info(format!(
            "{}: {}",
            t!("Theme", "테마", "テーマ"),
            preset.label()
        ));
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    fn commit_palette_animation(&mut self, field: Field) -> Vec<Cmd> {
        let radio = self.radio_dedicated_mode;
        let before = self.config.animations;
        let enabled_after =
            toggle_palette_animation_field(&mut self.config.animations, field, radio);
        self.mirror_palette_animation_into_settings_draft(field, before, enabled_after);
        self.set_status_info(format!(
            "{}: {}",
            field.label(),
            if enabled_after { "✓" } else { "✗" }
        ));
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    /// `SettingsDraft::apply_to` copies `draft.animations` wholesale, so a palette commit
    /// made while Settings is open has to land in the draft or Save reverts it. Only the
    /// fields this commit actually changed are copied, leaving other unsaved draft edits.
    fn mirror_palette_animation_into_settings_draft(
        &mut self,
        field: Field,
        before: AnimationsConfig,
        enabled_after: bool,
    ) {
        let after = self.config.animations;
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        let draft = &mut settings.draft.animations;
        if before.master != after.master {
            draft.master = after.master;
        }
        if before.radio_master != after.radio_master {
            draft.radio_master = after.radio_master;
        }
        if field != Field::AnimMaster
            && let Some(flag) = field.anim_flag(draft)
        {
            *flag = enabled_after;
        }
    }

    fn execute_global_palette_action(&mut self, action: Action) -> Vec<Cmd> {
        if let Some(chord) = self.keymap.chord(KeyContext::Global, action)
            && let Some(cmds) = self.route_global_key_context(chord)
        {
            return cmds;
        }
        // Fallback when user unbound a global action: handle core global actions directly.
        match action {
            Action::OpenBookmarks => self.open_listening(ListeningTab::Bookmarks),
            Action::OpenDjPresets => self.open_listening(ListeningTab::Presets),
            Action::OpenPassport => self.open_listening(ListeningTab::Passport),
            Action::ToggleHelp => {
                self.overlays.help_visible = true;
                self.bridges.help_scroll.reset();
                Vec::new()
            }
            Action::ToggleAbout => {
                self.overlays.about_visible = true;
                Vec::new()
            }
            Action::WhyAi => {
                self.open_selected_why_gem();
                Vec::new()
            }
            Action::ToggleAnimations => self.toggle_animations(),
            Action::ToggleControlBox => self.toggle_control_box(),
            Action::ToggleZoomWheelLock => self.toggle_zoom_wheel_lock(),
            // Zoom is dispatched in `on_key` before `route_global_key_context`, so the
            // chord replay above never sees it. Run the same step the key uses.
            Action::TextZoomIn => self.zoom_step(true),
            Action::TextZoomOut => self.zoom_step(false),
            Action::Home => self.go_home(),
            Action::Quit => self.quit_app(),
            _ => Vec::new(),
        }
    }

    fn execute_context_palette_action(&mut self, ctx: KeyContext, action: Action) -> Vec<Cmd> {
        let Some(chord) = self.keymap.chord(ctx, action) else {
            return Vec::new();
        };
        // The row is an action, not a keystroke. Replaying a typeable chord into the
        // search box or a library/local filter inserts that character and drops the command.
        self.release_text_entry_for_palette_action(ctx, chord);
        let key_event = KeyEvent::new(chord.code, chord.mods);
        match ctx {
            KeyContext::Player => self.on_player_action(action),
            KeyContext::Library | KeyContext::Playlists => self.on_key_library(key_event),
            KeyContext::SearchResults => self.on_key_search(key_event),
            KeyContext::LocalDeck => self.on_key_local(key_event),
            KeyContext::Queue => self.on_key_queue(key_event),
            _ => Vec::new(),
        }
    }

    /// Step out of a focused editor so the screen handler resolves the palette action.
    /// Search only swallows typeable chords; the library and local filters swallow every key.
    fn release_text_entry_for_palette_action(&mut self, ctx: KeyContext, chord: Chord) {
        match ctx {
            KeyContext::SearchResults
                if chord.is_typeable() && self.search.focus == SearchFocus::Input =>
            {
                self.search.focus = SearchFocus::Results;
                self.search.select_all = false;
            }
            KeyContext::Library | KeyContext::Playlists if self.library_ui.filter_editing => {
                self.library_ui.filter_editing = false;
            }
            KeyContext::LocalDeck if self.local_mode.ui.filter_editing => {
                self.local_mode.ui.filter_editing = false;
            }
            _ => {}
        }
    }

    fn build_palette_entries(&self) -> Vec<PaletteEntry> {
        let mut entries = Vec::with_capacity(192);
        let retro = self.retro_mode();

        // 1. Current track chapters (top priority on empty query when present).
        let chapters = self.loaded_chapters();
        let current_ch_idx = self.playback.time_pos.and_then(|pos| {
            yututui_core::chapters::current_index(chapters.iter().map(|c| c.start_secs), pos)
        });
        for (idx, chapter) in chapters.iter().enumerate() {
            let ts = crate::util::format::time(chapter.start_secs);
            let title = chapter.title.trim();
            let display_title = if title.is_empty() {
                format!("{} {}", t!("Chapter", "챕터", "チャプター"), idx + 1)
            } else {
                title.to_owned()
            };
            let subtitle = if current_ch_idx == Some(idx) {
                format!("{ts} · {}", t!("now", "현재", "現在"))
            } else {
                ts.clone()
            };
            let search_text = format!(
                "{} {} {} {}",
                display_title,
                ts,
                idx + 1,
                PaletteCategory::Chapter.search_aliases()
            )
            .to_lowercase();
            entries.push(PaletteEntry {
                category: PaletteCategory::Chapter,
                title: display_title.clone(),
                subtitle: Some(subtitle),
                shortcut: None,
                enabled: true,
                disabled_reason: None,
                priority: 0,
                search_text,
                target: PaletteTarget::Chapter {
                    start_secs: chapter.start_secs,
                    title: display_title,
                },
            });
        }

        // 2. Listening records: bookmarks & DJ presets (when enabled).
        if self.listening_records_enabled()
            && let Ok(projection) = ListeningProjection::from_ledger(&self.personal_state.ledger)
        {
            let current_video_id = self.queue.current().map(|s| s.video_id.as_str());
            for list in projection.bookmarks.values() {
                for bookmark in list {
                    let is_current =
                        current_video_id.is_some_and(|vid| match &bookmark.track.key {
                            crate::personal_state::PortableTrackKey::Catalog {
                                exact_catalog_id,
                                ..
                            } => exact_catalog_id == vid,
                            crate::personal_state::PortableTrackKey::OpenSubsonic {
                                item_id,
                                ..
                            } => item_id == vid,
                            crate::personal_state::PortableTrackKey::LocalPlaceholder {
                                portable_placeholder_id,
                            } => portable_placeholder_id == vid,
                        });
                    let ts = crate::util::format::time(bookmark.position_ms as f64 / 1000.0);
                    let subtitle = format!("{} · {}", ts, bookmark.track.title);
                    let search_text = format!(
                        "{} {} {} {}",
                        bookmark.label,
                        bookmark.track.title,
                        ts,
                        PaletteCategory::Bookmark.search_aliases()
                    )
                    .to_lowercase();
                    entries.push(PaletteEntry {
                        category: PaletteCategory::Bookmark,
                        title: bookmark.label.clone(),
                        subtitle: Some(subtitle),
                        shortcut: None,
                        enabled: true,
                        disabled_reason: None,
                        priority: if is_current { 1 } else { 5 },
                        search_text,
                        target: PaletteTarget::Bookmark(bookmark.clone()),
                    });
                }
            }
            for list in projection.dj_presets.values() {
                for preset in list {
                    let search_text = format!(
                        "{} {}",
                        preset.name,
                        PaletteCategory::Preset.search_aliases()
                    )
                    .to_lowercase();
                    entries.push(PaletteEntry {
                        category: PaletteCategory::Preset,
                        title: preset.name.clone(),
                        subtitle: Some(
                            t!(
                                "Load DJ preset",
                                "DJ 프리셋 불러오기",
                                "DJプリセットを読み込む"
                            )
                            .to_owned(),
                        ),
                        shortcut: None,
                        enabled: true,
                        disabled_reason: None,
                        priority: 4,
                        search_text,
                        target: PaletteTarget::DjPreset(preset.clone()),
                    });
                }
            }
        }

        // 3. Actions: Global + Player + active screen context (with availability gating).
        let active_ctx = self.palette_active_context();
        let mut seen_actions: HashSet<(KeyContext, Action)> = HashSet::new();
        for (ctx, actions) in crate::keymap::groups() {
            let include_ctx =
                matches!(ctx, KeyContext::Global | KeyContext::Player) || Some(ctx) == active_ctx;
            if !include_ctx {
                continue;
            }
            for action in actions {
                if !is_palette_action(action) || !seen_actions.insert((ctx, action)) {
                    continue;
                }
                let shortcut = self
                    .keymap
                    .chord(ctx, action)
                    .map(|chord| crate::keymap::format_chord_for_display(chord, retro));
                let availability = self.palette_action_availability(ctx, action, active_ctx);
                let (enabled, disabled_reason) = match availability {
                    Ok(()) => (true, None),
                    Err(reason) => (false, Some(reason.to_owned())),
                };
                let label = action.human_label_for(ctx).to_owned();
                let search_text = format!(
                    "{} {} {} {} {}",
                    label,
                    action.id().replace('_', " "),
                    ctx.title(),
                    shortcut.as_deref().unwrap_or(""),
                    PaletteCategory::Action.search_aliases()
                )
                .to_lowercase();
                let priority = if is_featured_action(action) {
                    2
                } else if ctx == KeyContext::Global || Some(ctx) == active_ctx {
                    3
                } else {
                    4
                };
                let target = match ctx {
                    KeyContext::Global => PaletteTarget::GlobalAction(action),
                    KeyContext::Player => PaletteTarget::PlayerAction(action),
                    other => PaletteTarget::ContextAction { ctx: other, action },
                };
                entries.push(PaletteEntry {
                    category: PaletteCategory::Action,
                    title: label,
                    subtitle: disabled_reason
                        .clone()
                        .or_else(|| Some(ctx.title().to_owned())),
                    shortcut,
                    enabled,
                    disabled_reason,
                    priority,
                    search_text,
                    target,
                });
            }
        }

        // 4. Local playlists (jump into Library -> Playlists -> opened playlist).
        for playlist in self.playlists.list() {
            let count = playlist.songs.len();
            let subtitle = match crate::i18n::current() {
                crate::i18n::Language::Korean => format!("{count}곡"),
                crate::i18n::Language::Japanese => format!("{count}曲"),
                crate::i18n::Language::English => format!("{count} tracks"),
            };
            let search_text = format!(
                "{} {}",
                playlist.name,
                PaletteCategory::Playlist.search_aliases()
            )
            .to_lowercase();
            entries.push(PaletteEntry {
                category: PaletteCategory::Playlist,
                title: playlist.name.clone(),
                subtitle: Some(subtitle),
                shortcut: None,
                enabled: true,
                disabled_reason: None,
                priority: 4,
                search_text,
                target: PaletteTarget::Playlist {
                    id: playlist.id.clone(),
                },
            });
        }

        // 5. Settings fields (jump directly to tab + row).
        let radio_context = self.radio_dedicated_mode || self.current_is_radio_stream();
        for tab in SettingsTab::ALL {
            for field in tab.fields() {
                if tab == SettingsTab::Playback
                    && !radio_context
                    && matches!(field, Field::RadioRecording | Field::Atlas(_))
                {
                    continue;
                }
                if tab == SettingsTab::Ai && retro && field == Field::ClearRomanizedTitleCache {
                    continue;
                }
                // Color-role overrides and individual animation flags have dedicated entries/pickers.
                if matches!(field, Field::ThemeColor(_) | Field::Band(_)) {
                    continue;
                }
                let label = field.label();
                let tab_label = tab.label();
                let search_text = format!(
                    "{} {} {}",
                    label,
                    tab_label,
                    PaletteCategory::Setting.search_aliases()
                )
                .to_lowercase();
                entries.push(PaletteEntry {
                    category: PaletteCategory::Setting,
                    title: label,
                    subtitle: Some(tab_label.to_owned()),
                    shortcut: None,
                    enabled: true,
                    disabled_reason: None,
                    priority: 6,
                    search_text,
                    target: PaletteTarget::Setting { tab, field },
                });
            }
        }

        // 6. Theme presets (in-memory live preview, Esc reverts, Enter persists).
        let current_preset = self.theme.preset_enum();
        for preset in ThemePreset::ALL {
            let label = preset.label().to_owned();
            let subtitle = if preset == current_preset {
                Some(t!("active", "사용 중", "使用中").to_owned())
            } else {
                Some(t!("preview on select", "선택 시 미리보기", "選択でプレビュー").to_owned())
            };
            let search_text = format!(
                "{} {} {}",
                label,
                preset.id(),
                PaletteCategory::Theme.search_aliases()
            )
            .to_lowercase();
            entries.push(PaletteEntry {
                category: PaletteCategory::Theme,
                title: label,
                subtitle,
                shortcut: None,
                enabled: true,
                disabled_reason: None,
                priority: 7,
                search_text,
                target: PaletteTarget::Theme(preset),
            });
        }

        // 7. EQ presets (Enter-apply ONLY — never live-previewed on cursor move).
        for preset in EqPreset::CYCLE {
            let label = format!("EQ: {}", preset.label());
            let subtitle = if preset == self.audio.preset {
                Some(t!("active", "사용 중", "使用中").to_owned())
            } else {
                Some(t!("Enter to apply", "Enter로 적용", "Enterで適用").to_owned())
            };
            let search_text = format!(
                "{} {} {}",
                label,
                preset.label(),
                PaletteCategory::Eq.search_aliases()
            )
            .to_lowercase();
            entries.push(PaletteEntry {
                category: PaletteCategory::Eq,
                title: label,
                subtitle,
                shortcut: None,
                enabled: true,
                disabled_reason: None,
                priority: 7,
                search_text,
                target: PaletteTarget::Eq(preset),
            });
        }

        // 8. Popular animation toggles (in-memory live preview, Esc reverts, Enter persists).
        let anim_fields = [
            Field::AnimMaster,
            Field::AnimSeekbar,
            Field::AnimRain,
            Field::AnimDonut,
            Field::AnimVisualizer,
            Field::AnimStarfield,
            Field::AnimSnow,
            Field::AnimFireflies,
            Field::AnimAquarium,
            Field::AnimWaves,
            Field::AnimFireworks,
            Field::AnimLife,
            Field::AnimPipes,
            Field::AnimPlasma,
        ];
        for field in anim_fields {
            // `animations()` resolves Radio's `radio_master`, so the on/off subtitle matches
            // the switch the preview and the renderer actually use.
            let mut shown = self.animations();
            let on = field.anim_flag(&mut shown).is_some_and(|flag| *flag);
            let label = field.label();
            let subtitle = Some(if on {
                t!(
                    "on · preview on select",
                    "켜짐 · 선택 시 미리보기",
                    "オン · 選択でプレビュー"
                )
                .to_owned()
            } else {
                t!(
                    "off · preview on select",
                    "꺼짐 · 선택 시 미리보기",
                    "オフ · 選択でプレビュー"
                )
                .to_owned()
            });
            let search_text =
                format!("{} {}", label, PaletteCategory::Animation.search_aliases()).to_lowercase();
            entries.push(PaletteEntry {
                category: PaletteCategory::Animation,
                title: label,
                subtitle,
                shortcut: None,
                enabled: true,
                disabled_reason: None,
                priority: 8,
                search_text,
                target: PaletteTarget::Animation(field),
            });
        }

        entries
    }

    fn palette_active_context(&self) -> Option<KeyContext> {
        if self.queue_popup.open {
            return Some(KeyContext::Queue);
        }
        match self.mode {
            Mode::Player => Some(KeyContext::Player),
            Mode::Search => Some(KeyContext::SearchResults),
            Mode::Library if self.local_dedicated_mode => Some(KeyContext::LocalDeck),
            Mode::Library if self.effective_library_tab() == LibraryTab::Playlists => {
                Some(KeyContext::Playlists)
            }
            Mode::Library => Some(KeyContext::Library),
            Mode::Settings => Some(KeyContext::Settings),
            Mode::Ai => Some(KeyContext::AiInput),
            Mode::Artist => None,
        }
    }

    fn palette_action_availability(
        &self,
        ctx: KeyContext,
        action: Action,
        active_ctx: Option<KeyContext>,
    ) -> Result<(), &'static str> {
        let has_track = self.queue.current().is_some();
        match (ctx, action) {
            (_, Action::ChapterNext | Action::ChapterPrev) if self.loaded_chapters().is_empty() => {
                Err(t!(
                    "No chapters on current track",
                    "현재 트랙에 챕터가 없어요",
                    "現在の曲にチャプターがありません"
                ))
            }
            (_, Action::AbLoopClear) if !self.playback.ab_loop.is_set() => Err(t!(
                "No A–B loop active",
                "활성화된 A–B 반복이 없어요",
                "有効なA–Bループがありません"
            )),
            (_, Action::AbLoopCycle)
                if self.queue.current().is_some_and(Song::is_radio_station) =>
            {
                Err(t!(
                    "Unavailable on live radio",
                    "실시간 라디오에서는 사용할 수 없어요",
                    "ライブ配信では利用できません"
                ))
            }
            (_, Action::AbLoopCycle) if !has_track || self.prefetch.loaded_video_id.is_none() => {
                Err(t!(
                    "No track loaded",
                    "재생 중인 트랙이 없어요",
                    "再生中の曲がありません"
                ))
            }
            (_, Action::ToggleAtlas) if !self.radio_dedicated_mode => Err(t!(
                "Enter Radio mode first",
                "먼저 라디오 모드로 들어가세요",
                "先にラジオモードに入ってください"
            )),
            (_, Action::ToggleStreaming)
                if self.radio_dedicated_mode || self.local_dedicated_mode =>
            {
                Err(t!(
                    "Available in Music mode",
                    "음악 모드에서 사용 가능",
                    "音楽モードで利用可能"
                ))
            }
            (_, Action::CycleRepeat) if self.autoplay_streaming && !self.radio_dedicated_mode => {
                Err(t!(
                    "Turn off Autoplay first",
                    "먼저 자동재생을 끄세요",
                    "先に自動再生をオフにしてください"
                ))
            }
            (
                KeyContext::Player,
                Action::AddToPlaylist
                | Action::Download
                | Action::CopyLink
                | Action::CycleRating
                | Action::PlayVideo
                | Action::IdentifyNowPlaying
                | Action::SeekBack
                | Action::SeekForward,
            ) if !has_track => Err(t!(
                "No track loaded",
                "재생 중인 트랙이 없어요",
                "再生中の曲がありません"
            )),
            (KeyContext::Global, Action::WhyAi) if !has_track => Err(t!(
                "No track loaded",
                "재생 중인 트랙이 없어요",
                "再生中の曲がありません"
            )),
            (
                KeyContext::Library
                | KeyContext::Playlists
                | KeyContext::SearchResults
                | KeyContext::LocalDeck
                | KeyContext::Queue,
                _,
            ) if active_ctx != Some(ctx) => Err(t!(
                "Switch to that screen first",
                "해당 화면에서 사용할 수 있어요",
                "対象画面で利用できます"
            )),
            (
                KeyContext::SearchResults,
                Action::Enqueue | Action::Favorite | Action::Download | Action::AddToPlaylist,
            ) if self.search.results.is_empty() => Err(t!(
                "No search result selected",
                "선택된 검색 결과가 없어요",
                "選択された検索結果がありません"
            )),
            _ => Ok(()),
        }
    }
}

/// Flip one palette animation field.
///
/// Dedicated Radio mode keeps its own master (`radio_master`, inheriting `master` until
/// pinned). Effect toggles that need a master force that Radio switch on, and the
/// Anim Master row flips it, leaving the music-mode `master` flag alone.
fn toggle_palette_animation_field(anim: &mut AnimationsConfig, field: Field, radio: bool) -> bool {
    if radio && field == Field::AnimMaster {
        let on = !anim.effective(true).master;
        anim.radio_master = Some(on);
        return on;
    }
    let on = {
        let Some(flag) = field.anim_flag(anim) else {
            return false;
        };
        *flag = !*flag;
        *flag
    };
    if field != Field::AnimMaster && on {
        if radio {
            if !anim.effective(true).master {
                anim.radio_master = Some(true);
            }
        } else {
            anim.master = true;
        }
    }
    on
}

fn is_palette_action(action: Action) -> bool {
    !matches!(
        action,
        Action::OpenCommandPalette
            | Action::MoveUp
            | Action::MoveDown
            | Action::PageUp
            | Action::PageDown
            | Action::JumpTop
            | Action::JumpBottom
            | Action::SelectUp
            | Action::SelectDown
            | Action::SelectPageUp
            | Action::SelectPageDown
            | Action::SelectToTop
            | Action::SelectToBottom
            | Action::FocusPrev
            | Action::FocusNext
            | Action::DeleteChar
            | Action::DeleteWord
            | Action::MoveCursorLeft
            | Action::MoveCursorRight
            | Action::MoveCursorWordLeft
            | Action::MoveCursorWordRight
            | Action::SelectAll
            | Action::Confirm
            | Action::Back
            | Action::OpenContextMenu
            | Action::SettingsCancel
            | Action::ChangeDecrease
            | Action::ChangeIncrease
    )
}

fn is_featured_action(action: Action) -> bool {
    matches!(
        action,
        Action::SleepTimer
            | Action::OpenSearch
            | Action::OpenLibrary
            | Action::OpenQueue
            | Action::OpenSettings
            | Action::ToggleLyrics
            | Action::OpenBookmarks
            | Action::OpenDjPresets
            | Action::OpenAi
            | Action::ToggleRadioMode
            | Action::ToggleAtlas
            | Action::CycleEq
            | Action::ToggleShuffle
            | Action::CycleRepeat
            | Action::ToggleHelp
    )
}
