//! Settings audio-output picker state and interaction.
//!
//! The device inventory lives on [`App`] even while Settings is closed so mpv hotplug
//! notifications are never lost. The overlay owns only ephemeral cursor/editor state; selecting
//! a row asks the playback intent path to perform the change and does not mutate config itself.

use super::*;

const MANUAL_DEVICE_ID_MAX_BYTES: usize = 512;

/// Live cross-platform audio endpoint state reported by mpv.
pub struct AudioDeviceRuntime {
    pub loading: bool,
    /// At least one complete `audio-device-list` snapshot has arrived from mpv.
    pub inventory_observed: bool,
    pub devices: Vec<crate::player::AudioDevice>,
    /// mpv's selected `audio-device`; `None` is automatic system routing.
    pub current_device: Option<String>,
    /// The active output driver (`coreaudio`, `wasapi`, `pipewire`, and so on).
    pub current_output: Option<String>,
    pub error: Option<String>,
    /// The admitted selection awaiting mpv's correlated terminal reply.
    pub pending: Option<AudioOutputPendingSelection>,
    /// A saved preference which was unavailable, after this session moved to `auto`.
    /// Kept until an explicit user choice so hotplug does not switch output mid-song.
    pub session_fallback_for: Option<String>,
    next_correlation_id: u64,
    latest_requested_id: Option<u64>,
}

impl Default for AudioDeviceRuntime {
    fn default() -> Self {
        Self {
            loading: true,
            inventory_observed: false,
            devices: Vec::new(),
            current_device: None,
            current_output: None,
            error: None,
            pending: None,
            session_fallback_for: None,
            next_correlation_id: 1,
            latest_requested_id: None,
        }
    }
}

impl AudioDeviceRuntime {
    pub fn allocate_correlation_id(&mut self) -> u64 {
        let id = self.next_correlation_id.max(1);
        self.next_correlation_id = id.wrapping_add(1).max(1);
        self.latest_requested_id = Some(id);
        id
    }

    pub fn correlation_is_latest(&self, correlation_id: u64) -> bool {
        self.latest_requested_id == Some(correlation_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputPendingSelection {
    pub correlation_id: u64,
    pub target: Option<String>,
    pub source: AudioOutputSelectionSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioOutputSelectionSource {
    SystemDefault,
    Detected,
    Manual,
    /// Internal safety fallback; never overwrites the saved preference.
    SessionFallback,
}

/// Ephemeral state for the centered Settings modal.
pub struct AudioOutputPicker {
    pub selected: usize,
    pub manual_input: String,
    pub editing_manual: bool,
    pub applying: bool,
    pub error: Option<String>,
    pub rect: Cell<Option<Rect>>,
}

impl AudioOutputPicker {
    fn new(saved: &str, devices: &[crate::player::AudioDevice]) -> Self {
        let saved = normalize_saved_device(saved);
        let selected = saved.as_deref().map_or(0, |saved| {
            devices
                .iter()
                .filter(|device| !device.name.eq_ignore_ascii_case("auto"))
                .position(|device| device.name == saved)
                .map_or_else(
                    || 1 + visible_device_count(devices),
                    |device_index| 1 + device_index,
                )
        });
        Self {
            selected,
            manual_input: saved
                .filter(|saved| !devices.iter().any(|device| device.name == *saved))
                .unwrap_or_default(),
            editing_manual: false,
            applying: false,
            error: None,
            rect: Cell::new(None),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioOutputRowKind {
    SystemDefault,
    Device(String),
    SavedUnavailable(String),
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputRow {
    pub label: String,
    pub kind: AudioOutputRowKind,
    pub selectable: bool,
}

impl App {
    /// Replace the hotplug snapshot while preserving the modal cursor by semantic device ID.
    pub(in crate::app) fn replace_audio_output_devices(
        &mut self,
        devices: Vec<crate::player::AudioDevice>,
    ) {
        let selected = self
            .overlays
            .audio_output_picker
            .as_ref()
            .and_then(|picker| self.audio_output_rows().get(picker.selected).cloned())
            .map(|row| row_identity(&row.kind));
        self.audio_devices.devices = devices;
        self.audio_devices.loading = false;
        self.audio_devices.inventory_observed = true;
        self.audio_devices.error = None;
        if let Some(selected) = selected {
            let rows = self.audio_output_rows();
            if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
                picker.selected = rows
                    .iter()
                    .position(|row| row_identity(&row.kind) == selected)
                    .unwrap_or_else(|| picker.selected.min(rows.len().saturating_sub(1)));
            }
        }
        self.dirty = true;
    }

    /// Friendly value rendered in the Settings form before the modal is opened.
    pub fn audio_output_display_label(&self, saved: &str) -> String {
        let Some(saved) = normalize_saved_device(saved) else {
            return t!("System default (recommended)", "시스템 기본값 (권장)").to_owned();
        };
        self.audio_devices
            .devices
            .iter()
            .find(|device| device.name == saved)
            .map(|device| sanitize_display_text(&device.description))
            .unwrap_or_else(|| {
                let saved = sanitize_display_text(&saved);
                if crate::i18n::is_korean() {
                    format!("찾을 수 없음 · {saved}")
                } else {
                    format!("Unavailable · {saved}")
                }
            })
    }

    /// Rows are derived from the latest hotplug inventory so a closed/reopened modal is current.
    pub fn audio_output_rows(&self) -> Vec<AudioOutputRow> {
        let saved = self
            .settings
            .as_ref()
            .and_then(|settings| normalize_saved_device(&settings.draft.audio_mpv_device));
        let mut rows = vec![AudioOutputRow {
            label: t!("System default (recommended)", "시스템 기본값 (권장)").to_owned(),
            kind: AudioOutputRowKind::SystemDefault,
            selectable: true,
        }];
        for device in self
            .audio_devices
            .devices
            .iter()
            .filter(|device| !device.name.eq_ignore_ascii_case("auto"))
        {
            let duplicate_description =
                self.audio_devices.devices.iter().any(|other| {
                    other.name != device.name && other.description == device.description
                });
            let description = sanitize_display_text(&device.description);
            let label = if duplicate_description {
                format!("{} · {}", description, short_device_id(&device.name))
            } else {
                description
            };
            rows.push(AudioOutputRow {
                label,
                kind: AudioOutputRowKind::Device(device.name.clone()),
                selectable: true,
            });
        }
        if let Some(saved) = saved
            && !self
                .audio_devices
                .devices
                .iter()
                .any(|device| device.name == saved)
        {
            let display_saved = sanitize_display_text(&saved);
            rows.push(AudioOutputRow {
                label: if crate::i18n::is_korean() {
                    format!("저장됨 · 현재 찾을 수 없음 · {display_saved}")
                } else {
                    format!("Saved · currently unavailable · {display_saved}")
                },
                kind: AudioOutputRowKind::SavedUnavailable(saved),
                selectable: false,
            });
        }
        rows.push(AudioOutputRow {
            label: t!("Enter a device ID manually…", "장치 ID 직접 입력…").to_owned(),
            kind: AudioOutputRowKind::Manual,
            selectable: true,
        });
        rows
    }

    /// Open first, then request a fresh inventory. The refresh admission path owns `loading`.
    pub(in crate::app) fn open_audio_output_picker(&mut self) -> Vec<Cmd> {
        let saved = self
            .settings
            .as_ref()
            .map(|settings| settings.draft.audio_mpv_device.as_str())
            .unwrap_or_default();
        self.overlays.audio_output_picker =
            Some(AudioOutputPicker::new(saved, &self.audio_devices.devices));
        self.dirty = true;
        self.request_audio_output_refresh()
    }

    pub(in crate::app) fn close_audio_output_picker(&mut self) {
        self.overlays.audio_output_picker = None;
        self.dirty = true;
    }

    pub(in crate::app) fn audio_output_selection_admitted(
        &mut self,
        correlation_id: u64,
        target: Option<String>,
        source: AudioOutputSelectionSource,
    ) {
        self.audio_devices.pending = Some(AudioOutputPendingSelection {
            correlation_id,
            target,
            source,
        });
        if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
            picker.applying = true;
            picker.error = None;
        }
        self.dirty = true;
    }

    pub(in crate::app) fn audio_output_selection_failed(&mut self, error: String) {
        if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
            picker.applying = false;
            picker.error = Some(error);
        }
        self.dirty = true;
    }

    pub(in crate::app) fn audio_output_selection_succeeded(&mut self) {
        self.overlays.audio_output_picker = None;
        self.dirty = true;
    }

    pub(in crate::app) fn audio_output_picker_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if matches!(self.keymap.global_action(key.into()), Some(Action::Quit)) {
            return self.quit_app();
        }
        if self
            .overlays
            .audio_output_picker
            .as_ref()
            .is_some_and(|picker| picker.editing_manual)
        {
            let delta = match key.code {
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::PageUp => Some(-(self.page_step() as i32)),
                KeyCode::PageDown => Some(self.page_step() as i32),
                _ => None,
            };
            if let Some(delta) = delta {
                self.audio_output_picker_move(delta);
                return Vec::new();
            }
            return self.audio_output_manual_key(key);
        }
        if key.code == KeyCode::Char('r') && key.modifiers.is_empty() {
            if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
                picker.error = None;
            }
            return self.request_audio_output_refresh();
        }
        let action = self
            .keymap
            .action(KeyContext::Settings, key.into())
            .or_else(|| Self::settings_safety_action(key));
        match action {
            Some(Action::MoveUp) => self.audio_output_picker_move(-1),
            Some(Action::MoveDown) => self.audio_output_picker_move(1),
            Some(Action::PageUp) => self.audio_output_picker_move(-(self.page_step() as i32)),
            Some(Action::PageDown) => self.audio_output_picker_move(self.page_step() as i32),
            Some(Action::Confirm) => return self.audio_output_picker_confirm(),
            Some(Action::Back | Action::SettingsCancel) => self.close_audio_output_picker(),
            _ => {}
        }
        Vec::new()
    }

    fn audio_output_manual_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        match key.code {
            KeyCode::Enter => {
                let value = self
                    .overlays
                    .audio_output_picker
                    .as_ref()
                    .map(|picker| picker.manual_input.trim().to_owned())
                    .unwrap_or_default();
                if value.is_empty() {
                    self.audio_output_selection_failed(
                        t!("Enter a device ID first", "먼저 장치 ID를 입력하세요").to_owned(),
                    );
                    return Vec::new();
                }
                return self.request_audio_output_selection(
                    Some(value),
                    AudioOutputSelectionSource::Manual,
                );
            }
            KeyCode::Esc => {
                if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
                    picker.editing_manual = false;
                    picker.error = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
                    picker.manual_input.pop();
                    picker.error = None;
                }
            }
            KeyCode::Char(ch) if safe_manual_char(ch) => {
                if let Some(picker) = self.overlays.audio_output_picker.as_mut()
                    && picker.manual_input.len() + ch.len_utf8() <= MANUAL_DEVICE_ID_MAX_BYTES
                {
                    picker.manual_input.push(ch);
                    picker.error = None;
                }
            }
            _ => {}
        }
        self.dirty = true;
        Vec::new()
    }

    fn audio_output_picker_move(&mut self, delta: i32) {
        let rows = self.audio_output_rows();
        let Some(picker) = self.overlays.audio_output_picker.as_mut() else {
            return;
        };
        let selected = stepped_selectable_row(&rows, picker.selected, delta);
        if selected != picker.selected {
            picker.editing_manual = false;
        }
        picker.selected = selected;
        picker.error = None;
        self.dirty = true;
    }

    pub(in crate::app) fn audio_output_picker_scroll(&mut self, up: bool, amount: usize) -> bool {
        if self.overlays.audio_output_picker.is_none() {
            return false;
        }
        self.audio_output_picker_move(if up { -(amount as i32) } else { amount as i32 });
        true
    }

    pub(in crate::app) fn audio_output_picker_click(&mut self, index: usize) -> Vec<Cmd> {
        let rows = self.audio_output_rows();
        let Some(row) = rows.get(index) else {
            return Vec::new();
        };
        if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
            if picker.selected != index {
                picker.editing_manual = false;
            }
            picker.selected = index;
            picker.error = None;
        }
        self.dirty = true;
        if !row.selectable {
            self.audio_output_selection_failed(
                t!(
                    "That saved device is not currently available",
                    "저장된 장치를 현재 찾을 수 없습니다"
                )
                .to_owned(),
            );
            Vec::new()
        } else {
            Vec::new()
        }
    }

    /// `Some` means the modal consumed the click, including clicks on non-row chrome/outside.
    pub(in crate::app) fn audio_output_picker_mouse_click(
        &mut self,
        col: u16,
        row: u16,
    ) -> Option<Vec<Cmd>> {
        let picker = self.overlays.audio_output_picker.as_ref()?;
        if !picker
            .rect
            .get()
            .is_some_and(|rect| rect_contains(rect, col, row))
        {
            self.close_audio_output_picker();
            return Some(Vec::new());
        }
        Some(match self.mouse_target_at(col, row) {
            Some(MouseTarget::AudioOutputRow(index)) => self.audio_output_picker_click(index),
            _ => Vec::new(),
        })
    }

    pub(in crate::app) fn audio_output_picker_mouse_double_click(
        &mut self,
        col: u16,
        row: u16,
    ) -> Vec<Cmd> {
        let Some(picker) = self.overlays.audio_output_picker.as_ref() else {
            return Vec::new();
        };
        if !picker
            .rect
            .get()
            .is_some_and(|rect| rect_contains(rect, col, row))
        {
            self.close_audio_output_picker();
            return Vec::new();
        }
        let Some(MouseTarget::AudioOutputRow(index)) = self.mouse_target_at(col, row) else {
            return Vec::new();
        };
        let _ = self.audio_output_picker_click(index);
        self.audio_output_picker_confirm()
    }

    fn audio_output_picker_confirm(&mut self) -> Vec<Cmd> {
        let rows = self.audio_output_rows();
        let Some((selected, applying)) = self
            .overlays
            .audio_output_picker
            .as_ref()
            .map(|picker| (picker.selected, picker.applying))
        else {
            return Vec::new();
        };
        if applying {
            return Vec::new();
        }
        match rows.get(selected).map(|row| &row.kind) {
            Some(AudioOutputRowKind::SystemDefault) => {
                self.request_audio_output_selection(None, AudioOutputSelectionSource::SystemDefault)
            }
            Some(AudioOutputRowKind::Device(name)) => self.request_audio_output_selection(
                Some(name.clone()),
                AudioOutputSelectionSource::Detected,
            ),
            Some(AudioOutputRowKind::Manual) => {
                if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
                    picker.editing_manual = true;
                    picker.error = None;
                }
                self.dirty = true;
                Vec::new()
            }
            Some(AudioOutputRowKind::SavedUnavailable(_)) => {
                self.audio_output_selection_failed(
                    t!(
                        "That saved device is not currently available",
                        "저장된 장치를 현재 찾을 수 없습니다"
                    )
                    .to_owned(),
                );
                Vec::new()
            }
            None => Vec::new(),
        }
    }
}

fn normalize_saved_device(saved: &str) -> Option<String> {
    let saved = saved.trim();
    (!saved.is_empty() && !saved.eq_ignore_ascii_case("auto")).then(|| saved.to_owned())
}

fn visible_device_count(devices: &[crate::player::AudioDevice]) -> usize {
    devices
        .iter()
        .filter(|device| !device.name.eq_ignore_ascii_case("auto"))
        .count()
}

fn short_device_id(name: &str) -> String {
    let name = sanitize_display_text(name);
    if name.chars().count() <= 24 {
        return name;
    }
    let driver = name.split_once('/').map_or("device", |(driver, _)| driver);
    let mut tail: String = name.chars().rev().take(12).collect();
    tail = tail.chars().rev().collect();
    format!("{driver}/…{tail}")
}

fn safe_manual_char(ch: char) -> bool {
    !ch.is_control()
        && !matches!(
            ch,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

fn sanitize_display_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(MANUAL_DEVICE_ID_MAX_BYTES));
    let mut separated = false;
    for ch in value.trim().chars() {
        if ch.is_whitespace() || !safe_manual_char(ch) {
            separated = !out.is_empty();
            continue;
        }
        if separated {
            out.push(' ');
            separated = false;
        }
        if out.len() + ch.len_utf8() > MANUAL_DEVICE_ID_MAX_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

fn row_identity(kind: &AudioOutputRowKind) -> (u8, Option<String>) {
    match kind {
        AudioOutputRowKind::SystemDefault => (0, None),
        AudioOutputRowKind::Device(name) | AudioOutputRowKind::SavedUnavailable(name) => {
            (1, Some(name.clone()))
        }
        AudioOutputRowKind::Manual => (2, None),
    }
}

fn stepped_selectable_row(rows: &[AudioOutputRow], current: usize, delta: i32) -> usize {
    if rows.is_empty() || delta == 0 {
        return current.min(rows.len().saturating_sub(1));
    }
    let forward = delta > 0;
    let mut next = current.min(rows.len() - 1);
    for _ in 0..delta.unsigned_abs() {
        loop {
            let candidate = if forward {
                (next + 1).min(rows.len() - 1)
            } else {
                next.saturating_sub(1)
            };
            if candidate == next || rows[candidate].selectable {
                next = candidate;
                break;
            }
            next = candidate;
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, selectable: bool) -> AudioOutputRow {
        AudioOutputRow {
            label: label.to_owned(),
            kind: AudioOutputRowKind::Manual,
            selectable,
        }
    }

    #[test]
    fn selection_skips_unavailable_saved_row() {
        let rows = [
            row("auto", true),
            row("missing", false),
            row("manual", true),
        ];
        assert_eq!(stepped_selectable_row(&rows, 0, 1), 2);
        assert_eq!(stepped_selectable_row(&rows, 2, -1), 0);
    }

    #[test]
    fn correlation_ids_start_at_one_and_never_emit_zero() {
        let mut runtime = AudioDeviceRuntime::default();
        assert_eq!(runtime.allocate_correlation_id(), 1);
        runtime.next_correlation_id = u64::MAX;
        assert_eq!(runtime.allocate_correlation_id(), u64::MAX);
        assert_eq!(runtime.allocate_correlation_id(), 1);
    }

    #[test]
    fn display_sanitizer_drops_terminal_and_bidi_controls() {
        assert_eq!(
            sanitize_display_text("  USB\u{1b}[31m\u{202e} speaker\n"),
            "USB [31m speaker"
        );
        assert!(!safe_manual_char('\u{202e}'));
        assert!(safe_manual_char('가'));
    }
}
