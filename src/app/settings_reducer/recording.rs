//! Radio-recording settings popup and saved-recordings browser reducers.

use super::super::*;

impl App {
    /// Keys while the radio-recording settings popup is open. Rows: 0 mode · 1 min · 2 max ·
    /// 3 folder · 4 past-tracks · 5 notify · 6 browse. Edits go straight into the draft.
    pub(in crate::app) fn recording_settings_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // The output-folder text field captures every key until Enter/Esc commits it.
        if self
            .overlays
            .recording_settings
            .as_ref()
            .is_some_and(|p| p.editing_dir)
        {
            return self.recording_dir_edit(k);
        }
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        if self.overlays.recording_settings.is_none() {
            return Vec::new();
        }
        self.dirty = true;
        match action {
            Some(Action::MoveUp) => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.row = p.row.saturating_sub(1);
                }
                Vec::new()
            }
            Some(Action::MoveDown) => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.row = (p.row + 1).min(RECORDING_POPUP_ROWS - 1);
                }
                Vec::new()
            }
            Some(Action::ChangeDecrease) => self.recording_settings_adjust(-1),
            Some(Action::ChangeIncrease) => self.recording_settings_adjust(1),
            Some(Action::Confirm) => self.recording_settings_confirm(),
            Some(Action::SettingsCancel | Action::Back) => {
                self.overlays.recording_settings = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Adjust the focused popup knob by one step (`dir >= 0` = increase). `pub(in crate::app)`
    /// so the mouse handler can reuse it for the `‹`/`›` arrow clicks.
    pub(in crate::app) fn recording_settings_adjust(&mut self, dir: i32) -> Vec<Cmd> {
        use crate::config::{
            RECORDING_MAX_SECONDS_MAX, RECORDING_MAX_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX,
            RECORDING_MIN_SECONDS_MIN, RECORDING_PAST_TRACKS_MAX, RECORDING_PAST_TRACKS_MIN,
        };
        let row = self
            .overlays
            .recording_settings
            .as_ref()
            .map(|p| p.row)
            .unwrap_or(0);
        let up = dir >= 0;
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        let d = &mut st.draft;
        match row {
            0 => d.recording_mode = d.recording_mode.cycled(up),
            1 => {
                let step: i64 = 5;
                let v = d.recording_min_seconds as i64 + if up { step } else { -step };
                d.recording_min_seconds = v.clamp(
                    RECORDING_MIN_SECONDS_MIN as i64,
                    RECORDING_MIN_SECONDS_MAX as i64,
                ) as u32;
                // Keep max strictly above min.
                if d.recording_max_seconds <= d.recording_min_seconds {
                    d.recording_max_seconds =
                        (d.recording_min_seconds + 60).min(RECORDING_MAX_SECONDS_MAX);
                }
            }
            2 => {
                let step: i64 = 60; // one minute
                let v = d.recording_max_seconds as i64 + if up { step } else { -step };
                let floor = (d.recording_min_seconds + 1) as i64;
                d.recording_max_seconds = v
                    .clamp(
                        RECORDING_MAX_SECONDS_MIN as i64,
                        RECORDING_MAX_SECONDS_MAX as i64,
                    )
                    .max(floor) as u32;
            }
            4 => {
                let v = d.recording_past_tracks as i64 + if up { 1 } else { -1 };
                d.recording_past_tracks = v.clamp(
                    RECORDING_PAST_TRACKS_MIN as i64,
                    RECORDING_PAST_TRACKS_MAX as i64,
                ) as usize;
            }
            5 => d.recording_notify = !d.recording_notify,
            _ => {}
        }
        self.dirty = true;
        Vec::new()
    }

    /// Map a pointer column over a numeric row's bar track to a value and apply it, snapped to
    /// the row's step and clamped to its bounds. Reuses the seekbar's fraction math but over
    /// `width - 1` divisions so both track ends are reachable (the rendered `bar` spreads its
    /// thumb across `WIDTH - 1` the same way). No-ops when the value is unchanged so a drag that
    /// stays in one cell's value doesn't spam redraws. `rect` is the track rect captured at
    /// press, so mapping keeps working after the pointer leaves the track.
    pub(in crate::app) fn recording_slider_set(
        &mut self,
        row: usize,
        col: u16,
        rect: Rect,
    ) -> Vec<Cmd> {
        use crate::config::{
            RECORDING_MAX_SECONDS_MAX, RECORDING_MAX_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX,
            RECORDING_MIN_SECONDS_MIN, RECORDING_PAST_TRACKS_MAX, RECORDING_PAST_TRACKS_MIN,
        };
        if rect.width == 0 {
            return Vec::new();
        }
        let clamped = col.clamp(rect.x, rect.right().saturating_sub(1));
        let denom = f64::from(rect.width.saturating_sub(1).max(1));
        let frac = f64::from(clamped - rect.x) / denom;
        // Fraction → value in `[min, max]`, snapped to `step`, then clamped.
        let snap = |min: i64, max: i64, step: i64| -> i64 {
            let raw = min as f64 + frac * (max - min) as f64;
            (((raw / step as f64).round() as i64) * step).clamp(min, max)
        };
        let mut changed = false;
        if let Some(st) = self.settings.as_mut() {
            let d = &mut st.draft;
            match row {
                1 => {
                    let v = snap(
                        RECORDING_MIN_SECONDS_MIN as i64,
                        RECORDING_MIN_SECONDS_MAX as i64,
                        5,
                    ) as u32;
                    if d.recording_min_seconds != v {
                        d.recording_min_seconds = v;
                        // Keep max strictly above min (same invariant as the ±step path).
                        if d.recording_max_seconds <= d.recording_min_seconds {
                            d.recording_max_seconds =
                                (d.recording_min_seconds + 60).min(RECORDING_MAX_SECONDS_MAX);
                        }
                        changed = true;
                    }
                }
                2 => {
                    let floor = i64::from(d.recording_min_seconds + 1);
                    let v = snap(
                        RECORDING_MAX_SECONDS_MIN as i64,
                        RECORDING_MAX_SECONDS_MAX as i64,
                        60,
                    )
                    .max(floor) as u32;
                    if d.recording_max_seconds != v {
                        d.recording_max_seconds = v;
                        changed = true;
                    }
                }
                4 => {
                    let v = snap(
                        RECORDING_PAST_TRACKS_MIN as i64,
                        RECORDING_PAST_TRACKS_MAX as i64,
                        1,
                    ) as usize;
                    if d.recording_past_tracks != v {
                        d.recording_past_tracks = v;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if changed {
            self.dirty = true;
        }
        Vec::new()
    }

    /// Enter/Confirm on a popup row. `pub(in crate::app)` so the mouse handler can reuse it
    /// (a row click = focus that row, then Confirm).
    pub(in crate::app) fn recording_settings_confirm(&mut self) -> Vec<Cmd> {
        let row = self
            .overlays
            .recording_settings
            .as_ref()
            .map(|p| p.row)
            .unwrap_or(0);
        match row {
            3 => {
                let end = self
                    .settings
                    .as_ref()
                    .map_or(0, |st| st.draft.recording_dir.len());
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.editing_dir = true;
                    p.dir_cursor = TextCursor::from_byte_index(end);
                }
                self.dirty = true;
                Vec::new()
            }
            6 => {
                // Open the recordings browser (the popup stays behind it).
                self.overlays.recordings_browser = Some(RecordingsBrowser::default());
                self.dirty = true;
                Vec::new()
            }
            // Mode / sliders / toggle: Enter nudges like ChangeIncrease.
            _ => self.recording_settings_adjust(1),
        }
    }

    /// Feed one key into the output-folder buffer while `editing_dir` is set.
    fn recording_dir_edit(&mut self, k: KeyEvent) -> Vec<Cmd> {
        self.dirty = true;
        if let Some(action) = self.keymap.text_edit_action(k.into()) {
            let mut cursor = self
                .overlays
                .recording_settings
                .as_ref()
                .map_or_else(TextCursor::default, |popup| popup.dir_cursor);
            if let Some(st) = self.settings.as_mut() {
                let _ = apply_text_edit_action(action, &mut cursor, &mut st.draft.recording_dir);
            }
            if let Some(popup) = self.overlays.recording_settings.as_mut() {
                popup.dir_cursor = cursor;
            }
            return Vec::new();
        }
        match k.code {
            KeyCode::Enter | KeyCode::Esc => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.editing_dir = false;
                }
            }
            KeyCode::Char(c) => {
                let mut cursor = self
                    .overlays
                    .recording_settings
                    .as_ref()
                    .map_or_else(TextCursor::default, |popup| popup.dir_cursor);
                if let Some(st) = self.settings.as_mut() {
                    cursor.insert_char(&mut st.draft.recording_dir, c);
                }
                if let Some(popup) = self.overlays.recording_settings.as_mut() {
                    popup.dir_cursor = cursor;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Keys while the recordings browser is open: ↑/↓ move, `s` save, `d` discard/cancel,
    /// Enter play/reveal, its toggle / Esc / Back close it.
    pub(in crate::app) fn recordings_browser_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = k.into();
        let close = k.code == KeyCode::Esc
            || matches!(
                self.keymap.action(KeyContext::Common, chord),
                Some(Action::Back)
            )
            || matches!(
                self.keymap.action(KeyContext::Player, chord),
                Some(Action::ToggleRecordings)
            );
        if close {
            self.overlays.recordings_browser = None;
            self.dirty = true;
            return Vec::new();
        }
        let action = self
            .keymap
            .action(KeyContext::Settings, chord)
            .or_else(|| Self::settings_safety_action(k));
        let ids = self.recordings_browser_ids();
        let selected = self
            .overlays
            .recordings_browser
            .as_ref()
            .map(|b| b.selected.min(ids.len().saturating_sub(1)))
            .unwrap_or(0);
        self.dirty = true;
        match action {
            Some(Action::MoveUp) => {
                if let Some(b) = self.overlays.recordings_browser.as_mut() {
                    b.selected = selected.saturating_sub(1);
                }
                Vec::new()
            }
            Some(Action::MoveDown) => {
                if let Some(b) = self.overlays.recordings_browser.as_mut() {
                    b.selected = (selected + 1).min(ids.len().saturating_sub(1));
                }
                Vec::new()
            }
            _ => {
                let id = ids.get(selected).copied();
                match k.code {
                    KeyCode::Char('s') => id.map(|id| self.recorder_save(id)).unwrap_or_default(),
                    KeyCode::Char('d') => {
                        id.map(|id| self.recorder_discard(id)).unwrap_or_default()
                    }
                    KeyCode::Enter => {
                        if let Some(id) = id {
                            self.recorder_reveal(id);
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
        }
    }
}
