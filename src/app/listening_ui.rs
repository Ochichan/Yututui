use super::*;
use crate::listening::{
    BookmarkRecord, DjPreset, ListeningOperation, ListeningProjection, PassportNote, PassportVisit,
    ResumeCandidate,
};

mod actions;
mod rows;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListeningTab {
    #[default]
    Bookmarks,
    Presets,
    Passport,
}

impl ListeningTab {
    pub const ALL: [Self; 3] = [Self::Bookmarks, Self::Presets, Self::Passport];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bookmarks => t!("Bookmarks", "북마크", "ブックマーク"),
            Self::Presets => t!("DJ presets", "DJ 프리셋", "DJプリセット"),
            Self::Passport => t!("Passport", "청취 여권", "パスポート"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListeningControl {
    Tab(ListeningTab),
    Select(usize),
    Activate,
    New,
    Edit,
    Details,
    Overwrite,
    Delete,
    Restart,
    ToggleResume,
    ClearPassport,
    Confirm,
    Cancel,
    Close,
    Noop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListeningAction {
    pub revision: u64,
    pub control: ListeningControl,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ListeningRow {
    Bookmark(BookmarkRecord),
    Resume(ResumeCandidate),
    Preset(DjPreset),
    Visit {
        visit: PassportVisit,
        note: Option<PassportNote>,
    },
}

#[derive(Clone)]
pub enum ListeningEdit {
    Enable,
    New,
    Rename(ListeningRow),
    Overwrite(DjPreset),
    Delete(ListeningRow),
    ClearPassport,
    Inspect(ListeningRow),
}

#[derive(Default)]
pub struct ListeningDialog {
    pub tab: ListeningTab,
    pub revision: u64,
    pub selected: usize,
    pub rows: Vec<ListeningRow>,
    pub editing: Option<ListeningEdit>,
    pub edit_revision: u64,
    pub input: String,
    pub cursor: TextCursor,
    pub error: Option<String>,
    pub detail_scroll: u16,
    pub detail_max_scroll: std::cell::Cell<u16>,
}

impl ListeningDialog {
    pub fn text_entry(&self) -> bool {
        matches!(
            self.editing,
            Some(ListeningEdit::New | ListeningEdit::Rename(_))
        )
    }
}

impl App {
    pub(in crate::app) fn open_listening(&mut self, tab: ListeningTab) -> Vec<Cmd> {
        self.close_station_card();
        self.overlays.listening = Some(ListeningDialog {
            tab,
            ..Default::default()
        });
        self.refresh_listening_dialog();
        if !self.listening_records_enabled() {
            self.overlays
                .listening
                .as_mut()
                .expect("dialog is open")
                .editing = Some(ListeningEdit::Enable);
        }
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn refresh_listening_dialog(&mut self) {
        let Some(dialog) = self.overlays.listening.as_ref() else {
            return;
        };
        if dialog.revision == self.personal_state.ledger.revision && dialog.revision != 0 {
            return;
        }
        let result = ListeningProjection::from_ledger(&self.personal_state.ledger);
        let dialog = self
            .overlays
            .listening
            .as_mut()
            .expect("dialog remains open");
        match result {
            Ok(projection) => {
                let old = dialog.rows.get(dialog.selected).cloned();
                dialog.rows = rows::project_rows(dialog.tab, projection);
                dialog.selected = old
                    .and_then(|row| dialog.rows.iter().position(|candidate| *candidate == row))
                    .unwrap_or_else(|| dialog.selected.min(dialog.rows.len().saturating_sub(1)));
                dialog.revision = self.personal_state.ledger.revision;
            }
            Err(error) => dialog.error = Some(error.to_string()),
        }
        self.dirty = true;
    }

    pub(in crate::app) fn commit_listening_change(
        &mut self,
        change: ListeningOperation,
    ) -> Vec<Cmd> {
        if let Err(error) = crate::persist::ensure_persistence_writes_allowed() {
            self.listening_error(error.to_string());
            return Vec::new();
        }
        let result = self
            .reconcile_personal_state(&self.playlists)
            .and_then(|state| {
                crate::personal_state::append_listening(
                    &state,
                    self.personal_state.device_id.as_ref(),
                    change,
                    crate::signals::unix_now(),
                )
            });
        match result.and_then(|state| self.install_personal_state_runtime(state)) {
            Ok(()) => {
                self.set_status_info(t!(
                    "Saving listening records…",
                    "청취 기록 저장 중…",
                    "リスニング記録を保存中…"
                ));
                self.refresh_listening_dialog();
                vec![Cmd::Persist(PersistCmd::Library)]
            }
            Err(error) => {
                self.listening_error(error.to_string());
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn listening_error(&mut self, error: String) {
        if let Some(dialog) = self.overlays.listening.as_mut() {
            dialog.error = Some(error.clone());
            dialog.detail_scroll = 0;
        }
        self.set_status_error(error);
    }

    pub(in crate::app) fn listening_key(&mut self, key: KeyEvent, chord: Chord) -> Vec<Cmd> {
        let Some(dialog) = self.overlays.listening.as_mut() else {
            return Vec::new();
        };
        if key.code == KeyCode::Esc {
            if dialog.editing.take().is_some() {
                dialog.input.clear();
                dialog.error = None;
            } else if dialog.error.take().is_none() {
                self.overlays.listening = None;
            }
            self.dirty = true;
            return Vec::new();
        }
        if dialog.error.is_some() || matches!(dialog.editing, Some(ListeningEdit::Inspect(_))) {
            match self.keymap.action(KeyContext::Common, chord) {
                Some(Action::MoveUp) => {
                    dialog.detail_scroll = dialog.detail_scroll.saturating_sub(1)
                }
                Some(Action::MoveDown) => {
                    dialog.detail_scroll = dialog
                        .detail_scroll
                        .saturating_add(1)
                        .min(dialog.detail_max_scroll.get())
                }
                _ => {}
            }
            self.dirty = true;
            return Vec::new();
        }
        if dialog.text_entry() {
            if key.code == KeyCode::Enter {
                return self.listening_control(ListeningControl::Confirm);
            }
            if let Some(action) = self.keymap.action(KeyContext::Common, chord)
                && apply_text_edit_action(action, &mut dialog.cursor, &mut dialog.input).is_some()
            {
                self.dirty = true;
                return Vec::new();
            }
            if chord.is_typeable()
                && let KeyCode::Char(c) = key.code
                && dialog.input.chars().count() < 256
                && !c.is_control()
            {
                dialog.cursor.insert_char(&mut dialog.input, c);
                self.dirty = true;
            }
            return Vec::new();
        }
        if dialog.editing.is_some() {
            return if key.code == KeyCode::Enter {
                self.listening_control(ListeningControl::Confirm)
            } else {
                Vec::new()
            };
        }
        let action = self.keymap.action(KeyContext::Listening, chord);
        let control = match action {
            Some(Action::FocusNext | Action::FocusPrev) => {
                let i = ListeningTab::ALL
                    .iter()
                    .position(|tab| *tab == dialog.tab)
                    .unwrap_or(0);
                let step = if action == Some(Action::FocusPrev) {
                    2
                } else {
                    1
                };
                ListeningControl::Tab(ListeningTab::ALL[(i + step) % 3])
            }
            Some(Action::MoveUp) => ListeningControl::Select(dialog.selected.saturating_sub(1)),
            Some(Action::MoveDown) => ListeningControl::Select(
                (dialog.selected + 1).min(dialog.rows.len().saturating_sub(1)),
            ),
            Some(Action::PageUp) => ListeningControl::Select(dialog.selected.saturating_sub(8)),
            Some(Action::PageDown) => ListeningControl::Select(
                (dialog.selected + 8).min(dialog.rows.len().saturating_sub(1)),
            ),
            Some(Action::Confirm) => ListeningControl::Activate,
            Some(Action::Back) => ListeningControl::Close,
            Some(Action::ListeningAdd) => ListeningControl::New,
            Some(Action::ListeningEdit) => ListeningControl::Edit,
            Some(Action::ListeningDetails) => ListeningControl::Details,
            Some(Action::ListeningOverwrite) => ListeningControl::Overwrite,
            Some(Action::ListeningDelete) => ListeningControl::Delete,
            Some(Action::ListeningRestart) => ListeningControl::Restart,
            Some(Action::ListeningToggleResume) => ListeningControl::ToggleResume,
            Some(Action::ListeningClearPassport) => ListeningControl::ClearPassport,
            _ => ListeningControl::Noop,
        };
        self.listening_control(control)
    }

    pub(in crate::app) fn listening_mouse(&mut self, action: ListeningAction) -> Vec<Cmd> {
        let Some(dialog) = self.overlays.listening.as_ref() else {
            return Vec::new();
        };
        if action.revision != self.personal_state.ledger.revision
            || action.revision != dialog.revision
        {
            self.refresh_listening_dialog();
            return Vec::new();
        }
        self.listening_control(action.control)
    }
}
