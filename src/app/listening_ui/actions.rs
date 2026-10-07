use super::*;
use crate::listening::DjPresetId;

impl App {
    pub(in crate::app) fn listening_control(&mut self, control: ListeningControl) -> Vec<Cmd> {
        let Some(dialog) = self.overlays.listening.as_mut() else {
            return Vec::new();
        };
        dialog.error = None;
        self.dirty = true;
        let selected = dialog.rows.get(dialog.selected).cloned();
        if matches!(
            control,
            ListeningControl::New
                | ListeningControl::Edit
                | ListeningControl::Overwrite
                | ListeningControl::Delete
                | ListeningControl::ClearPassport
                | ListeningControl::ToggleResume
                | ListeningControl::Restart
        ) && !self.listening_records_enabled()
        {
            self.overlays
                .listening
                .as_mut()
                .expect("dialog is open")
                .editing = Some(ListeningEdit::Enable);
            return Vec::new();
        }
        let dialog = self.overlays.listening.as_mut().expect("dialog is open");
        if matches!(
            control,
            ListeningControl::New
                | ListeningControl::Edit
                | ListeningControl::Overwrite
                | ListeningControl::Delete
                | ListeningControl::ClearPassport
        ) {
            dialog.edit_revision = dialog.revision;
        }
        match control {
            ListeningControl::Close => self.overlays.listening = None,
            ListeningControl::Cancel => {
                dialog.editing = None;
                dialog.input.clear();
            }
            ListeningControl::Noop => {}
            ListeningControl::Tab(tab) => {
                *dialog = ListeningDialog {
                    tab,
                    ..Default::default()
                };
                self.refresh_listening_dialog();
            }
            ListeningControl::Select(index) => {
                dialog.selected = index.min(dialog.rows.len().saturating_sub(1))
            }
            ListeningControl::Details => {
                if let Some(row) = selected {
                    dialog.editing = Some(ListeningEdit::Inspect(row));
                    dialog.detail_scroll = 0;
                }
            }
            ListeningControl::New if dialog.tab != ListeningTab::Passport => {
                dialog.editing = Some(ListeningEdit::New);
                dialog.input.clear();
                dialog.cursor = TextCursor::default();
            }
            ListeningControl::Edit => {
                if let Some(row) = selected {
                    let input = match &row {
                        ListeningRow::Bookmark(bookmark) => bookmark.label.clone(),
                        ListeningRow::Preset(preset) => preset.name.clone(),
                        ListeningRow::Visit { note, .. } => note
                            .as_ref()
                            .map(|note| note.note.clone())
                            .unwrap_or_default(),
                        ListeningRow::Resume(_) => return Vec::new(),
                    };
                    dialog.editing = Some(ListeningEdit::Rename(row));
                    dialog.input = input;
                    dialog.cursor = TextCursor::at_end(&dialog.input);
                }
            }
            ListeningControl::Overwrite => {
                if let Some(ListeningRow::Preset(preset)) = selected {
                    dialog.editing = Some(ListeningEdit::Overwrite(preset));
                }
            }
            ListeningControl::Delete => {
                if let Some(row) = selected {
                    dialog.editing = Some(ListeningEdit::Delete(row));
                }
            }
            ListeningControl::ClearPassport if dialog.tab == ListeningTab::Passport => {
                dialog.editing = Some(ListeningEdit::ClearPassport)
            }
            ListeningControl::Confirm => return self.commit_listening_editor(),
            ListeningControl::Activate => return self.activate_listening_row(selected),
            ListeningControl::Restart if dialog.tab == ListeningTab::Bookmarks => {
                return self.restart_listening();
            }
            ListeningControl::ToggleResume if dialog.tab == ListeningTab::Bookmarks => {
                return self.toggle_listening_resume();
            }
            _ => {}
        }
        Vec::new()
    }

    fn commit_listening_editor(&mut self) -> Vec<Cmd> {
        let Some(dialog) = self.overlays.listening.as_ref() else {
            return Vec::new();
        };
        let Some(edit) = dialog.editing.clone() else {
            return Vec::new();
        };
        if !matches!(edit, ListeningEdit::New | ListeningEdit::Enable)
            && dialog.edit_revision != self.personal_state.ledger.revision
        {
            self.listening_error(
                t!(
                    "Records changed while editing. Press Esc and open the record again.",
                    "편집 중 기록이 바뀌었어요. Esc를 누른 뒤 기록을 다시 열어주세요.",
                    "編集中に記録が変わりました。Escで戻り、記録を開き直してください。"
                )
                .to_owned(),
            );
            return Vec::new();
        }
        let input = dialog.input.trim().to_owned();
        if dialog.text_entry()
            && input.is_empty()
            && !matches!(edit, ListeningEdit::Rename(ListeningRow::Visit { .. }))
        {
            self.listening_error(
                t!(
                    "Enter a name",
                    "이름을 입력하세요",
                    "名前を入力してください"
                )
                .to_owned(),
            );
            return Vec::new();
        }
        let change = match edit {
            ListeningEdit::Enable => {
                let commands = self.enable_listening_records();
                if !commands.is_empty() {
                    self.finish_listening_editor();
                }
                return commands;
            }
            ListeningEdit::New if dialog.tab == ListeningTab::Bookmarks => {
                let commands = self.save_current_bookmark(input);
                if !commands.is_empty() {
                    self.finish_listening_editor();
                }
                return commands;
            }
            ListeningEdit::New => {
                let id = format!("preset-{:032x}", fastrand::u128(..));
                let preset_id = match DjPresetId::new(id) {
                    Ok(id) => id,
                    Err(error) => {
                        self.listening_error(error.to_string());
                        return Vec::new();
                    }
                };
                ListeningOperation::UpsertDjPreset {
                    preset: DjPreset {
                        preset_id,
                        name: input,
                        snapshot: self.streaming.taste.snapshot(),
                    },
                }
            }
            ListeningEdit::Rename(ListeningRow::Bookmark(mut bookmark)) => {
                bookmark.label = input;
                ListeningOperation::UpsertBookmark { bookmark }
            }
            ListeningEdit::Rename(ListeningRow::Preset(mut preset)) => {
                preset.name = input;
                ListeningOperation::UpsertDjPreset { preset }
            }
            ListeningEdit::Overwrite(mut preset) => {
                preset.snapshot = self.streaming.taste.snapshot();
                ListeningOperation::UpsertDjPreset { preset }
            }
            ListeningEdit::Rename(ListeningRow::Visit { visit, .. }) => {
                if input.is_empty() {
                    ListeningOperation::DeletePassportNote {
                        station_uuid: visit.station_uuid,
                    }
                } else {
                    ListeningOperation::SetPassportNote {
                        note: PassportNote {
                            station_uuid: visit.station_uuid,
                            note: input,
                        },
                    }
                }
            }
            ListeningEdit::Delete(ListeningRow::Bookmark(bookmark)) => {
                ListeningOperation::DeleteBookmark {
                    bookmark_id: bookmark.bookmark_id,
                }
            }
            ListeningEdit::Delete(ListeningRow::Preset(preset)) => {
                ListeningOperation::DeleteDjPreset {
                    preset_id: preset.preset_id,
                }
            }
            ListeningEdit::Delete(ListeningRow::Visit { visit, .. }) => {
                ListeningOperation::DeletePassportVisit {
                    station_uuid: visit.station_uuid,
                }
            }
            ListeningEdit::ClearPassport => ListeningOperation::ClearPassport,
            ListeningEdit::Delete(ListeningRow::Resume(candidate)) => {
                let track = match candidate {
                    ResumeCandidate::Position(point) => point.track,
                    ResumeCandidate::Clear(clear) => clear.track,
                };
                let commands = self.clear_listening_resume(track);
                if !commands.is_empty() {
                    self.finish_listening_editor();
                }
                return commands;
            }
            _ => return Vec::new(),
        };
        let commands = self.commit_listening_change(change);
        if !commands.is_empty() {
            self.finish_listening_editor();
        }
        commands
    }

    fn finish_listening_editor(&mut self) {
        if let Some(dialog) = self.overlays.listening.as_mut() {
            dialog.editing = None;
            dialog.input.clear();
            dialog.error = None;
        }
        self.refresh_listening_dialog();
    }

    fn activate_listening_row(&mut self, row: Option<ListeningRow>) -> Vec<Cmd> {
        match row {
            Some(ListeningRow::Bookmark(bookmark)) => {
                self.jump_listening(bookmark.track, bookmark.position_ms)
            }
            Some(ListeningRow::Resume(ResumeCandidate::Position(point))) => {
                self.jump_listening(point.track, point.position_ms)
            }
            Some(ListeningRow::Resume(ResumeCandidate::Clear(clear))) => {
                self.jump_listening(clear.track, 0)
            }
            Some(ListeningRow::Preset(preset)) => self.apply_dj_preset(preset),
            Some(ListeningRow::Visit { visit, .. }) => {
                let id = format!("rad:{}", visit.station_uuid);
                let song = self
                    .library
                    .radio_favorites
                    .iter()
                    .chain(self.library.radios.iter())
                    .find(|song| song.video_id == id)
                    .cloned();
                if let Some(song) = song {
                    self.overlays.listening = None;
                    self.play_now(song)
                } else {
                    self.listening_error(
                        t!(
                            "This station is not available locally. Find it again in Atlas.",
                            "이 방송국의 재생 정보가 없어요. Atlas에서 다시 찾아주세요.",
                            "この放送局の再生情報がありません。Atlasで再検索してください。"
                        )
                        .to_owned(),
                    );
                    Vec::new()
                }
            }
            None => Vec::new(),
        }
    }
}
