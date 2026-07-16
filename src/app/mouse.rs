//! Mouse/region reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

mod local_find;
mod scrollbar;

/// The last-rendered mouse hit map: the clickable button rects views publish each frame plus
/// the seekbar's screen rect. Kept behind a small method API so the reducer and views never
/// touch the raw cells. Interior-mutable throughout because the render pass only holds `&App`
/// yet must record this frame's geometry for the next event to hit-test against (extracted
/// from [`RenderBridges`], behaviour-preserving).
#[derive(Default)]
pub struct HitMap {
    /// Screen rect of the seekbar, written by the player view each render so a mouse click can
    /// be hit-tested against it for seeking.
    seekbar_rect: Cell<Option<Rect>>,
    /// Clickable button rects written by views each render, in publish order.
    buttons: RefCell<Vec<MouseButtonRegion>>,
}

impl HitMap {
    /// Clear the whole map for a fresh frame: forget the seekbar rect and drop every registered
    /// button (both are re-published by the render pass that follows).
    pub fn clear(&self) {
        self.seekbar_rect.set(None);
        self.buttons.borrow_mut().clear();
    }

    /// Register one clickable region. Zero-size rects are dropped (they can never be hit).
    pub fn register(&self, rect: Rect, target: MouseTarget) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.buttons
            .borrow_mut()
            .push(MouseButtonRegion { rect, target });
    }

    /// The topmost region covering `(col, row)`, if any. Scans in reverse publish order so a
    /// later-drawn button (drawn on top) wins over an earlier one beneath it.
    pub fn region_at(&self, col: u16, row: u16) -> Option<MouseButtonRegion> {
        self.buttons
            .borrow()
            .iter()
            .rev()
            .find(|b| rect_contains(b.rect, col, row))
            .cloned()
    }

    /// The target of the topmost region covering `(col, row)`, if any.
    pub fn target_at(&self, col: u16, row: u16) -> Option<MouseTarget> {
        self.region_at(col, row).map(|b| b.target)
    }

    /// Screen rect of the first registered region whose target equals `target`, in publish
    /// order. Used by the status-line dropdowns to anchor under their `eq:`/`streaming:` label.
    pub fn rect_of_target(&self, target: MouseTarget) -> Option<Rect> {
        self.buttons
            .borrow()
            .iter()
            .find(|b| b.target == target)
            .map(|b| b.rect)
    }

    /// The seekbar's last-rendered screen rect, if the player view published one.
    pub fn seekbar_rect(&self) -> Option<Rect> {
        self.seekbar_rect.get()
    }

    /// Record the seekbar's screen rect for the next event to hit-test against.
    pub fn set_seekbar_rect(&self, r: Rect) {
        self.seekbar_rect.set(Some(r));
    }

    /// Test-only view of the raw registered regions, so reducer tests can assert on the exact
    /// published hit map.
    #[cfg(test)]
    pub(in crate::app) fn regions(&self) -> std::cell::Ref<'_, Vec<MouseButtonRegion>> {
        self.buttons.borrow()
    }
}

impl App {
    pub fn clear_mouse_regions(&self) {
        self.hits.clear();
    }

    pub fn register_mouse_button(&self, rect: Rect, target: MouseTarget) {
        self.hits.register(rect, target);
    }

    pub(in crate::app) fn mouse_target_at(&self, col: u16, row: u16) -> Option<MouseTarget> {
        self.hits.target_at(col, row)
    }

    pub(in crate::app) fn mouse_region_at(&self, col: u16, row: u16) -> Option<MouseButtonRegion> {
        self.hits.region_at(col, row)
    }

    fn beginner_coach_hit(&self, col: u16, row: u16) -> bool {
        matches!(
            self.mouse_target_at(col, row),
            Some(MouseTarget::Onboarding(_))
        )
    }

    /// Dispatch a hit-tested left click; `multi` only modifies Search/Library list rows.
    pub(in crate::app) fn on_mouse_click(&mut self, col: u16, row: u16, multi: bool) -> Vec<Cmd> {
        if self.tool_setup.is_some() {
            return self
                .mouse_target_at(col, row)
                .map_or_else(Vec::new, |target| self.activate_tool_setup(target));
        }
        if !self.beginner_higher_overlay_open()
            && let Some(MouseTarget::Onboarding(action)) = self.mouse_target_at(col, row)
        {
            return self.activate_onboarding(action);
        }
        // Every fresh press cancels a prior scrub whose button-up was lost.
        self.cancel_seekbar_scrub();
        self.interaction.recording_drag = None;
        self.interaction.context_menu_press = false;
        self.interaction.context_menu_click = None;
        self.interaction.color_picker_click = None;
        self.interaction.pending_double_click_selection = None;
        if let Some(cmds) = self.audio_output_picker_mouse_click(col, row) {
            return cmds;
        }
        // The context menu is a small modal: a row click executes it, while every outside
        // click closes and is consumed so it can never activate the covered surface.
        if self.overlays.context_menu.is_some() {
            self.interaction.context_menu_press = true;
            self.interaction.context_menu_click = Some((col, row));
            if let Some(MouseTarget::ContextMenuItem(index)) = self.mouse_target_at(col, row) {
                return self.activate_context_menu_item(index);
            }
            self.overlays.context_menu = None;
            self.dirty = true;
            return Vec::new();
        }
        // A click dismisses the modal conflict warning, same as a keypress.
        if self.overlays.key_conflict.take().is_some() {
            self.dirty = true;
            return Vec::new();
        }
        // Radio mode confirmations are modal: only their Confirm/Cancel buttons act; a click
        // anywhere else backs out without switching modes.
        if self.radio_mode.pending_radio_mode_confirm.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::ConfirmRadioMode | MouseTarget::CancelRadioMode)) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.radio_mode.pending_radio_mode_confirm = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // Local Player confirmations are modal and mirror the Radio confirm behavior.
        if self.local_mode.pending_confirm.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::ConfirmLocalMode | MouseTarget::CancelLocalMode)) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.cancel_local_mode_switch();
                    return Vec::new();
                }
            }
        }
        if let Some(cmds) = self.intercept_local_import_modal_mouse_click(col, row) {
            return cmds;
        }
        // The "what's playing" identify overlay is modal: only its own buttons act; a
        // click anywhere else closes it (no click-through to the player/seekbar).
        if self.overlays.now_playing_overlay.is_some() {
            match self.mouse_target_at(col, row) {
                Some(
                    t @ (MouseTarget::NowPlayingFavorite
                    | MouseTarget::NowPlayingAskAi
                    | MouseTarget::CloseNowPlaying),
                ) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.close_now_playing_overlay();
                    return Vec::new();
                }
            }
        }
        // Settings confirmations are modal: only their Confirm/Cancel buttons act; a click
        // anywhere else backs out without changing the draft.
        if self.overlays.pending_settings_confirm.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::ConfirmSettings | MouseTarget::CancelSettings)) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.overlays.pending_settings_confirm = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // The download-delete confirmation is modal: only its own Delete/Cancel buttons act;
        // a click anywhere else backs out without touching any files.
        if self.library_ui.confirm_delete.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::ConfirmDelete | MouseTarget::CancelDelete)) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.library_ui.confirm_delete = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // The bulk-download confirmation is modal like the delete confirmations: only its own
        // Download/Cancel buttons act; a click anywhere else backs out.
        if self.library_ui.confirm_download.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::ConfirmDownload | MouseTarget::CancelDownload)) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.library_ui.confirm_download = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // The playlist-delete confirmation is modal the same way.
        if self.library_ui.confirm_playlist_delete.is_some() {
            match self.mouse_target_at(col, row) {
                Some(
                    t @ (MouseTarget::ConfirmPlaylistDelete | MouseTarget::CancelPlaylistDelete),
                ) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.library_ui.confirm_playlist_delete = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // The add-to-playlist picker is modal: its rows choose, its name-entry buttons act,
        // and a click anywhere else closes it without adding.
        if self.playlist_picker.is_some() {
            match self.mouse_target_at(col, row) {
                Some(
                    t @ (MouseTarget::PlaylistPickRow(_)
                    | MouseTarget::ConfirmPickerCreate
                    | MouseTarget::CancelPickerCreate),
                ) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.playlist_picker = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // The "Import from Spotify" picker is modal like the add-to-playlist picker: its rows
        // pick, and a click anywhere else closes it without importing.
        if self.overlays.spotify_picker.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ MouseTarget::SpotifyPickRow(_)) => return self.on_mouse_target(t),
                _ => {
                    self.overlays.spotify_picker = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        // The create-playlist popup is modal: only its Create/Cancel buttons act; a click
        // anywhere else cancels it (matching the queue window's click-outside-to-close).
        if self.library_ui.create_input.is_some() {
            match self.mouse_target_at(col, row) {
                Some(
                    t @ (MouseTarget::ConfirmPlaylistCreate | MouseTarget::CancelPlaylistCreate),
                ) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.library_ui.create_input = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        if let Some(commands) = self.local_find_mouse_modal(col, row) {
            return commands;
        }
        // The search results-filter popup is modal like the queue window: a click outside
        // it closes it; inside it only its own rows / scrollbar act, so a click landing on
        // the list underneath can't leak through.
        if self.search_filter.open && self.active_search_surface() != ActiveSearchSurface::Local {
            let inside = self
                .search_filter
                .rect
                .get()
                .is_some_and(|r| rect_contains(r, col, row));
            if !inside {
                self.search_filter.close();
                self.interaction.drag_scrollbar = None;
                self.dirty = true;
                return Vec::new();
            }
            match self.mouse_region_at(col, row) {
                Some(MouseButtonRegion {
                    target: MouseTarget::Scrollbar(ScrollSurface::SearchFilter),
                    rect,
                }) => return self.on_scrollbar_press(ScrollSurface::SearchFilter, rect, row),
                Some(MouseButtonRegion {
                    target: t @ MouseTarget::SearchFilterRow(_),
                    ..
                }) => return self.on_mouse_target(t),
                _ => return Vec::new(),
            }
        }
        if self.overlays.help_visible {
            self.overlays.help_visible = false;
            self.dirty = true;
            return Vec::new();
        }
        if self.overlays.mouse_help_visible {
            self.overlays.mouse_help_visible = false;
            self.dirty = true;
            return Vec::new();
        }
        // The About card is modal: clicking its GitHub link or the update-notice "Releases"
        // link opens the browser (and keeps the card up); a click anywhere else dismisses it.
        if self.overlays.about_visible {
            if let Some(t @ (MouseTarget::AboutLink | MouseTarget::AboutUpdateLink)) =
                self.mouse_target_at(col, row)
            {
                return self.on_mouse_target(t);
            }
            self.overlays.about_visible = false;
            self.dirty = true;
            return Vec::new();
        }
        // The recordings browser is modal wherever it opens (over the player, or on top of the
        // recording-settings popup), so it's checked first: a click outside closes it, and inside
        // only its own rows act — nothing leaks to whatever is beneath.
        if self.overlays.recordings_browser.is_some() {
            let inside = self
                .overlays
                .recordings_browser
                .as_ref()
                .and_then(|b| b.rect.get())
                .is_some_and(|r| rect_contains(r, col, row));
            if !inside {
                self.overlays.recordings_browser = None;
                self.dirty = true;
                return Vec::new();
            }
            if let Some(MouseTarget::RecordingBrowseRow(i)) = self.mouse_target_at(col, row) {
                if let Some(b) = self.overlays.recordings_browser.as_mut() {
                    b.selected = i;
                }
                self.dirty = true;
            }
            return Vec::new();
        }
        // The recording-settings popup is modal like the queue window: a click outside closes it,
        // inside only its own rows act (no leak to the Settings form beneath). A row click focuses
        // the row and activates it (mode cycles, sliders nudge up, folder edits, notify toggles,
        // browse opens the list) — the mouse equivalent of moving there and pressing Enter.
        if self.overlays.recording_settings.is_some() {
            let inside = self
                .overlays
                .recording_settings
                .as_ref()
                .and_then(|p| p.rect.get())
                .is_some_and(|r| rect_contains(r, col, row));
            if !inside {
                self.overlays.recording_settings = None;
                self.dirty = true;
                return Vec::new();
            }
            match self.mouse_region_at(col, row).map(|r| (r.target, r.rect)) {
                // An arrow / the mode `< >` / (nothing else uses this): focus + signed nudge.
                Some((MouseTarget::RecordingChange { row: i, delta }, _)) => {
                    if let Some(p) = self.overlays.recording_settings.as_mut() {
                        p.row = i;
                    }
                    self.dirty = true;
                    return self.recording_settings_adjust(delta);
                }
                // The bar track: focus, arm the drag with the track rect, and seek to the click.
                Some((MouseTarget::RecordingSlider(i), rect)) => {
                    if let Some(p) = self.overlays.recording_settings.as_mut() {
                        p.row = i;
                    }
                    self.interaction.recording_drag = Some((i, rect));
                    self.dirty = true;
                    return self.recording_slider_set(i, col, rect);
                }
                // A bare row click: focus it; folder / notify / browse also activate (Enter),
                // but mode / sliders only focus (their arrows and track do the changing).
                Some((MouseTarget::RecordingRow(i), _)) => {
                    if let Some(p) = self.overlays.recording_settings.as_mut() {
                        p.row = i;
                    }
                    self.dirty = true;
                    if matches!(i, 3 | 5 | 6) {
                        return self.recording_settings_confirm();
                    }
                    return Vec::new();
                }
                _ => {}
            }
            return Vec::new();
        }
        // The queue window is modal: a click outside it closes it ("창 밖을 클릭하면 꺼지고"),
        // and inside it only its own rows / `✗` buttons act — underlying player buttons are
        // ignored so a click landing on the player beneath the popup doesn't leak through.
        if self.queue_popup.open {
            let inside = self
                .queue_popup
                .rect
                .get()
                .is_some_and(|r| rect_contains(r, col, row));
            if !inside {
                self.queue_popup.open = false;
                self.interaction.drag_selection = None;
                self.interaction.drag_scrollbar = None;
                self.dirty = true;
                return Vec::new();
            }
            match self.mouse_region_at(col, row) {
                Some(MouseButtonRegion {
                    target: MouseTarget::Scrollbar(ScrollSurface::Queue),
                    rect,
                }) => return self.on_scrollbar_press(ScrollSurface::Queue, rect, row),
                Some(MouseButtonRegion {
                    target: t @ (MouseTarget::QueueRow(_) | MouseTarget::QueueDel(_)),
                    ..
                }) => {
                    return self.on_mouse_target(t);
                }
                _ => return Vec::new(),
            }
        }
        if let Some(region) = self.mouse_region_at(col, row) {
            if let Some(commands) = self.on_any_scrollbar_press(&region.target, region.rect, row) {
                return commands;
            }
            if matches!(region.target, MouseTarget::SettingsColorSwatch(_)) {
                self.interaction.color_picker_click = Some((col, row));
            }
            // Ctrl/Cmd+click on a list row toggles it in/out of the multi-selection.
            if multi && let MouseTarget::ListRow(i) = region.target {
                match self.mode {
                    Mode::Search => return self.search_toggle_pick(i),
                    Mode::Library if !self.local_dedicated_mode => {
                        return self.library_toggle_pick(i);
                    }
                    _ => {}
                }
            }
            return self.on_mouse_target(region.target);
        }
        // A click that missed every button dismisses an open dropdown (modal-style), so the same
        // click doesn't also seek.
        if self.dropdowns.eq_open
            || self.dropdowns.streaming_open
            || self.dropdowns.search_source_open
            || self
                .settings
                .as_ref()
                .is_some_and(|st| st.spotify_import_mode_dropdown.is_some())
        {
            self.dropdowns.eq_open = false;
            self.dropdowns.streaming_open = false;
            self.dropdowns.search_source_open = false;
            self.settings_close_spotify_import_mode_dropdown();
            self.dirty = true;
            return Vec::new();
        }
        if !self.player_controls_live() {
            return Vec::new();
        }
        if let Some(area) = self.hits.seekbar_rect()
            && let Some(dur) = self.playback.duration
            && dur.is_finite()
            && dur > 0.0
            && area.width > 0
            && rect_contains(area, col, row)
        {
            self.begin_seekbar_scrub(col, area, dur);
            return Vec::new();
        }
        Vec::new()
    }

    /// Whether the player transport/status controls are on screen and may take input:
    /// always on the Player screen, and on every screen showing the docked control box.
    /// A control that isn't rendered must never take clicks — and vice versa.
    pub(in crate::app) fn player_controls_live(&self) -> bool {
        self.mode == Mode::Player
            || self.control_box_active()
            || self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Mini
    }

    pub(in crate::app) fn on_mouse_target(&mut self, target: MouseTarget) -> Vec<Cmd> {
        match target {
            target @ (MouseTarget::LyricsLine { .. }
            | MouseTarget::LyricsDelayHandle { .. }
            | MouseTarget::LyricsDelayEarlier { .. }
            | MouseTarget::LyricsDelayLater { .. }
            | MouseTarget::LyricsDelayBlock) => self.on_lyrics_mouse_target(target),
            MouseTarget::ContextMenuItem(_) => Vec::new(),
            target @ (MouseTarget::ToolSetupCopy
            | MouseTarget::ToolSetupGuide
            | MouseTarget::ToolSetupRetry
            | MouseTarget::ToolSetupLater) => self.activate_tool_setup(target),
            MouseTarget::Onboarding(action) => self.activate_onboarding(action),
            MouseTarget::Global(Action::ToggleHelp) => {
                self.overlays.help_visible = true;
                self.overlays.mouse_help_visible = false;
                self.bridges.help_scroll.reset();
                self.dirty = true;
                Vec::new()
            }
            // The ✨ at the top-left of the nav bar — same handler as the `A` shortcut.
            MouseTarget::Global(Action::ToggleAnimations) => self.toggle_animations(),
            // The ▲/▼ at the right of the footer hint — same handler as the `B` shortcut.
            MouseTarget::Global(Action::ToggleControlBox) => self.toggle_control_box(),
            MouseTarget::Global(_) => Vec::new(),
            MouseTarget::Player(action) if self.player_controls_live() => {
                self.on_player_action(action)
            }
            MouseTarget::Player(_) => Vec::new(),
            // Toggle the EQ dropdown by clicking its `eq:` label (closes the streaming one).
            MouseTarget::EqMenu if self.player_controls_live() => {
                self.dropdowns.streaming_open = false;
                self.dropdowns.search_source_open = false;
                self.dropdowns.eq_open = !self.dropdowns.eq_open;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::EqMenu => Vec::new(),
            // Pick a preset from the open dropdown.
            MouseTarget::EqSelect(preset) if self.player_controls_live() => {
                self.select_eq_preset(preset)
            }
            MouseTarget::EqSelect(_) => Vec::new(),
            // Toggle the streaming-mode dropdown by clicking its `streaming:` label (closes the EQ one).
            MouseTarget::StreamingMenu if self.player_controls_live() => {
                self.dropdowns.eq_open = false;
                self.dropdowns.search_source_open = false;
                self.dropdowns.streaming_open = !self.dropdowns.streaming_open;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::StreamingMenu => Vec::new(),
            // Pick a streaming mode from the open dropdown.
            MouseTarget::StreamingSelect(mode) if self.player_controls_live() => {
                self.select_streaming_mode(mode)
            }
            MouseTarget::StreamingSelect(_) => Vec::new(),
            MouseTarget::VolumeArea => Vec::new(),
            // Nav bar: switch screens from anywhere.
            MouseTarget::Nav(mode) => self.navigate_to(mode),
            // Search bar submit button.
            MouseTarget::SearchSubmit
                if self.mode == Mode::Search
                    && self.active_search_surface() != ActiveSearchSurface::Local =>
            {
                self.submit_search_query()
            }
            MouseTarget::SearchSubmit => Vec::new(),
            MouseTarget::SearchInput
                if self.mode == Mode::Search
                    && self.active_search_surface() != ActiveSearchSurface::Local =>
            {
                self.search.focus = SearchFocus::Input;
                self.search.select_all = false;
                self.search.input_cursor = TextCursor::at_end(&self.search.input);
                self.dropdowns.search_source_open = false;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::SearchInput => Vec::new(),
            // The `⌕ Filter` button opens the results-filter popup (a no-op with no results).
            MouseTarget::SearchFilterOpen
                if self.mode == Mode::Search
                    && self.active_search_surface() != ActiveSearchSurface::Local =>
            {
                self.open_search_filter()
            }
            MouseTarget::SearchFilterOpen => Vec::new(),
            // Single-click a filter-popup row: move the popup cursor there (double-click plays).
            MouseTarget::SearchFilterRow(i) if self.search_filter.open => {
                if i < self.search_filter.matches.len() {
                    self.search_filter.cursor = i;
                    self.dirty = true;
                }
                Vec::new()
            }
            MouseTarget::SearchFilterRow(_) => Vec::new(),
            MouseTarget::SearchSourceMenu if self.mode == Mode::Search => {
                self.dropdowns.eq_open = false;
                self.dropdowns.streaming_open = false;
                self.dropdowns.search_source_open = !self.dropdowns.search_source_open;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::SearchSourceMenu => Vec::new(),
            MouseTarget::SearchSourceSelect(source) if self.mode == Mode::Search => {
                self.select_search_source(source)
            }
            MouseTarget::SearchSourceSelect(_) => Vec::new(),
            // DJ Gem prompt box and submit button mirror the Search box interaction.
            MouseTarget::AiInput if self.mode == Mode::Ai => {
                self.ai.focus = AiFocus::Input;
                self.ai.select_all = false;
                self.ai.input_cursor = TextCursor::at_end(&self.ai.input);
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::AiInput => Vec::new(),
            MouseTarget::AiSubmit if self.mode == Mode::Ai => self.submit_ai_prompt(),
            MouseTarget::AiSubmit => Vec::new(),
            MouseTarget::AiModel if self.mode == Mode::Ai => self.cycle_ai_model_from_chat(),
            MouseTarget::AiModel => Vec::new(),
            MouseTarget::AiSuggestionRow(i) if self.mode == Mode::Ai => {
                if i < self.ai.suggestions.len() {
                    self.ai.suggestions_selected = i;
                    self.ai.focus = AiFocus::Suggestions;
                    self.dirty = true;
                }
                Vec::new()
            }
            MouseTarget::AiSuggestionRow(_) => Vec::new(),
            MouseTarget::AiTranscriptRow(i) if self.mode == Mode::Ai => {
                self.interaction.ai_transcript_drag = Some(AiTranscriptDrag {
                    anchor: i,
                    cursor: i,
                    moved: false,
                });
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::AiTranscriptRow(_) => Vec::new(),
            // Library tab header.
            MouseTarget::LibraryTab(tab) if self.mode == Mode::Library => {
                if !self.library_tab_available(tab) {
                    return Vec::new();
                }
                self.library_ui.tab = tab;
                // A tab switch resets the whole list surface — the filter and any playlist
                // drill-down included, matching the keyboard Tab/BackTab path.
                self.reset_playlist_ui_state();
                self.clear_library_filter();
                if tab == LibraryTab::Playlists {
                    self.hint_playlist_create();
                }
                self.interaction.drag_selection = None;
                self.interaction.drag_scrollbar = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::LibraryTab(_) => Vec::new(),
            MouseTarget::LocalNav(i) if self.mode == Mode::Library && self.local_dedicated_mode => {
                let Some(section) = LocalSection::ALL.get(i).copied() else {
                    return Vec::new();
                };
                self.switch_local_section(section);
                Vec::new()
            }
            MouseTarget::LocalNav(_) => Vec::new(),
            MouseTarget::LocalRow(i) if self.mode == Mode::Library && self.local_dedicated_mode => {
                self.local_row_click(i)
            }
            MouseTarget::LocalRow(_) => Vec::new(),
            target @ (MouseTarget::LocalFindRow { .. }
            | MouseTarget::LocalFindInput
            | MouseTarget::LocalFindSubmit
            | MouseTarget::LocalFindRefineOpen
            | MouseTarget::LocalFindRefineRow(_)
            | MouseTarget::LocalFindLaunchpad { .. }
            | MouseTarget::ConfirmLocalFindBulk
            | MouseTarget::CancelLocalFindBulk
            | MouseTarget::ConfirmLocalFindRebuild
            | MouseTarget::CancelLocalFindRebuild) => self.on_local_find_mouse_target(target),
            MouseTarget::LocalImportDel(session_id)
                if self.mode == Mode::Library && self.local_dedicated_mode =>
            {
                self.request_local_import_record_delete_id(session_id)
            }
            MouseTarget::LocalImportDel(_) => Vec::new(),
            MouseTarget::MouseHelp => {
                self.overlays.help_visible = false;
                self.overlays.mouse_help_visible = true;
                self.bridges.help_scroll.reset();
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::SettingsTab(i) if self.mode == Mode::Settings => {
                self.settings_select_tab(i);
                Vec::new()
            }
            MouseTarget::SettingsTab(_) => Vec::new(),
            MouseTarget::SettingsChange { row, delta } if self.mode == Mode::Settings => {
                if self.settings_close_spotify_import_mode_dropdown() {
                    return Vec::new();
                }
                self.settings_focus_row(row);
                self.settings_change(delta)
            }
            MouseTarget::SettingsChange { .. } => Vec::new(),
            MouseTarget::SettingsActivate(row) if self.mode == Mode::Settings => {
                if self.settings_close_spotify_import_mode_dropdown() {
                    return Vec::new();
                }
                self.settings_focus_row(row);
                self.settings_activate()
            }
            MouseTarget::SettingsActivate(_) => Vec::new(),
            MouseTarget::SettingsColorSwatch(row) => self.settings_color_swatch_click(row),
            MouseTarget::SettingsColorPickerSurface
            | MouseTarget::SettingsColorPickerCurrent
            | MouseTarget::SettingsColorPickerChoice(_) => Vec::new(),
            MouseTarget::SettingsSpotifyImportModeMenu if self.mode == Mode::Settings => {
                if self
                    .settings
                    .as_ref()
                    .is_some_and(|st| st.spotify_import_mode_dropdown.is_some())
                {
                    self.settings_close_spotify_import_mode_dropdown();
                } else {
                    self.settings_open_spotify_import_mode_dropdown();
                }
                Vec::new()
            }
            MouseTarget::SettingsSpotifyImportModeMenu => Vec::new(),
            MouseTarget::SettingsSpotifyImportModeSelect(mode) if self.mode == Mode::Settings => {
                self.settings_select_spotify_import_mode(mode);
                Vec::new()
            }
            MouseTarget::SettingsSpotifyImportModeSelect(_) => Vec::new(),
            // Single-click a list row: select it (double-click plays — see double-click path).
            MouseTarget::ListRow(i) => self.on_list_row_click(i),
            // Scrollbar targets are handled by the coordinate-aware click/drag paths.
            MouseTarget::Scrollbar(_) | MouseTarget::LocalFindScrollbar { .. } => Vec::new(),
            // Open the queue window from the `N/M` position label.
            MouseTarget::QueuePos if self.mode == Mode::Player => {
                self.open_queue_popup();
                Vec::new()
            }
            // The miniplayer renders the queue window itself, so the `N/M` label opens it
            // in place — even when the bar is Top or collapsed (the mini is the only UI).
            MouseTarget::QueuePos
                if self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Mini =>
            {
                self.open_queue_popup();
                Vec::new()
            }
            // From another screen (docked box) the queue window still lives on the Player
            // screen — follow the click there instead of opening an invisible popup.
            MouseTarget::QueuePos if self.control_box_active() => {
                let cmds = self.navigate_to(Mode::Player);
                self.open_queue_popup();
                cmds
            }
            MouseTarget::QueuePos => Vec::new(),
            // Single-click a queue row: select it (and anchor a drag range here).
            MouseTarget::QueueRow(i) if self.queue_popup.open => {
                self.queue_popup.cursor = i;
                self.queue_popup.anchor = i;
                self.interaction.drag_selection = Some(DragSelection {
                    surface: DragSurface::Queue,
                    anchor: i,
                });
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::QueueRow(_) => Vec::new(),
            // The `✗` button on a queue row removes just that track.
            MouseTarget::QueueDel(i) if self.queue_popup.open => self.remove_queue_range(i, i),
            MouseTarget::QueueDel(_) => Vec::new(),
            // The `✗` button on a library row removes just that track (per-tab semantics).
            MouseTarget::LibraryDel(i) if self.mode == Mode::Library => {
                self.library_delete_rows(i, i)
            }
            MouseTarget::LibraryDel(_) => Vec::new(),
            // The opened-playlist breadcrumb returns to the playlist list.
            MouseTarget::PlaylistBack if self.mode == Mode::Library => {
                self.close_open_playlist();
                Vec::new()
            }
            MouseTarget::PlaylistBack => Vec::new(),
            // The "delete playlist" confirmation buttons.
            MouseTarget::ConfirmPlaylistDelete => self.confirm_playlist_delete_apply(),
            MouseTarget::CancelPlaylistDelete => {
                self.library_ui.confirm_playlist_delete = None;
                self.dirty = true;
                Vec::new()
            }
            // The create-playlist popup buttons.
            MouseTarget::ConfirmPlaylistCreate => self.playlist_create_commit(),
            MouseTarget::CancelPlaylistCreate => {
                self.library_ui.create_input = None;
                self.dirty = true;
                Vec::new()
            }
            // Add-to-playlist picker: clicking a row chooses it (the trailing row opens
            // the inline name entry); the name-entry buttons commit or back out.
            MouseTarget::PlaylistPickRow(i) if self.playlist_picker.is_some() => {
                self.picker_choose(i)
            }
            MouseTarget::PlaylistPickRow(_) => Vec::new(),
            // First click on a Spotify picker row selects it; clicking the already-selected row
            // (or a double-click, routed here as a click) starts the import.
            MouseTarget::SpotifyPickRow(i) if self.overlays.spotify_picker.is_some() => {
                let confirm = self.overlays.spotify_picker.as_mut().is_some_and(|p| {
                    let valid = i < p.items.len();
                    let already = valid && p.selected == i;
                    if valid {
                        p.selected = i;
                    }
                    already
                });
                self.dirty = true;
                if confirm {
                    self.spotify_picker_confirm()
                } else {
                    Vec::new()
                }
            }
            MouseTarget::SpotifyPickRow(_) => Vec::new(),
            MouseTarget::ConfirmPickerCreate => self.picker_create_commit(),
            MouseTarget::CancelPickerCreate => {
                if let Some(picker) = self.playlist_picker.as_mut() {
                    picker.naming = None;
                    self.dirty = true;
                }
                Vec::new()
            }
            // The "delete downloaded files" confirmation buttons.
            MouseTarget::ConfirmDownload => self.confirm_download_apply(),
            MouseTarget::CancelDownload => {
                self.library_ui.confirm_download = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmDelete => self.confirm_delete_files_apply(),
            MouseTarget::CancelDelete => {
                self.library_ui.confirm_delete = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmSettings => {
                let Some(confirm) = self.overlays.pending_settings_confirm.take() else {
                    return Vec::new();
                };
                self.settings_apply_confirm(confirm)
            }
            MouseTarget::CancelSettings => {
                self.overlays.pending_settings_confirm = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmRadioMode => {
                let Some(confirm) = self.radio_mode.pending_radio_mode_confirm else {
                    return Vec::new();
                };
                self.apply_radio_mode_confirm(confirm)
            }
            MouseTarget::CancelRadioMode => {
                self.radio_mode.pending_radio_mode_confirm = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmLocalMode => {
                let Some(confirm) = self.local_mode.pending_confirm else {
                    return Vec::new();
                };
                self.apply_local_mode_confirm(confirm)
            }
            MouseTarget::CancelLocalMode => {
                self.cancel_local_mode_switch();
                Vec::new()
            }
            MouseTarget::ConfirmLocalOrganize => {
                let Some(confirm) = self.local_mode.pending_organize_confirm.take() else {
                    return Vec::new();
                };
                self.apply_local_import_organize_confirm(confirm)
            }
            MouseTarget::CancelLocalOrganize => {
                self.local_mode.pending_organize_confirm = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmLocalAcceptAll => {
                let Some(confirm) = self.local_mode.pending_accept_all_confirm.take() else {
                    return Vec::new();
                };
                self.apply_local_import_accept_all_confirm(confirm)
            }
            MouseTarget::CancelLocalAcceptAll => {
                self.local_mode.pending_accept_all_confirm = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmLocalImportDelete => {
                let Some(session_id) = self.local_mode.pending_import_record_delete.take() else {
                    return Vec::new();
                };
                self.apply_local_import_record_delete(session_id)
            }
            MouseTarget::CancelLocalImportDelete => {
                self.local_mode.pending_import_record_delete = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::NowPlayingFavorite => self.now_playing_favorite(),
            MouseTarget::NowPlayingAskAi => self.now_playing_ask_ai(),
            MouseTarget::CloseNowPlaying => {
                self.close_now_playing_overlay();
                Vec::new()
            }
            // Click the `yututui` brand to open the About card.
            MouseTarget::AboutTitle => {
                self.overlays.about_visible = true;
                self.dirty = true;
                Vec::new()
            }
            // Click the GitHub link inside the About card to open the repo in the browser.
            MouseTarget::AboutLink => {
                open_in_browser(crate::ui::views::about::GITHUB_URL);
                self.status.text = t!(
                    "Opening GitHub in your browser…",
                    "브라우저에서 GitHub을 여는 중…",
                    "ブラウザでGitHubを開いています…"
                )
                .to_owned();
                self.dirty = true;
                Vec::new()
            }
            // Click the "Releases" link in the About card's update notice to open the
            // latest release page (keeps the card open, like the GitHub link).
            MouseTarget::AboutUpdateLink => {
                open_in_browser(crate::update::RELEASES_URL);
                self.status.text = t!(
                    "Opening Releases in your browser…",
                    "브라우저에서 Releases를 여는 중…",
                    "ブラウザでReleasesを開いています…"
                )
                .to_owned();
                self.dirty = true;
                Vec::new()
            }
            // The recording popup/browser rows are handled by their own modal guards in
            // `on_mouse_click` (which never fall through to here); listed for exhaustiveness.
            MouseTarget::RecordingRow(_)
            | MouseTarget::RecordingChange { .. }
            | MouseTarget::RecordingSlider(_)
            | MouseTarget::RecordingBrowseRow(_) => Vec::new(),
            MouseTarget::AudioOutputRow(_) => Vec::new(),
        }
    }

    /// Double-click activates a list row; other targets retain single-click behavior.
    pub(in crate::app) fn on_mouse_double_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if self.beginner_coach_hit(col, row) {
            return Vec::new();
        }
        if self.overlays.audio_output_picker.is_some() {
            return self.audio_output_picker_mouse_double_click(col, row);
        }
        // The first press may have activated a menu command that opened a confirmation/picker.
        // Consume its paired double-click press before that new modal can see and act on it.
        if self.interaction.context_menu_click.take() == Some((col, row)) {
            self.dirty = true;
            return Vec::new();
        }
        // Modal overlays treat a double-click like a single click.
        if self.overlays.help_visible
            || self.overlays.context_menu.is_some()
            || self.overlays.mouse_help_visible
            || self.overlays.about_visible
            || self.overlays.key_conflict.is_some()
            || self.radio_mode.pending_radio_mode_confirm.is_some()
            || self.local_mode.pending_confirm.is_some()
            || self.local_mode.find.pending_bulk_confirm.is_some()
            || self.local_mode.find.pending_rebuild_confirm
            || self.local_mode.find.refine_popup.open
            || self.local_import_confirmation_open()
            || self.overlays.pending_settings_confirm.is_some()
            || self.library_ui.confirm_delete.is_some()
            || self.library_ui.confirm_playlist_delete.is_some()
            || self.library_ui.create_input.is_some()
            || self.playlist_picker.is_some()
            || self.overlays.spotify_picker.is_some()
            || self.overlays.recordings_browser.is_some()
            || self.overlays.recording_settings.is_some()
        {
            return self.on_mouse_click(col, row, false);
        }
        // Double-clicking a filter-popup row plays it (the mouse Enter), mirroring the
        // queue window's inside/outside split.
        if self.search_filter.open {
            let inside = self
                .search_filter
                .rect
                .get()
                .is_some_and(|r| rect_contains(r, col, row));
            if inside {
                if let Some(MouseTarget::SearchFilterRow(i)) = self.mouse_target_at(col, row) {
                    return self.search_filter_activate(i);
                }
                return Vec::new();
            }
            return self.on_mouse_click(col, row, false); // outside -> close, same as single click
        }
        if self.queue_popup.open {
            let inside = self
                .queue_popup
                .rect
                .get()
                .is_some_and(|r| rect_contains(r, col, row));
            if inside {
                if let Some(MouseTarget::QueueRow(i)) = self.mouse_target_at(col, row) {
                    return self.queue_popup_play(i);
                }
                return Vec::new();
            }
            return self.on_mouse_click(col, row, false); // outside -> close, same as single click
        }
        match self.mouse_target_at(col, row) {
            Some(MouseTarget::Nav(Mode::Player)) if self.mode == Mode::Player => {
                self.request_radio_mode_switch()
            }
            Some(MouseTarget::Nav(Mode::Library)) if self.mode == Mode::Library => {
                self.request_local_mode_switch()
            }
            Some(MouseTarget::AiSuggestionRow(i)) if self.mode == Mode::Ai => {
                if i < self.ai.suggestions.len() {
                    self.ai.suggestions_selected = i;
                    self.ai.focus = AiFocus::Suggestions;
                    return self.play_ai_suggestion();
                }
                Vec::new()
            }
            Some(MouseTarget::LocalRow(i)) if self.local_dedicated_mode => {
                self.local_row_activate(i)
            }
            Some(MouseTarget::LocalFindRow { index, stamp }) => {
                self.local_find_mouse_activate(index, stamp)
            }
            Some(MouseTarget::ListRow(i)) => {
                self.restore_double_click_selection(i);
                self.on_list_row_activate(i)
            }
            _ => self.on_mouse_click(col, row, false),
        }
    }

    /// Extend the active pointer gesture while preserving its original surface.
    pub(in crate::app) fn on_mouse_drag(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if self.beginner_coach_hit(col, row) {
            return Vec::new();
        }
        if self.interaction.context_menu_press {
            return Vec::new();
        }
        // Radio-recording slider scrub: a press that grabbed a bar track sets the value
        // continuously as the pointer moves (row ignored — grab and drag anywhere horizontally,
        // exactly like the seekbar). `recording_slider_set` dedupes and clamps.
        if let Some((slider_row, rect)) = self.interaction.recording_drag {
            if self.overlays.recording_settings.is_some() {
                return self.recording_slider_set(slider_row, col, rect);
            }
            // Popup closed mid-drag — stop scrubbing.
            self.interaction.recording_drag = None;
            return Vec::new();
        }
        // Seekbar movement is preview-only; mouse-up commits the final target once.
        if self.interaction.seekbar_scrub.is_some() {
            self.update_seekbar_scrub(col);
            return Vec::new();
        }
        if self.queue_popup.open {
            if let Some(drag) = self.interaction.drag_scrollbar.clone() {
                self.drag_scrollbar_to(&drag, row);
                return Vec::new();
            }
            if let Some(MouseTarget::QueueRow(i) | MouseTarget::QueueDel(i)) =
                self.mouse_target_at(col, row)
            {
                let anchor = self.drag_anchor(DragSurface::Queue, i);
                if self.queue_popup.anchor != anchor || self.queue_popup.cursor != i {
                    self.queue_popup.anchor = anchor;
                    self.queue_popup.cursor = i;
                    self.dirty = true;
                }
            }
            return Vec::new();
        }
        if let Some(drag) = self.interaction.drag_scrollbar.clone() {
            self.drag_scrollbar_to(&drag, row);
            return Vec::new();
        }
        if let Some(region) = self.mouse_region_at(col, row)
            && let Some(commands) = self.on_any_scrollbar_press(&region.target, region.rect, row)
        {
            return commands;
        }
        if self.mode == Mode::Ai
            && let Some(MouseTarget::AiTranscriptRow(i)) = self.mouse_target_at(col, row)
        {
            match self.interaction.ai_transcript_drag.as_mut() {
                Some(drag) => {
                    let moved = drag.moved || drag.cursor != i || drag.anchor != i;
                    if drag.cursor != i || drag.moved != moved {
                        drag.cursor = i;
                        drag.moved = moved;
                        self.dirty = true;
                    }
                }
                None => {
                    self.interaction.ai_transcript_drag = Some(AiTranscriptDrag {
                        anchor: i,
                        cursor: i,
                        moved: false,
                    });
                    self.dirty = true;
                }
            }
            return Vec::new();
        }
        if self.mode == Mode::Library
            && let Some(MouseTarget::ListRow(i) | MouseTarget::LibraryDel(i)) =
                self.mouse_target_at(col, row)
        {
            // The Playlists root has no multi-select (its actions are single-row), so a drag
            // just moves the cursor with the anchor pinned to it.
            let anchor = if self.playlists_root() {
                i
            } else {
                self.drag_anchor(DragSurface::Library, i)
            };
            if self.library_ui.anchor != anchor || self.library_ui.selected != i {
                self.interaction.pending_double_click_selection = None;
                self.library_ui.anchor = anchor;
                self.library_ui.selected = i;
                // A drag is a range gesture — discontiguous picks yield to the range.
                self.library_ui.picked.clear();
                self.dirty = true;
            }
        }
        // The Search results list drags identically to the Library list above.
        if self.mode == Mode::Search
            && let Some(MouseTarget::ListRow(i)) = self.mouse_target_at(col, row)
        {
            let anchor = self.drag_anchor(DragSurface::Search, i);
            if self.search.anchor != anchor || self.search.selected != i {
                self.interaction.pending_double_click_selection = None;
                self.search.anchor = anchor;
                self.search.selected = i;
                self.search.picked.clear();
                self.search.focus = SearchFocus::Results;
                self.dirty = true;
            }
        }
        Vec::new()
    }

    pub(in crate::app) fn on_mouse_left_up(&mut self) -> Vec<Cmd> {
        self.interaction.context_menu_press = false;
        self.interaction.drag_selection = None;
        self.interaction.drag_scrollbar = None;
        let seek_cmds = self.commit_seekbar_scrub();

        if let Some(drag) = self.interaction.ai_transcript_drag.take() {
            if drag.moved {
                let (start, end) = if drag.anchor <= drag.cursor {
                    (drag.anchor, drag.cursor)
                } else {
                    (drag.cursor, drag.anchor)
                };
                let text = {
                    let lines = self.bridges.ai_transcript_copy_lines.borrow();
                    let end = end.min(lines.len().saturating_sub(1));
                    if start >= lines.len() || start > end {
                        String::new()
                    } else {
                        lines[start..=end].join("\n")
                    }
                };
                if !text.trim().is_empty() {
                    copy_to_clipboard(&text);
                    self.status.kind = StatusKind::Info;
                    self.status.text = t!(
                        "✓ Chat selection copied to clipboard",
                        "✓ 선택한 채팅이 클립보드에 복사됐어요",
                        "✓ 選択したチャットをクリップボードにコピーしました"
                    )
                    .to_owned();
                    self.dirty = true;
                }
            } else {
                self.dirty = true;
            }
        }

        seek_cmds
    }

    fn drag_anchor(&mut self, surface: DragSurface, row: usize) -> usize {
        match self.interaction.drag_selection {
            Some(DragSelection {
                surface: active,
                anchor,
            }) if active == surface => anchor,
            _ => {
                self.interaction.drag_selection = Some(DragSelection {
                    surface,
                    anchor: row,
                });
                row
            }
        }
    }

    /// Scroll the topmost pointer surface; Ctrl+wheel retains its text-zoom override.
    pub(in crate::app) fn on_mouse_scroll(
        &mut self,
        up: bool,
        col: u16,
        row: u16,
        ctrl: bool,
    ) -> Vec<Cmd> {
        if self.local_find_mouse_scroll_modal(up, MOUSE_SCROLL_LINES) {
            return Vec::new();
        }
        if self.audio_output_picker_scroll(up, MOUSE_SCROLL_LINES) {
            return Vec::new();
        }
        if self.overlays.context_menu.is_some() {
            self.move_context_menu_selection(up);
            return Vec::new();
        }
        if self.beginner_coach_hit(col, row) {
            return Vec::new();
        }
        // While the wheel-zoom lock is on, Ctrl+wheel degrades to a plain wheel scroll
        // (the whole point: modifier-assisted scrolling without accidental zooming);
        // the Ctrl+-/= keys keep zooming either way.
        if ctrl && !self.config.effective_zoom_wheel_lock() {
            return self.zoom_step(up);
        }
        let n = MOUSE_SCROLL_LINES;
        // The cheat-sheet overlays scroll like any list (length is clamped at render).
        if self.overlays.help_visible || self.overlays.mouse_help_visible {
            self.bridges.help_scroll.wheel(up, n, usize::MAX);
            self.dirty = true;
            return Vec::new();
        }
        // The Spotify import picker captures the wheel to move its own selection, matching the
        // keyboard ↑/↓ (the render pass keeps the selection in view).
        if let Some(p) = self.overlays.spotify_picker.as_mut() {
            p.selected = if up {
                p.selected.saturating_sub(n)
            } else {
                (p.selected + n).min(p.items.len().saturating_sub(1))
            };
            self.dirty = true;
            return Vec::new();
        }
        // The recording overlays capture the wheel so it moves their own selection/focus instead
        // of leaking to the Settings form underneath. Browser (on top) wins over the popup.
        if self.overlays.recordings_browser.is_some() {
            let total = usize::from(self.recorder.current.is_some()) + self.recorder.history.len();
            if let Some(b) = self.overlays.recordings_browser.as_mut() {
                b.selected = if up {
                    b.selected.saturating_sub(n)
                } else {
                    (b.selected + n).min(total.saturating_sub(1))
                };
            }
            self.dirty = true;
            return Vec::new();
        }
        if self.overlays.recording_settings.is_some() {
            if let Some(p) = self.overlays.recording_settings.as_mut() {
                p.row = if up {
                    p.row.saturating_sub(n)
                } else {
                    (p.row + n).min(RECORDING_POPUP_ROWS - 1)
                };
            }
            self.dirty = true;
            return Vec::new();
        }
        if self.queue_popup.open {
            self.queue_popup.scroll.wheel(up, n, self.queue.len());
            self.dirty = true;
            return Vec::new();
        }
        if self.search_filter.open && self.active_search_surface() != ActiveSearchSurface::Local {
            let len = self.search_filter.matches.len();
            self.search_filter.scroll.wheel(up, n, len);
            self.dirty = true;
            return Vec::new();
        }
        if self.player_controls_live()
            && self.config.effective_mouse_wheel_volume()
            && matches!(
                self.mouse_target_at(col, row),
                Some(
                    MouseTarget::VolumeArea | MouseTarget::Player(Action::VolDown | Action::VolUp)
                )
            )
        {
            let action = if up { Action::VolUp } else { Action::VolDown };
            return self.on_player_action(action);
        }
        match self.mode {
            Mode::Library => {
                let len = if self.local_dedicated_mode {
                    self.local_rows_len()
                } else {
                    self.library_len()
                };
                self.bridges.library_scroll.wheel(up, n, len);
                self.dirty = true;
            }
            Mode::Search if self.active_search_surface() == ActiveSearchSurface::Local => {
                self.scroll_local_find(up, n);
            }
            Mode::Search => {
                self.bridges
                    .search_scroll
                    .wheel(up, n, self.search.results.len());
                self.dirty = true;
            }
            Mode::Ai => {
                match self.mouse_target_at(col, row) {
                    Some(
                        MouseTarget::AiSuggestionRow(_)
                        | MouseTarget::Scrollbar(ScrollSurface::AiSuggestions),
                    ) => {
                        self.bridges
                            .ai_scroll
                            .wheel(up, n, self.ai.suggestions.len());
                    }
                    _ => {
                        let len = self.bridges.ai_transcript_copy_lines.borrow().len();
                        self.bridges.ai_transcript_scroll.wheel(up, n, len);
                    }
                }
                self.dirty = true;
            }
            // Settings is an interactive form, not a browse list, so the wheel keeps walking
            // the focused field (which the render then keeps on-screen with a margin).
            Mode::Settings => {
                let delta = if up { -1 } else { 1 } * n as i32;
                self.settings_move_row(delta);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Single-click select on the active screen's list. `index` is the logical item index
    /// the view published (song index, or a Settings row index).
    pub(in crate::app) fn on_list_row_click(&mut self, index: usize) -> Vec<Cmd> {
        match self.mode {
            Mode::Search if index < self.search.results.len() => {
                self.interaction.pending_double_click_selection = {
                    let selection = self.search_selection_indices();
                    (selection.len() > 1 && selection.contains(&index)).then_some(
                        PendingDoubleClickSelection {
                            surface: DragSurface::Search,
                            row: index,
                            indices: selection,
                        },
                    )
                };
                self.search.selected = index;
                self.search.anchor = index;
                // A plain click always has immediate single-row semantics. If it turns out
                // to be the first half of a double-click, the matching activation path above
                // restores the pre-click selection from the transient snapshot.
                self.search.picked.clear();
                self.search.focus = SearchFocus::Results;
                self.interaction.drag_selection = Some(DragSelection {
                    surface: DragSurface::Search,
                    anchor: index,
                });
                self.dirty = true;
            }
            Mode::Library if self.local_dedicated_mode => return self.local_row_click(index),
            Mode::Library if index < self.library_len() => {
                self.interaction.pending_double_click_selection = {
                    let selection = self.library_selection_indices();
                    (!self.playlists_root() && selection.len() > 1 && selection.contains(&index))
                        .then_some(PendingDoubleClickSelection {
                            surface: DragSurface::Library,
                            row: index,
                            indices: selection,
                        })
                };
                self.library_ui.selected = index;
                self.library_ui.anchor = index;
                self.library_ui.picked.clear();
                self.interaction.drag_selection = Some(DragSelection {
                    surface: DragSurface::Library,
                    anchor: index,
                });
                self.dirty = true;
            }
            Mode::Settings => {
                // A whole-row click focuses the row; on an actionable Button row it also
                // activates it (the Enter equivalent) so clicking anywhere on e.g. the Spotify
                // "connect in browser" / "import" row works, not just the small value glyph.
                // Text/toggle/slider rows stay focus-only here — their value hit-rects own edit.
                let is_button = self
                    .settings
                    .as_ref()
                    .and_then(|st| st.fields().get(index).copied())
                    .is_some_and(|f| matches!(f.kind(), crate::settings::FieldKind::Button));
                if is_button {
                    self.settings_focus_row(index);
                    return self.settings_activate();
                }
                if let Some(st) = self.settings.as_mut() {
                    st.row = index;
                    st.editing_text = false;
                    self.dirty = true;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Restore a selection hidden by the first press of this exact double-click. The snapshot
    /// is one-shot and surface/row scoped, so a double-click on any other list row activates
    /// only that clicked row.
    fn restore_double_click_selection(&mut self, index: usize) {
        let surface = match self.mode {
            Mode::Search => Some(DragSurface::Search),
            Mode::Library if !self.local_dedicated_mode => Some(DragSurface::Library),
            _ => None,
        };
        let Some(snapshot) = self.interaction.pending_double_click_selection.take() else {
            return;
        };
        if surface != Some(snapshot.surface) || snapshot.row != index {
            return;
        }
        match snapshot.surface {
            DragSurface::Search => {
                self.search.selected = index;
                self.search.anchor = index;
                self.search.picked = snapshot.indices.into_iter().collect();
            }
            DragSurface::Library => {
                self.library_ui.selected = index;
                self.library_ui.anchor = index;
                self.library_ui.picked = snapshot.indices.into_iter().collect();
            }
            DragSurface::Queue => {}
        }
    }

    /// Double-click activate on the active screen's list: play the song now, keeping the queue
    /// (Search/Library) — the mouse equivalent of Enter. Settings rows have no "play", so a
    /// double-click just selects.
    pub(in crate::app) fn on_list_row_activate(&mut self, index: usize) -> Vec<Cmd> {
        match self.mode {
            // The shared activation path, so a double-clicked playlist row fetches its
            // tracks first (like Enter) instead of trying to play the row itself.
            Mode::Search if index < self.search.results.len() => {
                match self.multi_selected_search_songs() {
                    Some(songs) => self.play_now_many(songs),
                    None => self.activate_search_index(index),
                }
            }
            Mode::Library if self.local_dedicated_mode => self.local_row_activate(index),
            Mode::Library if index < self.library_len() => {
                self.library_ui.selected = index;
                // At the Playlists root a double-click opens the playlist (the row is a
                // playlist, not a song) — the mouse equivalent of Enter there too.
                if self.playlists_root() {
                    self.library_ui.anchor = index;
                    return self.open_selected_playlist();
                }
                self.play_now_many(self.selected_library_songs())
            }
            _ => self.on_list_row_click(index),
        }
    }
}
