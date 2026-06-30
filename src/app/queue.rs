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
        let song = self.queue.goto(pos).cloned();
        self.queue_popup.open = false;
        self.queue_popup.cursor = self.queue.cursor_pos();
        self.queue_popup.anchor = self.queue_popup.cursor;
        self.status.text.clear();
        self.load_song(song)
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
        let len_before = self.queue.len();
        if len_before == 0 || lo > hi {
            return Vec::new();
        }
        let lo = lo.min(len_before - 1);
        let hi = hi.min(len_before - 1);
        let current_pos = self.queue.cursor_pos();
        let removed_current = lo <= current_pos && current_pos <= hi;
        let removed_count = hi - lo + 1;
        let next_pos_after_removal = if removed_current && removed_count < len_before {
            if hi + 1 < len_before {
                Some(lo)
            } else if self.queue.repeat == crate::queue::Repeat::All {
                Some(0)
            } else {
                None
            }
        } else {
            None
        };

        let mut current_changed = false;
        for pos in (lo..=hi).rev() {
            if let Some(changed) = self.queue.remove_at(pos) {
                current_changed |= changed;
            }
        }
        let len = self.queue.len();
        if len == 0 {
            self.queue_popup.open = false;
            self.queue_popup.cursor = 0;
            self.queue_popup.anchor = 0;
        } else {
            let sel = lo.min(len - 1);
            self.queue_popup.cursor = sel;
            self.queue_popup.anchor = sel;
        }
        self.dirty = true;
        if current_changed {
            if let Some(pos) = next_pos_after_removal {
                let song = self.queue.goto(pos).cloned();
                self.load_song(song)
            } else {
                let mut cmds = self.load_song(None);
                cmds.push(Cmd::Player(PlayerCmd::Stop));
                cmds
            }
        } else {
            Vec::new()
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
                self.queue_popup.cursor = self.queue_popup.cursor.saturating_sub(1);
                self.queue_popup.anchor = self.queue_popup.cursor;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let last = self.queue.len().saturating_sub(1);
                if self.queue_popup.cursor < last {
                    self.queue_popup.cursor += 1;
                }
                self.queue_popup.anchor = self.queue_popup.cursor;
                self.dirty = true;
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
