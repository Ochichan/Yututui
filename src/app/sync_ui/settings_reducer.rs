use super::*;
use crate::settings::sync::{
    SyncAuditRow, SyncDeviceRow, SyncDisplayPage, SyncNotice, SyncRow, SyncRowAction,
    SyncSettingsModel,
};

impl super::super::App {
    pub(crate) fn sync_settings_model(&self) -> SyncSettingsModel {
        let busy = self.personal_state.sync_ui.syncing();
        let selected = self.personal_state.sync_ui.row;
        match self.personal_state.sync_ui.page {
            SyncPage::Overview => {
                let mut model = if self.personal_state.sync_ui.configured() {
                    SyncSettingsModel::configured(
                        self.personal_state.sync_ui.status.state,
                        self.personal_state.sync_ui.status.failure,
                    )
                } else {
                    overview_pending_model(&self.personal_state.sync_ui)
                };
                model.busy = busy;
                model.selected = selected;
                model
            }
            SyncPage::Devices => SyncSettingsModel {
                page: SyncDisplayPage::Devices,
                health: self.personal_state.sync_ui.status.state,
                failure: self.personal_state.sync_ui.status.failure,
                configured: self.personal_state.sync_ui.configured(),
                busy,
                selected,
                rows: device_rows(&self.personal_state.sync_ui),
            },
            SyncPage::Activity => SyncSettingsModel {
                page: SyncDisplayPage::Activity,
                health: self.personal_state.sync_ui.status.state,
                failure: self.personal_state.sync_ui.status.failure,
                configured: self.personal_state.sync_ui.configured(),
                busy,
                selected,
                rows: activity_rows(&self.personal_state.sync_ui),
            },
        }
    }

    pub(in crate::app) fn activate_sync_row(&mut self) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.syncing() || self.personal_state.sync.in_progress {
            return Vec::new();
        }
        let model = self.sync_settings_model();
        let Some(row) = model.selected().and_then(|index| model.rows.get(index)) else {
            return Vec::new();
        };
        match row {
            SyncRow::Action(action) => self.activate_sync_action(*action),
            SyncRow::Device(_) => self.open_selected_sync_device(),
            SyncRow::Audit(_) | SyncRow::MergeSummary(_) | SyncRow::Notice(_) => Vec::new(),
        }
    }

    fn activate_sync_action(&mut self, action: SyncRowAction) -> Vec<super::super::Cmd> {
        match action {
            SyncRowAction::Create => {
                self.personal_state.sync_ui.next_flow();
                self.personal_state.sync_ui.wizard = Some(SyncWizard::Setup {
                    form: SyncConnectionForm::setup(),
                    confirm: false,
                });
                self.dirty = true;
                Vec::new()
            }
            SyncRowAction::Join => {
                self.personal_state.sync_ui.next_flow();
                self.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
                    form: SyncConnectionForm::join(),
                    confirm: false,
                });
                self.dirty = true;
                Vec::new()
            }
            SyncRowAction::SyncNow => self.start_sync_now_from_settings(),
            SyncRowAction::Retry => self.retry_sync_lifecycle(),
            SyncRowAction::ResumeConnection => self.resume_unfinished_connection(),
            SyncRowAction::ReenterConnection => {
                self.personal_state.sync_ui.next_flow();
                self.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
                    form: SyncConnectionForm::join(),
                    confirm: false,
                });
                self.dirty = true;
                Vec::new()
            }
            SyncRowAction::DiscardConnection => {
                self.personal_state.sync_ui.next_flow();
                self.personal_state.sync_ui.wizard = Some(SyncWizard::DiscardJoinConfirm);
                self.dirty = true;
                Vec::new()
            }
            SyncRowAction::ReviewDevice => self.start_host_pairing(),
            SyncRowAction::ReviewMerge => self.retry_sync_lifecycle(),
            SyncRowAction::ViewMergeResult => {
                self.open_sync_activity();
                Vec::new()
            }
            SyncRowAction::AddDevice => self.start_host_pairing(),
            SyncRowAction::Devices => {
                self.personal_state.sync_ui.page = SyncPage::Devices;
                self.personal_state.sync_ui.row = 0;
                self.dirty = true;
                Vec::new()
            }
            SyncRowAction::Recovery | SyncRowAction::SaveRecoveryKit => {
                self.open_recovery_export();
                Vec::new()
            }
            SyncRowAction::Activity => {
                self.open_sync_activity();
                Vec::new()
            }
            SyncRowAction::Back => {
                self.personal_state.sync_ui.page = SyncPage::Overview;
                self.personal_state.sync_ui.row = 0;
                self.dirty = true;
                Vec::new()
            }
            SyncRowAction::ApproveDevice
            | SyncRowAction::RejectDevice
            | SyncRowAction::ApplyMerge
            | SyncRowAction::RemoveDevice
            | SyncRowAction::ConfirmRemoveDevice => Vec::new(),
        }
    }

    fn open_sync_activity(&mut self) {
        self.personal_state.sync_ui.page = SyncPage::Activity;
        self.personal_state.sync_ui.row = 0;
        self.dirty = true;
    }

    fn start_sync_now_from_settings(&mut self) -> Vec<super::super::Cmd> {
        let flow_id = self.personal_state.sync_ui.flow_id.max(1);
        self.personal_state.sync_ui.flow_id = flow_id;
        self.personal_state.sync_ui.busy = Some(SyncBusy::SyncNow);
        self.start_personal_sync_for_tui(super::super::PersonalSyncAction::SyncNow, flow_id)
    }

    fn retry_sync_lifecycle(&mut self) -> Vec<super::super::Cmd> {
        let flow_id = self.personal_state.sync_ui.flow_id.max(1);
        self.personal_state.sync_ui.flow_id = flow_id;
        match self.personal_state.sync_ui.lifecycle {
            SyncLifecycleState::SetupPending => {
                self.personal_state.sync_ui.busy = Some(SyncBusy::Setup);
                vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
                    SyncUiCommand::SetupResume {
                        flow_id,
                        state: Box::new(self.personal_state.ledger.clone()),
                        playlist_revision: self.playlists.revision(),
                    },
                ))]
            }
            SyncLifecycleState::JoinWaiting | SyncLifecycleState::JoinReadyToMerge => {
                self.start_join_poll(flow_id)
            }
            SyncLifecycleState::Active if self.personal_state.sync_ui.status.configured => {
                self.start_sync_now_from_settings()
            }
            SyncLifecycleState::Absent
            | SyncLifecycleState::Revoked
            | SyncLifecycleState::NeedsCleanup
            | SyncLifecycleState::Active => self.request_sync_ui_refresh(),
        }
    }

    fn resume_unfinished_connection(&mut self) -> Vec<super::super::Cmd> {
        let flow_id = self.personal_state.sync_ui.next_flow();
        self.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinPoll);
        vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
            SyncUiCommand::JoinResume {
                flow_id,
                state: Box::new(self.personal_state.ledger.clone()),
            },
        ))]
    }

    fn start_host_pairing(&mut self) -> Vec<super::super::Cmd> {
        let flow_id = self.personal_state.sync_ui.next_flow();
        self.personal_state.sync_ui.busy = Some(SyncBusy::PairHostCreate);
        vec![super::super::Cmd::Data(super::super::DataCmd::SyncUi(
            SyncUiCommand::HostCreate {
                flow_id,
                state: Box::new(self.personal_state.ledger.clone()),
            },
        ))]
    }

    fn open_recovery_export(&mut self) {
        self.personal_state.sync_ui.next_flow();
        self.personal_state.sync_ui.wizard = Some(SyncWizard::Recovery(SyncRecoveryForm {
            source: String::new(),
            destination: directories::UserDirs::new()
                .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            field: 0,
            cursor: crate::util::text_edit::TextCursor::default(),
            confirm: false,
        }));
        self.dirty = true;
    }

    fn open_selected_sync_device(&mut self) -> Vec<super::super::Cmd> {
        if self.personal_state.sync_ui.page != SyncPage::Devices
            || self.personal_state.sync_ui.row < 3
        {
            return Vec::new();
        }
        let index = self.personal_state.sync_ui.row - 3;
        let Some(device) = self.personal_state.sync_ui.devices.get(index) else {
            return Vec::new();
        };
        let local = self.personal_state.sync_ui.status.device_id.as_deref();
        if !device.active || !device.keyed || Some(device.device_id.as_str()) == local {
            return Vec::new();
        }
        self.personal_state.sync_ui.wizard = Some(SyncWizard::Revoke {
            device_id: device.device_id.clone(),
            device_name: device.name.clone(),
        });
        self.dirty = true;
        Vec::new()
    }
}

fn overview_pending_model(state: &SyncUiState) -> SyncSettingsModel {
    let rows = match state.lifecycle {
        SyncLifecycleState::Absent => vec![
            SyncRow::Action(SyncRowAction::Create),
            SyncRow::Action(SyncRowAction::Join),
        ],
        SyncLifecycleState::Revoked => vec![
            SyncRow::Notice(SyncNotice::DeviceRevoked),
            SyncRow::Action(SyncRowAction::Activity),
        ],
        SyncLifecycleState::SetupPending => vec![
            SyncRow::Notice(SyncNotice::RecoveryKitRequired),
            SyncRow::Action(SyncRowAction::Retry),
            SyncRow::Action(SyncRowAction::Activity),
        ],
        SyncLifecycleState::JoinWaiting => vec![
            SyncRow::Notice(SyncNotice::WaitingForDevice),
            SyncRow::Action(SyncRowAction::Retry),
            SyncRow::Action(SyncRowAction::Activity),
        ],
        SyncLifecycleState::JoinReadyToMerge => vec![
            SyncRow::Notice(SyncNotice::FirstMergeKeepsEverything),
            SyncRow::Action(SyncRowAction::ReviewMerge),
            SyncRow::Action(SyncRowAction::Activity),
        ],
        SyncLifecycleState::NeedsCleanup => vec![
            SyncRow::Notice(SyncNotice::NeedsCleanup),
            SyncRow::Action(SyncRowAction::ResumeConnection),
            SyncRow::Action(SyncRowAction::ReenterConnection),
            SyncRow::Action(SyncRowAction::DiscardConnection),
            SyncRow::Action(SyncRowAction::Activity),
        ],
        SyncLifecycleState::Active => vec![
            SyncRow::Action(SyncRowAction::SyncNow),
            SyncRow::Action(SyncRowAction::Activity),
        ],
    };
    SyncSettingsModel {
        page: SyncDisplayPage::Status,
        health: state.status.state,
        failure: (state.lifecycle != SyncLifecycleState::NeedsCleanup)
            .then_some(state.status.failure)
            .flatten(),
        configured: false,
        busy: state.syncing(),
        selected: state.row,
        rows,
    }
}

fn device_rows(state: &SyncUiState) -> Vec<SyncRow> {
    let mut rows = vec![
        SyncRow::Action(SyncRowAction::Back),
        SyncRow::Action(SyncRowAction::AddDevice),
        SyncRow::Action(SyncRowAction::Recovery),
    ];
    if state.devices.is_empty() {
        rows.push(SyncRow::Notice(SyncNotice::NoOtherDevices));
        return rows;
    }
    let local = state.status.device_id.as_deref();
    rows.extend(state.devices.iter().map(|device| {
        SyncRow::Device(SyncDeviceRow::new(
            &device.name,
            &device.device_id,
            Some(device.device_id.as_str()) == local,
            device.active,
        ))
    }));
    rows
}

fn activity_rows(state: &SyncUiState) -> Vec<SyncRow> {
    let mut rows = vec![SyncRow::Action(SyncRowAction::Back)];
    if state.audit.is_empty() {
        rows.push(SyncRow::Notice(SyncNotice::NoActivity));
    } else {
        rows.extend(
            state
                .audit
                .iter()
                .rev()
                .take(50)
                .map(SyncAuditRow::from)
                .map(SyncRow::Audit),
        );
    }
    rows
}
