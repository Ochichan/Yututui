//! Queue-popup reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Open the queue window, selecting the currently playing track. No-op on an empty queue.
    pub(in crate::app) fn open_queue_popup(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let pos = self.queue.cursor_pos();
        self.queue_popup.open = true;
        self.queue_popup.cursor = pos;
        self.queue_popup.anchor = pos;
        self.queue_popup.scroll.reset();
        self.dirty = true;
    }

    /// Jump playback to a queue order position and close the window.
    pub(in crate::app) fn queue_popup_play(&mut self, pos: usize) -> Vec<Cmd> {
        self.move_to_queue_track(pos)
    }

    /// Move the queue-window cursor by `lines`, clamped to the queue. When `extend` is
    /// false the anchor collapses onto the cursor (plain nav); when true the anchor stays
    /// put so Shift+nav grows the inclusive selection range like a mouse drag.
    pub(in crate::app) fn move_queue_cursor(&mut self, up: bool, lines: usize, extend: bool) {
        let last = self.queue.len().saturating_sub(1);
        self.queue_popup.cursor = if up {
            self.queue_popup.cursor.saturating_sub(lines)
        } else {
            (self.queue_popup.cursor + lines).min(last)
        };
        if !extend {
            self.queue_popup.anchor = self.queue_popup.cursor;
        }
        self.dirty = true;
    }

    /// Remove the queue window's current selection (the inclusive anchor..=cursor range).
    pub(in crate::app) fn queue_popup_remove_selection(&mut self) -> Vec<Cmd> {
        let lo = self.queue_popup.cursor.min(self.queue_popup.anchor);
        let hi = self.queue_popup.cursor.max(self.queue_popup.anchor);
        self.remove_queue_range(lo, hi)
    }

    /// Remove queue order positions `lo..=hi`, high-to-low so positions stay valid as
    /// earlier ones drop. If the playing track was removed, loads the next surviving track
    /// after the removed range, wraps only under repeat-all, or stops when no next track
    /// exists. Also clamps/closes the window's selection.
    pub(in crate::app) fn remove_queue_range(&mut self, lo: usize, hi: usize) -> Vec<Cmd> {
        let Some((mutation, outcome)) = self.queue.prepare_remove_range(lo, hi) else {
            return Vec::new();
        };
        debug_assert!(outcome.removed() > 0);
        match outcome.playback() {
            crate::queue::QueueRemovalPlayback::Unchanged => {
                self.queue.commit_mutation(mutation);
                self.commit_queue_removal_ui(outcome.popup_cursor());
                self.dirty = true;
                Vec::new()
            }
            playback => {
                self.load_prepared_queue_removal(mutation, playback, outcome.popup_cursor())
            }
        }
    }

    /// Apply the queue-window projection shared by immediate and admission-gated removals.
    pub(in crate::app) fn commit_queue_removal_ui(&mut self, cursor: usize) {
        if self.queue.is_empty() {
            self.queue_popup.open = false;
            self.queue_popup.cursor = 0;
            self.queue_popup.anchor = 0;
        } else {
            let cursor = cursor.min(self.queue.len() - 1);
            self.queue_popup.cursor = cursor;
            self.queue_popup.anchor = cursor;
        }
    }

    /// Keyboard handling while the queue window is open (it captures the keyboard). Nav
    /// (up/down via `Common`), Enter jumps+plays, Delete removes the selection, q/Esc close.
    pub(in crate::app) fn on_key_queue(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if k.code == KeyCode::Esc {
            self.queue_popup.open = false;
            self.dirty = true;
            return Vec::new();
        }
        match self.keymap.action(KeyContext::Queue, k.into()) {
            Some(Action::Back) => {
                self.queue_popup.open = false;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveUp) => {
                let step = self.nav_repeat_step(Action::MoveUp);
                self.move_queue_cursor(true, step, false);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let step = self.nav_repeat_step(Action::MoveDown);
                self.move_queue_cursor(false, step, false);
                Vec::new()
            }
            Some(Action::PageUp) => {
                self.move_queue_cursor(true, self.page_step(), false);
                Vec::new()
            }
            Some(Action::PageDown) => {
                self.move_queue_cursor(false, self.page_step(), false);
                Vec::new()
            }
            Some(Action::JumpTop) => {
                self.move_queue_cursor(true, self.queue.len(), false);
                Vec::new()
            }
            Some(Action::JumpBottom) => {
                self.move_queue_cursor(false, self.queue.len(), false);
                Vec::new()
            }
            // Shift+nav grows the selection range (keyboard mirror of a mouse drag): move
            // the cursor but leave the anchor fixed.
            Some(Action::SelectUp) => {
                let step = self.nav_repeat_step(Action::SelectUp);
                self.move_queue_cursor(true, step, true);
                Vec::new()
            }
            Some(Action::SelectDown) => {
                let step = self.nav_repeat_step(Action::SelectDown);
                self.move_queue_cursor(false, step, true);
                Vec::new()
            }
            Some(Action::SelectPageUp) => {
                self.move_queue_cursor(true, self.page_step(), true);
                Vec::new()
            }
            Some(Action::SelectPageDown) => {
                self.move_queue_cursor(false, self.page_step(), true);
                Vec::new()
            }
            Some(Action::SelectToTop) => {
                self.move_queue_cursor(true, self.queue.len(), true);
                Vec::new()
            }
            Some(Action::SelectToBottom) => {
                self.move_queue_cursor(false, self.queue.len(), true);
                Vec::new()
            }
            Some(Action::Confirm) => {
                let start = self.queue_popup.cursor.min(self.queue_popup.anchor);
                self.queue_popup_play(start)
            }
            Some(Action::QueueRemove) => self.queue_popup_remove_selection(),
            _ => Vec::new(),
        }
    }
}
