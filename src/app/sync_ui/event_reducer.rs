use super::*;

impl SyncUiState {
    pub(crate) fn finish_activation_success(&mut self, kind: &super::super::SyncActivationKind) {
        self.auto_poll_enabled = false;
        let message = match kind {
            super::super::SyncActivationKind::Setup(prepared) => match crate::i18n::current() {
                crate::i18n::Language::Korean => {
                    format!(
                        "암호화 동기화를 설정했어요 (복구 확인값: {})",
                        short_checksum(prepared.recovery_checksum())
                    )
                }
                crate::i18n::Language::Japanese => {
                    format!(
                        "暗号化同期を設定しました（復旧チェック値: {}）",
                        short_checksum(prepared.recovery_checksum())
                    )
                }
                _ => format!(
                    "Encrypted personal sync is ready (recovery check: {}).",
                    short_checksum(prepared.recovery_checksum())
                ),
            },
            super::super::SyncActivationKind::PairJoin(prepared) => {
                let summary = prepared.summary();
                match crate::i18n::current() {
                    crate::i18n::Language::Korean => {
                        format!(
                            "기기를 연결했어요: {}개 변경 병합",
                            summary.operations_added
                        )
                    }
                    crate::i18n::Language::Japanese => {
                        format!(
                            "デバイスを接続しました: {}件を統合",
                            summary.operations_added
                        )
                    }
                    _ => format!(
                        "Device connected: {} changes merged.",
                        summary.operations_added
                    ),
                }
            }
            super::super::SyncActivationKind::PairApprove { prepared, .. } => {
                match crate::i18n::current() {
                    crate::i18n::Language::Korean => {
                        format!("{} 기기를 승인했어요", prepared.target_device_name())
                    }
                    crate::i18n::Language::Japanese => {
                        format!("{} を承認しました", prepared.target_device_name())
                    }
                    _ => format!("Approved {}.", prepared.target_device_name()),
                }
            }
        };
        self.wizard = Some(SyncWizard::Result {
            success: true,
            message,
        });
    }
}

impl super::super::App {
    pub(in crate::app) fn reduce_sync_ui_event(
        &mut self,
        event: SyncUiEvent,
    ) -> Vec<super::super::Cmd> {
        let flow_id = sync_event_flow_id(&event);
        if !self.personal_state.sync_ui.is_current(flow_id) {
            return Vec::new();
        }
        match event {
            SyncUiEvent::Refreshed {
                request_id, result, ..
            } => {
                if self.personal_state.sync_ui.refresh_in_flight != Some(request_id) {
                    return Vec::new();
                }
                self.personal_state.sync_ui.refresh_in_flight = None;
                if self.personal_state.sync_ui.busy == Some(SyncBusy::Refresh) {
                    self.personal_state.sync_ui.busy = None;
                }
                if request_id != self.personal_state.sync_ui.refresh_request_id {
                    self.dirty = true;
                    return Vec::new();
                }
                match *result {
                    Ok(overview) => self.personal_state.sync_ui.set_overview(overview),
                    Err(error) => {
                        self.set_status_error(localized_sync_error(error));
                    }
                }
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::RecoveryExported { result, .. } => {
                self.personal_state.sync_ui.busy = None;
                self.queue_sync_ui_refresh();
                self.personal_state.sync_ui.wizard = Some(match *result {
                    Ok(result) => SyncWizard::Result {
                        success: true,
                        message: match crate::i18n::current() {
                            crate::i18n::Language::Korean => {
                                format!(
                                    "복구 키트를 확인하고 저장했어요 (확인값: {})",
                                    short_checksum(&result.checksum)
                                )
                            }
                            crate::i18n::Language::Japanese => {
                                format!(
                                    "復旧キットを確認して保存しました（チェック値: {}）",
                                    short_checksum(&result.checksum)
                                )
                            }
                            _ => format!(
                                "Recovery kit verified and saved (check: {}).",
                                short_checksum(&result.checksum)
                            ),
                        },
                    },
                    Err(error) => SyncWizard::Result {
                        success: false,
                        message: localized_sync_error(error),
                    },
                });
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::SetupPrepared { result, .. } => match *result {
                Ok(prepared) => {
                    self.personal_state.sync_ui.busy = Some(SyncBusy::Setup);
                    self.start_sync_activation(
                        flow_id,
                        super::super::SyncActivationKind::Setup(prepared),
                    )
                }
                Err(error) => {
                    self.sync_worker_failed(error);
                    Vec::new()
                }
            },
            SyncUiEvent::HostCreated { result, .. } => {
                self.personal_state.sync_ui.busy = None;
                match *result {
                    Ok(host) => {
                        let code = Zeroizing::new(host.code().to_owned());
                        let expires_at_unix = host.expires_at_unix();
                        self.personal_state.sync_ui.last_poll_unix =
                            Some(crate::signals::unix_now());
                        self.personal_state.sync_ui.auto_poll_enabled = true;
                        self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                            code,
                            expires_at_unix,
                            host: Some(Box::new(host)),
                            review: None,
                        });
                    }
                    Err(error) => self.sync_worker_failed(error),
                }
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::HostPolled { host, result, .. } => {
                self.personal_state.sync_ui.busy = None;
                let (code, expires_at_unix) = take_host_display(&mut self.personal_state.sync_ui)
                    .unwrap_or_else(|| {
                        (
                            Zeroizing::new(host.code().to_owned()),
                            host.expires_at_unix(),
                        )
                    });
                match *result {
                    Ok(review) => {
                        self.personal_state.sync_ui.last_poll_unix =
                            Some(crate::signals::unix_now());
                        self.personal_state.sync_ui.auto_poll_enabled = review.is_none();
                        self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                            code,
                            expires_at_unix,
                            host: Some(host),
                            review: review.map(Box::new),
                        });
                    }
                    Err(error) => {
                        self.personal_state.sync_ui.auto_poll_enabled = false;
                        if host_error_can_retry(error) {
                            self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                                code,
                                expires_at_unix,
                                host: Some(host),
                                review: None,
                            });
                            self.sync_worker_error_status(error);
                        } else {
                            self.personal_state.sync_ui.wizard = Some(SyncWizard::Result {
                                success: false,
                                message: localized_sync_error(error),
                            });
                            self.queue_sync_ui_refresh();
                            self.set_status_error(localized_sync_error(error));
                        }
                    }
                }
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::HostApprovalPrepared {
                host,
                observed_state,
                result,
                ..
            } => match *result {
                Ok(prepared) => {
                    self.personal_state.sync_ui.busy = Some(SyncBusy::PairHostApprove);
                    self.start_sync_activation(
                        flow_id,
                        super::super::SyncActivationKind::PairApprove {
                            prepared,
                            network_observed_state: *observed_state,
                        },
                    )
                }
                Err(error) => {
                    let (code, expires_at_unix) =
                        take_host_display(&mut self.personal_state.sync_ui).unwrap_or_else(|| {
                            (
                                Zeroizing::new(host.code().to_owned()),
                                host.expires_at_unix(),
                            )
                        });
                    self.personal_state.sync_ui.auto_poll_enabled = false;
                    if host_error_can_retry(error) {
                        self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                            code,
                            expires_at_unix,
                            host: Some(host),
                            review: None,
                        });
                        self.sync_worker_error_status(error);
                    } else {
                        self.personal_state.sync_ui.wizard = Some(SyncWizard::Result {
                            success: false,
                            message: localized_sync_error(error),
                        });
                        self.queue_sync_ui_refresh();
                        self.set_status_error(localized_sync_error(error));
                    }
                    Vec::new()
                }
            },
            SyncUiEvent::HostCancelled { host, result, .. } => {
                self.personal_state.sync_ui.busy = None;
                self.personal_state.sync_ui.auto_poll_enabled = false;
                self.personal_state.sync_ui.wizard = Some(match result {
                    Ok(()) => {
                        self.queue_sync_ui_refresh();
                        SyncWizard::Result {
                            success: true,
                            message: crate::t!(
                                "Device connection cancelled.",
                                "기기 연결을 취소했어요.",
                                "デバイス接続をキャンセルしました。"
                            )
                            .to_owned(),
                        }
                    }
                    Err(error) => {
                        self.set_status_error(localized_sync_error(error));
                        SyncWizard::Host {
                            code: Zeroizing::new(host.code().to_owned()),
                            expires_at_unix: host.expires_at_unix(),
                            host: Some(host),
                            review: None,
                        }
                    }
                });
                Vec::new()
            }
            SyncUiEvent::JoinStarted { result, .. } => {
                self.personal_state.sync_ui.busy = None;
                match *result {
                    Ok(waiting) => {
                        self.personal_state.sync_ui.last_poll_unix = None;
                        self.personal_state.sync_ui.auto_poll_enabled = true;
                        self.personal_state.sync_ui.wizard =
                            Some(SyncWizard::JoinWaiting(Box::new(waiting)));
                        self.start_join_poll(flow_id)
                    }
                    Err(error) => {
                        self.sync_worker_failed(error);
                        Vec::new()
                    }
                }
            }
            SyncUiEvent::JoinPolled { result, .. } => {
                self.personal_state.sync_ui.busy = None;
                match *result {
                    Ok(Some(preview)) => {
                        self.personal_state.sync_ui.auto_poll_enabled = false;
                        self.personal_state.sync_ui.wizard =
                            Some(SyncWizard::JoinPreview(Box::new(preview)));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.personal_state.sync_ui.auto_poll_enabled = false;
                        self.sync_worker_failed(error);
                    }
                }
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::JoinResumed { result, .. } => {
                self.personal_state.sync_ui.busy = None;
                match *result {
                    Ok(preview) => {
                        self.personal_state.sync_ui.auto_poll_enabled = false;
                        self.personal_state.sync_ui.wizard =
                            Some(SyncWizard::JoinPreview(Box::new(preview)));
                    }
                    Err(error) => self.sync_worker_failed(error),
                }
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::JoinDiscarded { result, .. } => {
                self.personal_state.sync_ui.busy = None;
                self.personal_state.sync_ui.auto_poll_enabled = false;
                self.queue_sync_ui_refresh();
                self.personal_state.sync_ui.wizard = Some(match result {
                    Ok(()) => SyncWizard::Result {
                        success: true,
                        message: crate::t!(
                            "The unfinished connection was discarded. Local listening data was not changed.",
                            "완료되지 않은 연결을 버렸습니다. 이 기기의 감상 데이터는 변경되지 않았습니다.",
                            "未完了の接続を破棄しました。ローカルの再生データは変更されていません。"
                        )
                        .to_owned(),
                    },
                    Err(error) => SyncWizard::Result {
                        success: false,
                        message: localized_discard_error(error),
                    },
                });
                self.dirty = true;
                Vec::new()
            }
            SyncUiEvent::JoinActivationPrepared { result, .. } => match *result {
                Ok(prepared) => {
                    self.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinApply);
                    self.start_sync_activation(
                        flow_id,
                        super::super::SyncActivationKind::PairJoin(prepared),
                    )
                }
                Err(error) => {
                    self.sync_worker_failed(error);
                    Vec::new()
                }
            },
        }
    }

    fn sync_worker_failed(&mut self, error: crate::sync::service::SyncServiceError) {
        self.personal_state.sync_ui.busy = None;
        self.personal_state.sync_ui.auto_poll_enabled = false;
        self.queue_sync_ui_refresh();
        self.personal_state.sync_ui.wizard = Some(SyncWizard::Result {
            success: false,
            message: localized_sync_error(error),
        });
        self.set_status_error(localized_sync_error(error));
        self.dirty = true;
    }

    fn sync_worker_error_status(&mut self, error: crate::sync::service::SyncServiceError) {
        self.personal_state.sync_ui.busy = None;
        self.queue_sync_ui_refresh();
        self.set_status_error(localized_sync_error(error));
        self.dirty = true;
    }

    pub(in crate::app::sync_ui) fn start_join_poll(
        &mut self,
        flow_id: u64,
    ) -> Vec<super::super::Cmd> {
        self.personal_state.sync_ui.auto_poll_enabled = true;
        self.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinPoll);
        self.personal_state.sync_ui.last_poll_unix = Some(crate::signals::unix_now());
        vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
            SyncUiCommand::JoinPoll {
                flow_id,
                state: Box::new(self.personal_state.ledger.clone()),
            },
        ))]
    }

    pub(in crate::app) fn poll_sync_ui_if_due(&mut self) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.busy.is_some()
            || !self.personal_state.sync_ui.auto_poll_enabled
        {
            return Vec::new();
        }
        let now = crate::signals::unix_now();
        if self
            .personal_state
            .sync_ui
            .last_poll_unix
            .is_some_and(|last| now.saturating_sub(last) < 2)
        {
            return Vec::new();
        }
        match self.personal_state.sync_ui.wizard.take() {
            Some(SyncWizard::JoinWaiting(waiting)) => {
                self.personal_state.sync_ui.wizard = Some(SyncWizard::JoinWaiting(waiting));
                self.start_join_poll(self.personal_state.sync_ui.flow_id)
            }
            Some(SyncWizard::Host {
                code,
                expires_at_unix,
                host: Some(host),
                review: None,
            }) => {
                self.personal_state.sync_ui.busy = Some(SyncBusy::PairHostPoll);
                self.personal_state.sync_ui.last_poll_unix = Some(now);
                self.personal_state.sync_ui.wizard = Some(SyncWizard::Host {
                    code,
                    expires_at_unix,
                    host: None,
                    review: None,
                });
                vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                    SyncUiCommand::HostPoll {
                        flow_id: self.personal_state.sync_ui.flow_id,
                        state: Box::new(self.personal_state.ledger.clone()),
                        host,
                    },
                ))]
            }
            other => {
                self.personal_state.sync_ui.wizard = other;
                Vec::new()
            }
        }
    }
}
fn sync_event_flow_id(event: &SyncUiEvent) -> u64 {
    match event {
        SyncUiEvent::Refreshed { flow_id, .. }
        | SyncUiEvent::RecoveryExported { flow_id, .. }
        | SyncUiEvent::SetupPrepared { flow_id, .. }
        | SyncUiEvent::HostCreated { flow_id, .. }
        | SyncUiEvent::HostPolled { flow_id, .. }
        | SyncUiEvent::HostApprovalPrepared { flow_id, .. }
        | SyncUiEvent::HostCancelled { flow_id, .. }
        | SyncUiEvent::JoinStarted { flow_id, .. }
        | SyncUiEvent::JoinPolled { flow_id, .. }
        | SyncUiEvent::JoinResumed { flow_id, .. }
        | SyncUiEvent::JoinDiscarded { flow_id, .. }
        | SyncUiEvent::JoinActivationPrepared { flow_id, .. } => *flow_id,
    }
}

fn short_checksum(checksum: &str) -> String {
    checksum.chars().take(12).collect()
}

fn take_host_display(state: &mut SyncUiState) -> Option<(Zeroizing<String>, i64)> {
    let SyncWizard::Host {
        code,
        expires_at_unix,
        ..
    } = state.wizard.take()?
    else {
        return None;
    };
    Some((code, expires_at_unix))
}

fn host_error_can_retry(error: crate::sync::service::SyncServiceError) -> bool {
    use crate::sync::service::SyncServiceError as E;
    matches!(
        error,
        E::PendingApproval
            | E::Authentication
            | E::Certificate
            | E::Offline
            | E::LocalStateChanged
            | E::Storage
    )
}

fn localized_discard_error(error: crate::sync::service::SyncServiceError) -> String {
    use crate::sync::service::SyncServiceError as E;
    match error {
        E::AlreadyConfigured | E::LocalStateChanged => crate::t!(
            "The connection changed and was kept. Continue it, or remove the device from an existing device if it was already approved.",
            "연결 상태가 바뀌어 그대로 유지했습니다. 연결을 이어가거나, 이미 승인된 경우 기존 기기에서 해당 기기를 제거하세요.",
            "接続状態が変わったため保持しました。接続を再開するか、承認済みの場合は既存のデバイスからこのデバイスを削除してください。"
        )
        .to_owned(),
        _ => localized_sync_error(error),
    }
}
