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
        self.route_mouse_click(col, row, multi)
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
        self.route_mouse_target(target)
    }

    /// Double-click activates a list row; other targets retain single-click behavior.
    pub(in crate::app) fn on_mouse_double_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if self.interaction.why_gem_click.take() == Some((col, row)) {
            self.dirty = true;
            return Vec::new();
        }
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
            || self.overlays.why_gem_video_id.is_some()
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
            // Artist-screen rows: double-click activates (song plays, album fetches).
            Some(MouseTarget::ArtistSongRow(i)) if self.mode == Mode::Artist => {
                self.artist_select(ArtistSection::Songs, i);
                self.artist_activate_selected()
            }
            Some(MouseTarget::ArtistAlbumRow(i)) if self.mode == Mode::Artist => {
                self.artist_select(ArtistSection::Albums, i);
                self.artist_activate_selected()
            }
            _ => self.on_mouse_click(col, row, false),
        }
    }

    /// Extend the active pointer gesture while preserving its original surface.
    pub(in crate::app) fn on_mouse_drag(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        // An outside press closes WhyGem immediately, but that press still owns the pointer
        // gesture. Keep consuming its subsequent drag events so the newly exposed queue/list
        // cannot move underneath the dismissed modal before button-up.
        if self.overlays.why_gem_video_id.is_some() || self.interaction.why_gem_click.is_some() {
            return Vec::new();
        }
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
        if self.overlays.why_gem_video_id.is_some() {
            self.cancel_seekbar_scrub();
            self.interaction.ai_transcript_drag = None;
            return Vec::new();
        }
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
        if self.overlays.why_gem_video_id.is_some() {
            return Vec::new();
        }
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
            // Artist screen: scroll the section under the pointer, falling back to the
            // focused one (mirrors the DJ Gem screen's hover-aware wheel).
            Mode::Artist => {
                let hovered = match self.mouse_target_at(col, row) {
                    Some(
                        MouseTarget::ArtistSongRow(_)
                        | MouseTarget::Scrollbar(ScrollSurface::ArtistSongs),
                    ) => Some(ArtistSection::Songs),
                    Some(
                        MouseTarget::ArtistAlbumRow(_)
                        | MouseTarget::Scrollbar(ScrollSurface::ArtistAlbums),
                    ) => Some(ArtistSection::Albums),
                    _ => None,
                };
                if let Some(st) = self.artist.as_ref() {
                    match hovered.unwrap_or(st.section) {
                        ArtistSection::Songs => st.songs_scroll.wheel(up, n, st.page.songs.len()),
                        ArtistSection::Albums => {
                            st.albums_scroll.wheel(up, n, st.page.albums.len());
                        }
                    }
                    self.dirty = true;
                }
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
