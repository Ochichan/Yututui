//! Queue-popup reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Open the queue window, selecting the currently playing track. No-op on an empty queue.
    pub(in crate::app) fn open_queue_popup(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let pos = self.queue.cursor_pos();
        self.queue_popup_open = true;
        self.queue_popup_cursor = pos;
        self.queue_popup_anchor = pos;
        self.queue_popup_scroll.reset();
        self.dirty = true;
    }

    /// Jump playback to a queue order position and close the window.
    pub(in crate::app) fn queue_popup_play(&mut self, pos: usize) -> Vec<Cmd> {
        let song = self.queue.goto(pos).cloned();
        self.queue_popup_open = false;
        self.queue_popup_cursor = self.queue.cursor_pos();
        self.queue_popup_anchor = self.queue_popup_cursor;
        self.status.clear();
        self.load_song(song)
    }

    /// Remove the queue window's current selection (the inclusive anchor..=cursor range).
    pub(in crate::app) fn queue_popup_remove_selection(&mut self) -> Vec<Cmd> {
        let lo = self.queue_popup_cursor.min(self.queue_popup_anchor);
        let hi = self.queue_popup_cursor.max(self.queue_popup_anchor);
        self.remove_queue_range(lo, hi)
    }

    /// Remove queue order positions `lo..=hi`, high-to-low so positions stay valid as
    /// earlier ones drop. Reloads the playing track if it was among them (or stops on an
    /// emptied queue), and clamps/closes the window's selection.
    pub(in crate::app) fn remove_queue_range(&mut self, lo: usize, hi: usize) -> Vec<Cmd> {
        let mut current_changed = false;
        for pos in (lo..=hi).rev() {
            if let Some(changed) = self.queue.remove_at(pos) {
                current_changed |= changed;
            }
        }
        let len = self.queue.len();
        if len == 0 {
            self.queue_popup_open = false;
            self.queue_popup_cursor = 0;
            self.queue_popup_anchor = 0;
        } else {
            let sel = lo.min(len - 1);
            self.queue_popup_cursor = sel;
            self.queue_popup_anchor = sel;
        }
        self.dirty = true;
        if current_changed {
            let song = self.queue.current().cloned();
            self.load_song(song)
        } else {
            Vec::new()
        }
    }

    /// Keyboard handling while the queue window is open (it captures the keyboard). Nav
    /// (up/down via `Common`), Enter jumps+plays, Delete removes the selection, q/Esc close.
    pub(in crate::app) fn on_key_queue(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if k.code == KeyCode::Esc {
            self.queue_popup_open = false;
            self.dirty = true;
            return Vec::new();
        }
        match self.keymap.action(KeyContext::Queue, k.into()) {
            Some(Action::Back) => {
                self.queue_popup_open = false;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveUp) => {
                self.queue_popup_cursor = self.queue_popup_cursor.saturating_sub(1);
                self.queue_popup_anchor = self.queue_popup_cursor;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let last = self.queue.len().saturating_sub(1);
                if self.queue_popup_cursor < last {
                    self.queue_popup_cursor += 1;
                }
                self.queue_popup_anchor = self.queue_popup_cursor;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Confirm) => self.queue_popup_play(self.queue_popup_cursor),
            Some(Action::QueueRemove) => self.queue_popup_remove_selection(),
            _ => Vec::new(),
        }
    }
}
