//! Local Find pointer routing, kept out of the size-capped shared mouse reducer.

use super::*;

impl App {
    /// Pointer identity for the exact Local Find row set currently on screen. This is stricter
    /// than `local_find_action_generation`: entering a drill does not start a new query, but an
    /// old scrollbar must still not scroll the drill (or vice versa).
    pub(crate) fn local_find_pointer_stamp(&self) -> LocalFindPointerStamp {
        let view = if let Some(drill) = &self.local_mode.find.drill {
            LocalFindPointerView::Drill(drill.source.clone())
        } else if self.local_mode.find.query.trim().is_empty() {
            if self.local_mode.index.index.is_empty() {
                LocalFindPointerView::Recovery
            } else {
                LocalFindPointerView::Launchpad
            }
        } else if self
            .local_mode
            .find
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.total_hits > 0)
        {
            LocalFindPointerView::Results
        } else {
            LocalFindPointerView::Recovery
        };
        LocalFindPointerStamp {
            corpus_generation: self.local_mode.find.corpus_generation,
            result_generation: self.local_mode.find.request_id,
            view,
            rows_len: self.local_find_rows_len(),
        }
    }

    pub(in crate::app) fn local_find_pointer_stamp_is_live(
        &self,
        stamp: &LocalFindPointerStamp,
    ) -> bool {
        self.mode == Mode::Search
            && self.active_search_surface() == ActiveSearchSurface::Local
            && self.local_find_pointer_stamp() == *stamp
    }

    /// `Some` means a Local Find modal owned and consumed this click.
    pub(super) fn local_find_mouse_modal(&mut self, col: u16, row: u16) -> Option<Vec<Cmd>> {
        if self.local_mode.find.pending_rebuild_confirm {
            let commands = match self.mouse_target_at(col, row) {
                Some(
                    target @ (MouseTarget::ConfirmLocalFindRebuild
                    | MouseTarget::CancelLocalFindRebuild),
                ) => self.on_mouse_target(target),
                _ => {
                    self.local_mode.find.pending_rebuild_confirm = false;
                    self.dirty = true;
                    Vec::new()
                }
            };
            return Some(commands);
        }
        if self.local_mode.find.pending_bulk_confirm.is_some() {
            let commands = match self.mouse_target_at(col, row) {
                Some(
                    target @ (MouseTarget::ConfirmLocalFindBulk | MouseTarget::CancelLocalFindBulk),
                ) => self.on_mouse_target(target),
                _ => {
                    self.local_mode.find.pending_bulk_confirm = None;
                    self.dirty = true;
                    Vec::new()
                }
            };
            return Some(commands);
        }
        if !self.local_mode.find.refine_popup.open {
            return None;
        }
        let inside = self
            .local_mode
            .find
            .refine_popup
            .rect
            .get()
            .is_some_and(|rect| rect_contains(rect, col, row));
        if !inside {
            self.local_mode.find.refine_popup.open = false;
            self.local_mode.find.refine_popup.rect.set(None);
            self.dirty = true;
            return Some(Vec::new());
        }
        Some(match self.mouse_target_at(col, row) {
            Some(target @ MouseTarget::LocalFindRefineRow(_)) => self.on_mouse_target(target),
            _ => Vec::new(),
        })
    }

    pub(super) fn on_local_find_mouse_target(&mut self, target: MouseTarget) -> Vec<Cmd> {
        let live =
            self.mode == Mode::Search && self.active_search_surface() == ActiveSearchSurface::Local;
        match target {
            MouseTarget::LocalFindRow { index, stamp }
                if live && self.local_find_pointer_stamp_is_live(&stamp) =>
            {
                self.local_find_select(index, stamp.result_generation);
                self.local_mode.find.focus = LocalFindFocus::Results;
                Vec::new()
            }
            MouseTarget::LocalFindInput if live => {
                self.local_mode.find.focus = LocalFindFocus::Input;
                self.local_mode.find.input_cursor = TextCursor::at_end(&self.local_mode.find.query);
                self.local_mode.find.select_all = false;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::LocalFindSubmit if live => self.commit_local_find_query(),
            MouseTarget::LocalFindRefineOpen if live => self.open_local_find_refine(),
            MouseTarget::LocalFindRefineRow(row) => self.local_find_refine_click(row),
            MouseTarget::LocalFindLaunchpad { index, stamp }
                if live && self.local_find_pointer_stamp_is_live(&stamp) =>
            {
                self.activate_local_find_launchpad(index)
            }
            MouseTarget::ConfirmLocalFindBulk => self.confirm_local_find_bulk(),
            MouseTarget::CancelLocalFindBulk => {
                self.local_mode.find.pending_bulk_confirm = None;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::ConfirmLocalFindRebuild if live => self.confirm_local_find_rebuild(),
            MouseTarget::CancelLocalFindRebuild if live => {
                self.local_mode.find.pending_rebuild_confirm = false;
                self.dirty = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn local_find_mouse_activate(
        &mut self,
        index: usize,
        stamp: LocalFindPointerStamp,
    ) -> Vec<Cmd> {
        if !self.local_dedicated_mode
            || self.mode != Mode::Search
            || !self.local_find_pointer_stamp_is_live(&stamp)
        {
            return Vec::new();
        }
        self.local_find_activate_index(index, stamp.result_generation)
    }

    pub(super) fn scroll_local_find(&mut self, up: bool, rows: usize) {
        self.bridges
            .local_find_scroll
            .wheel(up, rows, self.local_find_rows_len());
        self.dirty = true;
    }

    /// Local Find popups are topmost pointer surfaces. Refine owns a wrapped help viewport; the
    /// confirmation dialogs consume the wheel without moving the covered result list.
    pub(super) fn local_find_mouse_scroll_modal(&mut self, up: bool, rows: usize) -> bool {
        if self.local_mode.find.refine_popup.open {
            self.local_mode
                .find
                .refine_popup
                .help_scroll
                .wheel(up, rows, usize::MAX);
            self.dirty = true;
            return true;
        }
        self.local_mode.find.pending_bulk_confirm.is_some()
            || self.local_mode.find.pending_rebuild_confirm
    }
}
