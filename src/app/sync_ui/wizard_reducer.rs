use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

impl super::super::App {
    pub(in crate::app) fn on_key_sync_wizard(&mut self, key: KeyEvent) -> Vec<super::super::Cmd> {
        let is_typing_character = matches!(
            self.personal_state.sync_ui.wizard.as_ref(),
            Some(SyncWizard::Setup { confirm: false, .. })
                | Some(SyncWizard::Join { confirm: false, .. })
                | Some(SyncWizard::Recovery(SyncRecoveryForm {
                    confirm: false,
                    ..
                }))
        ) && matches!(key.code, KeyCode::Char(_))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
        if !is_typing_character
            && matches!(
                self.keymap.global_action(key.into()),
                Some(crate::keymap::Action::Quit)
            )
        {
            return self.quit_app();
        }
        let Some(wizard) = self.personal_state.sync_ui.wizard.take() else {
            return Vec::new();
        };
        self.dirty = true;
        match wizard {
            SyncWizard::Setup { form, confirm } => {
                self.key_connection_form(key, form, false, confirm)
            }
            SyncWizard::Join { form, confirm } => {
                self.key_connection_form(key, form, true, confirm)
            }
            SyncWizard::Host {
                code,
                expires_at_unix,
                host,
                review,
            } => self.key_host_wizard(key, code, expires_at_unix, host, review),
            SyncWizard::JoinWaiting(waiting) => self.key_join_waiting(key, waiting),
            SyncWizard::JoinPreview(preview) => self.key_join_preview(key, preview),
            SyncWizard::DiscardJoinConfirm => self.key_discard_join_confirm(key),
            SyncWizard::Revoke {
                device_id,
                device_name,
            } => self.key_revoke(key, device_id, device_name),
            SyncWizard::Recovery(form) => self.key_recovery(key, form),
            SyncWizard::Result { success, message } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    self.personal_state.sync_ui.wizard = None;
                } else {
                    self.personal_state.sync_ui.wizard =
                        Some(SyncWizard::Result { success, message });
                }
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn on_sync_wizard_mouse_target(
        &mut self,
        target: super::super::MouseTarget,
    ) -> Vec<super::super::Cmd> {
        match target {
            super::super::MouseTarget::SyncWizardPrimary => {
                self.on_key_sync_wizard(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            }
            super::super::MouseTarget::SyncWizardSecondary => {
                self.on_key_sync_wizard(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            }
            super::super::MouseTarget::SyncWizardReveal => {
                self.on_key_sync_wizard(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            }
            super::super::MouseTarget::SyncWizardField(index) => {
                match self.personal_state.sync_ui.wizard.as_mut() {
                    Some(SyncWizard::Setup {
                        form,
                        confirm: false,
                    }) if index < form.fields(false).len() => {
                        form.select_field_at(false, index);
                    }
                    Some(SyncWizard::Join {
                        form,
                        confirm: false,
                    }) if index < form.fields(true).len() => {
                        form.select_field_at(true, index);
                    }
                    Some(SyncWizard::Recovery(form)) if !form.confirm && index < 2 => {
                        form.field = index;
                        let value = if index == 0 {
                            &form.source
                        } else {
                            &form.destination
                        };
                        form.cursor = crate::util::text_edit::TextCursor::at_end(value);
                    }
                    _ => {}
                }
                self.dirty = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn key_connection_form(
        &mut self,
        key: KeyEvent,
        mut form: SyncConnectionForm,
        join: bool,
        mut confirm: bool,
    ) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.busy.is_some() {
            self.personal_state.sync_ui.wizard = Some(if join {
                SyncWizard::Join { form, confirm }
            } else {
                SyncWizard::Setup { form, confirm }
            });
            return Vec::new();
        }
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            form.toggle_current_secret(join);
        } else if key.code == KeyCode::Esc {
            if confirm {
                confirm = false;
            } else {
                return Vec::new();
            }
        } else if key.code == KeyCode::Enter {
            if !confirm {
                match form.validate(join) {
                    Ok(()) => {
                        form.hide_secrets();
                        confirm = true;
                    }
                    Err(error) => self.set_status_error(localized_form_error(error)),
                }
            } else {
                return self.submit_connection_form(form, join);
            }
        } else if !confirm
            && matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab
            )
        {
            let delta = if matches!(key.code, KeyCode::Up | KeyCode::BackTab)
                || key.modifiers.contains(KeyModifiers::SHIFT)
            {
                -1
            } else {
                1
            };
            form.select_field(join, delta);
        } else if !confirm {
            edit_connection_form(self, &mut form, join, key);
        }
        self.personal_state.sync_ui.wizard = Some(if join {
            SyncWizard::Join { form, confirm }
        } else {
            SyncWizard::Setup { form, confirm }
        });
        Vec::new()
    }

    fn submit_connection_form(
        &mut self,
        form: SyncConnectionForm,
        join: bool,
    ) -> Vec<super::super::Cmd> {
        if self.personal_state.sync.in_progress {
            self.personal_state.sync_ui.wizard = Some(if join {
                SyncWizard::Join {
                    form,
                    confirm: true,
                }
            } else {
                SyncWizard::Setup {
                    form,
                    confirm: true,
                }
            });
            return Vec::new();
        }
        let input = match form.into_input(join) {
            Ok(input) => input,
            Err(error) => {
                self.set_status_error(localized_form_error(error));
                return Vec::new();
            }
        };
        let flow_id = self.personal_state.sync_ui.flow_id.max(1);
        self.personal_state.sync_ui.flow_id = flow_id;
        if join {
            self.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinStart);
            vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                SyncUiCommand::JoinStart { flow_id, input },
            ))]
        } else {
            self.personal_state.sync_ui.busy = Some(SyncBusy::Setup);
            vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                SyncUiCommand::SetupPrepare {
                    flow_id,
                    state: Box::new(self.personal_state.ledger.clone()),
                    playlist_revision: self.playlists.revision(),
                    input,
                },
            ))]
        }
    }

    fn key_host_wizard(
        &mut self,
        key: KeyEvent,
        code: Zeroizing<String>,
        expires_at_unix: i64,
        host: Option<Box<PairingHostInvite>>,
        review: Option<Box<PairingReview>>,
    ) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.busy.is_some() {
            self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                code,
                expires_at_unix,
                host,
                review,
            });
            return Vec::new();
        }
        if key.code == KeyCode::Enter && review.is_some() && self.personal_state.sync.in_progress {
            self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                code,
                expires_at_unix,
                host,
                review,
            });
            return Vec::new();
        }
        let flow_id = self.personal_state.sync_ui.flow_id;
        match key.code {
            KeyCode::Esc => {
                let Some(host) = host else {
                    return Vec::new();
                };
                self.personal_state.sync_ui.busy = Some(SyncBusy::PairHostCancel);
                vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                    SyncUiCommand::HostCancel {
                        flow_id,
                        state: Box::new(self.personal_state.ledger.clone()),
                        host,
                    },
                ))]
            }
            KeyCode::Enter => {
                let Some(host) = host else {
                    return Vec::new();
                };
                if let Some(review) = review {
                    self.personal_state.sync_ui.busy = Some(SyncBusy::PairHostApprove);
                    vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                        SyncUiCommand::HostApprove {
                            flow_id,
                            state: Box::new(self.personal_state.ledger.clone()),
                            host,
                            review,
                        },
                    ))]
                } else {
                    self.personal_state.sync_ui.auto_poll_enabled = true;
                    self.personal_state.sync_ui.busy = Some(SyncBusy::PairHostPoll);
                    self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                        code,
                        expires_at_unix,
                        host: None,
                        review: None,
                    });
                    vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                        SyncUiCommand::HostPoll {
                            flow_id,
                            state: Box::new(self.personal_state.ledger.clone()),
                            host,
                        },
                    ))]
                }
            }
            _ => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                    code,
                    expires_at_unix,
                    host,
                    review,
                });
                Vec::new()
            }
        }
    }

    fn key_join_waiting(
        &mut self,
        key: KeyEvent,
        waiting: Box<PairingJoinWaiting>,
    ) -> Vec<super::super::Cmd> {
        let flow_id = self.personal_state.sync_ui.flow_id;
        if self.personal_state.sync_ui.busy.is_some() {
            self.personal_state.sync_ui.wizard = Some(SyncWizard::JoinWaiting(waiting));
            return Vec::new();
        }
        match key.code {
            KeyCode::Enter => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::JoinWaiting(waiting));
                self.start_join_poll(flow_id)
            }
            KeyCode::Esc => {
                self.personal_state.sync_ui.auto_poll_enabled = false;
                self.status.text = crate::t!(
                    "Device approval is still waiting. Return to Sync to continue later.",
                    "기기 승인은 계속 대기 중이에요. 나중에 Sync에서 이어갈 수 있습니다.",
                    "デバイス承認は待機中です。後で Sync から再開できます。"
                )
                .to_owned();
                self.status.kind = super::super::StatusKind::Info;
                self.queue_sync_ui_refresh();
                Vec::new()
            }
            _ => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::JoinWaiting(waiting));
                Vec::new()
            }
        }
    }

    fn key_join_preview(
        &mut self,
        key: KeyEvent,
        preview: Box<PairingJoinPreview>,
    ) -> Vec<super::super::Cmd> {
        match key.code {
            KeyCode::Enter if self.personal_state.sync_ui.busy.is_none() => {
                self.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinApply);
                vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                    SyncUiCommand::JoinPrepareActivation {
                        flow_id: self.personal_state.sync_ui.flow_id,
                        state: Box::new(self.personal_state.ledger.clone()),
                        preview,
                    },
                ))]
            }
            KeyCode::Esc => {
                self.personal_state.sync_ui.auto_poll_enabled = false;
                self.status.text = crate::t!(
                    "The approved merge is saved for later.",
                    "승인된 병합을 나중에 이어갈 수 있도록 저장했어요.",
                    "承認済みの統合を後で再開できるよう保存しました。"
                )
                .to_owned();
                self.status.kind = super::super::StatusKind::Info;
                self.queue_sync_ui_refresh();
                Vec::new()
            }
            _ => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::JoinPreview(preview));
                Vec::new()
            }
        }
    }

    fn key_revoke(
        &mut self,
        key: KeyEvent,
        device_id: String,
        device_name: String,
    ) -> Vec<super::super::Cmd> {
        match key.code {
            KeyCode::Enter if self.personal_state.sync_ui.busy.is_none() => {
                let target = match crate::personal_state::DeviceId::new(device_id) {
                    Ok(target) => target,
                    Err(_) => {
                        self.set_status_error(localized_sync_error(
                            crate::sync::service::SyncServiceError::InvalidRemoteData,
                        ));
                        return Vec::new();
                    }
                };
                let flow_id = self.personal_state.sync_ui.flow_id.max(1);
                self.personal_state.sync_ui.busy = Some(SyncBusy::Revoke);
                self.start_personal_sync_for_tui(
                    super::super::PersonalSyncAction::Revoke(target),
                    flow_id,
                )
            }
            KeyCode::Esc => Vec::new(),
            _ => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::Revoke {
                    device_id,
                    device_name,
                });
                Vec::new()
            }
        }
    }

    fn key_discard_join_confirm(&mut self, key: KeyEvent) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.busy.is_some() {
            self.personal_state.sync_ui.wizard = Some(SyncWizard::DiscardJoinConfirm);
            return Vec::new();
        }
        match key.code {
            KeyCode::Enter => {
                self.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinDiscard);
                vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                    SyncUiCommand::DiscardJoin {
                        flow_id: self.personal_state.sync_ui.flow_id,
                    },
                ))]
            }
            KeyCode::Esc => Vec::new(),
            _ => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::DiscardJoinConfirm);
                Vec::new()
            }
        }
    }

    fn key_recovery(
        &mut self,
        key: KeyEvent,
        mut form: SyncRecoveryForm,
    ) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.busy.is_some() {
            self.personal_state.sync_ui.wizard = Some(SyncWizard::Recovery(form));
            return Vec::new();
        }
        match key.code {
            KeyCode::Esc if form.confirm => form.confirm = false,
            KeyCode::Esc => return Vec::new(),
            KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab if !form.confirm => {
                form.field = usize::from(form.field == 0);
                let value = if form.field == 0 {
                    &form.source
                } else {
                    &form.destination
                };
                form.cursor = crate::util::text_edit::TextCursor::at_end(value);
            }
            KeyCode::Enter if !form.confirm => {
                if form.source.trim().is_empty() || form.destination.trim().is_empty() {
                    self.set_status_error(crate::t!(
                        "Choose the recovery kit and destination folder.",
                        "복구 키트와 저장할 폴더를 선택해 주세요.",
                        "復旧キットと保存先フォルダーを選んでください。"
                    ));
                } else {
                    form.confirm = true;
                }
            }
            KeyCode::Enter => {
                let flow_id = self.personal_state.sync_ui.flow_id;
                let source = std::path::PathBuf::from(std::mem::take(&mut form.source));
                let destination = std::path::PathBuf::from(std::mem::take(&mut form.destination));
                self.personal_state.sync_ui.busy = Some(SyncBusy::RecoveryExport);
                return vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                    SyncUiCommand::RecoveryExport {
                        flow_id,
                        state: Box::new(self.personal_state.ledger.clone()),
                        source,
                        destination,
                    },
                ))];
            }
            _ if !form.confirm => edit_recovery_form(self, &mut form, key),
            _ => {}
        }
        self.personal_state.sync_ui.wizard = Some(SyncWizard::Recovery(form));
        Vec::new()
    }
}

fn edit_connection_form(
    app: &super::super::App,
    form: &mut SyncConnectionForm,
    join: bool,
    key: KeyEvent,
) {
    let field = form.current_field(join);
    let mut cursor = form.cursor;
    let value = form.current_value_mut(join);
    if let Some(action) = app.keymap.text_edit_action(key.into()) {
        let _ = super::super::apply_text_edit_action(action, &mut cursor, value);
    } else if let KeyCode::Char(character) = key.code
        && value.chars().count() < SyncConnectionForm::max_chars(field)
    {
        cursor.insert_char(value, character);
    }
    form.cursor = cursor;
}

fn edit_recovery_form(app: &super::super::App, form: &mut SyncRecoveryForm, key: KeyEvent) {
    let mut cursor = form.cursor;
    let value = if form.field == 0 {
        &mut form.source
    } else {
        &mut form.destination
    };
    if let Some(action) = app.keymap.text_edit_action(key.into()) {
        let _ = super::super::apply_text_edit_action(action, &mut cursor, value);
    } else if let KeyCode::Char(character) = key.code
        && value.chars().count() < 4_096
    {
        cursor.insert_char(value, character);
    }
    form.cursor = cursor;
}

fn localized_form_error(error: FormError) -> &'static str {
    match error {
        FormError::EndpointRequired => crate::t!(
            "Enter the WebDAV address.",
            "WebDAV 주소를 입력해 주세요.",
            "WebDAVアドレスを入力してください。"
        ),
        FormError::SecretRequired => crate::t!(
            "Enter the password or token.",
            "비밀번호 또는 토큰을 입력해 주세요.",
            "パスワードまたはトークンを入力してください。"
        ),
        FormError::InvalidDeviceName => crate::t!(
            "Enter a short device name.",
            "짧은 기기 이름을 입력해 주세요.",
            "短いデバイス名を入力してください。"
        ),
        FormError::PairingCodeRequired => crate::t!(
            "Enter the one-time connection code.",
            "일회용 연결 코드를 입력해 주세요.",
            "一回限りの接続コードを入力してください。"
        ),
        FormError::RecoveryFileRequired => crate::t!(
            "Choose where to save the recovery kit.",
            "복구 키트를 저장할 위치를 선택해 주세요.",
            "復旧キットの保存先を選んでください。"
        ),
        FormError::CustomCa => crate::t!(
            "The CA file could not be read safely.",
            "CA 파일을 안전하게 읽을 수 없어요.",
            "CAファイルを安全に読み込めませんでした。"
        ),
    }
}
