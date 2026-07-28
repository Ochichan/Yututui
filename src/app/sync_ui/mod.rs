mod event_reducer;
mod form;
mod settings_reducer;
mod wizard_reducer;

use zeroize::Zeroizing;

pub(crate) use form::{
    ConnectionField, FormError, SyncConnectionForm, SyncConnectionInput, SyncJoinRequest,
};

use crate::sync::service::{
    DeviceSummary, PairingHostInvite, PairingJoinPreview, PairingJoinWaiting, PairingReview,
    PreparedPairingApproval, PreparedPairingJoinActivation, PreparedSetup, SyncLifecycleState,
    SyncStatusReport,
};
use crate::sync::{SyncAuditEntry, SyncHealthState};

/// Blocking sync-settings work. Secret-bearing variants intentionally implement neither
/// `Debug` nor `Clone`; each is moved exactly once from the owner to a worker.
pub enum SyncUiCommand {
    Refresh {
        flow_id: u64,
        request_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        in_progress: bool,
    },
    RecoveryExport {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        source: std::path::PathBuf,
        destination: std::path::PathBuf,
    },
    SetupPrepare {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        playlist_revision: u64,
        input: SyncConnectionInput,
    },
    SetupResume {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        playlist_revision: u64,
    },
    HostCreate {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
    },
    HostPoll {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        host: Box<PairingHostInvite>,
    },
    HostApprove {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        host: Box<PairingHostInvite>,
        review: Box<PairingReview>,
    },
    HostCancel {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        host: Box<PairingHostInvite>,
    },
    JoinStart {
        flow_id: u64,
        input: SyncConnectionInput,
    },
    JoinPoll {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
    },
    JoinResume {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
    },
    DiscardJoin {
        flow_id: u64,
    },
    JoinPrepareActivation {
        flow_id: u64,
        state: Box<crate::personal_state::PersonalStateV2>,
        preview: Box<PairingJoinPreview>,
    },
}

impl SyncUiCommand {
    pub(crate) fn flow_id(&self) -> u64 {
        match self {
            Self::Refresh { flow_id, .. }
            | Self::RecoveryExport { flow_id, .. }
            | Self::SetupPrepare { flow_id, .. }
            | Self::SetupResume { flow_id, .. }
            | Self::HostCreate { flow_id, .. }
            | Self::HostPoll { flow_id, .. }
            | Self::HostApprove { flow_id, .. }
            | Self::HostCancel { flow_id, .. }
            | Self::JoinStart { flow_id, .. }
            | Self::JoinPoll { flow_id, .. }
            | Self::JoinResume { flow_id, .. }
            | Self::DiscardJoin { flow_id }
            | Self::JoinPrepareActivation { flow_id, .. } => *flow_id,
        }
    }

    pub(crate) fn is_read_only(&self) -> bool {
        matches!(self, Self::Refresh { .. })
    }
}

pub enum SyncUiEvent {
    Refreshed {
        flow_id: u64,
        request_id: u64,
        result:
            Box<Result<crate::sync::service::SyncOverview, crate::sync::service::SyncServiceError>>,
    },
    RecoveryExported {
        flow_id: u64,
        result: Box<
            Result<
                crate::sync::service::RecoveryExportResult,
                crate::sync::service::SyncServiceError,
            >,
        >,
    },
    SetupPrepared {
        flow_id: u64,
        result: Box<Result<PreparedSetup, crate::sync::service::SyncServiceError>>,
    },
    HostCreated {
        flow_id: u64,
        result: Box<Result<PairingHostInvite, crate::sync::service::SyncServiceError>>,
    },
    HostPolled {
        flow_id: u64,
        host: Box<PairingHostInvite>,
        result: Box<Result<Option<PairingReview>, crate::sync::service::SyncServiceError>>,
    },
    HostApprovalPrepared {
        flow_id: u64,
        host: Box<PairingHostInvite>,
        observed_state: Box<crate::personal_state::PersonalStateV2>,
        result: Box<Result<PreparedPairingApproval, crate::sync::service::SyncServiceError>>,
    },
    HostCancelled {
        flow_id: u64,
        host: Box<PairingHostInvite>,
        result: Result<(), crate::sync::service::SyncServiceError>,
    },
    JoinStarted {
        flow_id: u64,
        result: Box<Result<PairingJoinWaiting, crate::sync::service::SyncServiceError>>,
    },
    JoinPolled {
        flow_id: u64,
        result: Box<Result<Option<PairingJoinPreview>, crate::sync::service::SyncServiceError>>,
    },
    JoinResumed {
        flow_id: u64,
        result: Box<Result<PairingJoinPreview, crate::sync::service::SyncServiceError>>,
    },
    JoinDiscarded {
        flow_id: u64,
        result: Result<(), crate::sync::service::SyncServiceError>,
    },
    JoinActivationPrepared {
        flow_id: u64,
        result: Box<Result<PreparedPairingJoinActivation, crate::sync::service::SyncServiceError>>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncPage {
    Overview,
    Devices,
    Activity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncBusy {
    Refresh,
    Setup,
    SyncNow,
    PairHostCreate,
    PairHostPoll,
    PairHostApprove,
    PairHostCancel,
    PairJoinStart,
    PairJoinPoll,
    PairJoinDiscard,
    PairJoinApply,
    Revoke,
    RecoveryExport,
}

impl SyncBusy {
    /// Whether this worker may mutate the encrypted WebDAV vault.
    ///
    /// Personal sync shares one primary-owner network lane with these pairing/setup phases.
    /// Poll-only host review, local recovery/refresh work, and the personal-sync variants
    /// themselves deliberately stay outside this classification.
    pub(crate) fn blocks_personal_sync(self) -> bool {
        matches!(
            self,
            Self::Setup
                | Self::PairHostCreate
                | Self::PairHostApprove
                | Self::PairJoinStart
                | Self::PairJoinPoll
        )
    }
}

pub(crate) enum SyncWizard {
    Setup {
        form: SyncConnectionForm,
        confirm: bool,
    },
    Join {
        form: SyncConnectionForm,
        confirm: bool,
    },
    Host {
        code: Zeroizing<String>,
        expires_at_unix: i64,
        host: Option<Box<PairingHostInvite>>,
        review: Option<Box<PairingReview>>,
    },
    JoinWaiting(Box<PairingJoinWaiting>),
    JoinPreview(Box<PairingJoinPreview>),
    DiscardJoinConfirm,
    Revoke {
        device_id: String,
        device_name: String,
    },
    Recovery(SyncRecoveryForm),
    Result {
        success: bool,
        message: String,
    },
}

impl SyncWizard {
    pub(crate) fn is_modal(&self) -> bool {
        true
    }
}

pub(crate) struct SyncRecoveryForm {
    pub(crate) source: String,
    pub(crate) destination: String,
    pub(crate) field: usize,
    pub(crate) cursor: crate::util::text_edit::TextCursor,
    pub(crate) confirm: bool,
}

impl Drop for SyncRecoveryForm {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.source.zeroize();
        self.destination.zeroize();
    }
}

/// Interactive sync state owned by the primary App lane.
///
/// No `Debug` or `Clone`: wizard variants may own pairing material and private path/input values.
pub(crate) struct SyncUiState {
    pub(crate) flow_id: u64,
    pub(crate) page: SyncPage,
    pub(crate) row: usize,
    pub(crate) status: SyncStatusReport,
    pub(crate) lifecycle: SyncLifecycleState,
    pub(crate) devices: Vec<DeviceSummary>,
    pub(crate) audit: Vec<SyncAuditEntry>,
    pub(crate) busy: Option<SyncBusy>,
    pub(crate) wizard: Option<SyncWizard>,
    pub(crate) last_poll_unix: Option<i64>,
    pub(crate) auto_poll_enabled: bool,
    pub(crate) refresh_request_id: u64,
    pub(crate) refresh_in_flight: Option<u64>,
    pub(crate) refresh_pending: bool,
}

impl Default for SyncUiState {
    fn default() -> Self {
        Self {
            flow_id: 0,
            page: SyncPage::Overview,
            row: 0,
            status: SyncStatusReport::default(),
            lifecycle: SyncLifecycleState::Absent,
            devices: Vec::new(),
            audit: Vec::new(),
            busy: None,
            wizard: None,
            last_poll_unix: None,
            auto_poll_enabled: false,
            refresh_request_id: 0,
            refresh_in_flight: None,
            refresh_pending: false,
        }
    }
}

impl SyncUiState {
    pub(crate) fn next_flow(&mut self) -> u64 {
        if matches!(self.busy, Some(SyncBusy::Refresh)) {
            self.busy = None;
        }
        self.refresh_in_flight = None;
        self.auto_poll_enabled = false;
        self.flow_id = self.flow_id.wrapping_add(1).max(1);
        self.flow_id
    }

    pub(crate) fn is_current(&self, flow_id: u64) -> bool {
        self.flow_id == flow_id
    }

    pub(crate) fn configured(&self) -> bool {
        self.lifecycle == SyncLifecycleState::Active && self.status.configured
    }

    pub(crate) fn syncing(&self) -> bool {
        self.busy.is_some() || self.status.state == SyncHealthState::Syncing
    }

    pub(crate) fn remote_mutation_in_progress(&self) -> bool {
        self.busy.is_some_and(SyncBusy::blocks_personal_sync)
    }

    pub(crate) fn modal_open(&self) -> bool {
        self.wizard.as_ref().is_some_and(SyncWizard::is_modal)
    }

    pub(crate) fn set_overview(&mut self, overview: crate::sync::service::SyncOverview) {
        self.status = overview.status;
        self.lifecycle = overview.lifecycle;
        self.devices = overview.devices;
        self.audit = overview.audit;
        self.row = self.row.min(self.row_count().saturating_sub(1));
    }

    pub(crate) fn row_count(&self) -> usize {
        match self.page {
            SyncPage::Overview if self.configured() => {
                4 + usize::from(self.status.failure.is_some())
            }
            SyncPage::Overview => match self.lifecycle {
                SyncLifecycleState::JoinWaiting => 3,
                SyncLifecycleState::JoinReadyToMerge => 3,
                SyncLifecycleState::SetupPending => 3,
                SyncLifecycleState::NeedsCleanup => 5,
                SyncLifecycleState::Absent
                | SyncLifecycleState::Revoked
                | SyncLifecycleState::Active => 2,
            },
            SyncPage::Devices => 3 + self.devices.len().max(1),
            SyncPage::Activity => 1 + self.audit.len().clamp(1, 50),
        }
    }

    #[cfg(test)]
    pub(crate) fn revocable_devices(&self) -> impl Iterator<Item = &DeviceSummary> {
        let local = self.status.device_id.as_deref();
        self.devices.iter().filter(move |device| {
            device.active && device.keyed && Some(device.device_id.as_str()) != local
        })
    }

    pub(crate) fn move_row(&mut self, delta: i32) {
        let last = self.row_count().saturating_sub(1) as i32;
        self.row = (self.row as i32 + delta).clamp(0, last.max(0)) as usize;
    }
}

impl super::App {
    pub(in crate::app) fn request_sync_ui_refresh(&mut self) -> Vec<super::Cmd> {
        self.queue_sync_ui_refresh();
        self.start_pending_sync_ui_refresh()
    }

    pub(crate) fn queue_sync_ui_refresh(&mut self) {
        self.personal_state.sync_ui.refresh_request_id = self
            .personal_state
            .sync_ui
            .refresh_request_id
            .wrapping_add(1)
            .max(1);
        self.personal_state.sync_ui.refresh_pending = true;
    }

    pub(crate) fn start_pending_sync_ui_refresh(&mut self) -> Vec<super::Cmd> {
        let mut commands = self.resume_automatic_sync_if_ready();
        if !self.personal_state.sync_ui.refresh_pending
            || self.personal_state.sync_ui.refresh_in_flight.is_some()
            || self.personal_state.sync_ui.busy.is_some()
        {
            return commands;
        }
        let flow_id = self.personal_state.sync_ui.flow_id.max(1);
        if self.personal_state.sync_ui.flow_id == 0 {
            self.personal_state.sync_ui.flow_id = flow_id;
        }
        let request_id = self.personal_state.sync_ui.refresh_request_id;
        self.personal_state.sync_ui.refresh_pending = false;
        self.personal_state.sync_ui.refresh_in_flight = Some(request_id);
        self.personal_state.sync_ui.busy = Some(SyncBusy::Refresh);
        commands.push(super::Cmd::Data(super::DataCmd::SyncUi(
            SyncUiCommand::Refresh {
                flow_id,
                request_id,
                state: Box::new(self.personal_state.ledger.clone()),
                in_progress: self.personal_state.sync.in_progress,
            },
        )));
        commands
    }

    pub(in crate::app) fn finish_sync_ui_event(&mut self, event: SyncUiEvent) -> Vec<super::Cmd> {
        self.reduce_sync_ui_event(event)
    }

    pub(in crate::app) fn finish_tui_personal_sync(
        &mut self,
        flow_id: u64,
        action: &super::PersonalSyncAction,
        summary: &crate::sync::manual::ManualSyncSummary,
    ) {
        if !self.personal_state.sync_ui.is_current(flow_id) {
            return;
        }
        self.personal_state.sync_ui.busy = None;
        self.queue_sync_ui_refresh();
        let message = if matches!(action, super::PersonalSyncAction::Revoke(_)) {
            crate::t!(
                "Device removed. Change the shared WebDAV password if it knew it.",
                "기기를 삭제했어요. 해당 기기가 비밀번호를 알았다면 WebDAV 비밀번호를 바꾸세요.",
                "デバイスを削除しました。パスワードを知られていた場合はWebDAVのパスワードを変更してください。"
            )
            .to_owned()
        } else if summary.downloaded_operations == 0 && summary.uploaded_operations == 0 {
            crate::t!(
                "Personal state is up to date.",
                "개인 상태가 최신이에요.",
                "個人データは最新です。"
            )
            .to_owned()
        } else {
            match crate::i18n::current() {
                crate::i18n::Language::Korean => format!(
                    "동기화 완료: {}개 보냄, {}개 받음",
                    summary.uploaded_operations, summary.downloaded_operations
                ),
                crate::i18n::Language::Japanese => format!(
                    "同期完了: {}件送信、{}件受信",
                    summary.uploaded_operations, summary.downloaded_operations
                ),
                _ => format!(
                    "Sync complete: {} sent, {} received.",
                    summary.uploaded_operations, summary.downloaded_operations
                ),
            }
        };
        self.status.text = message;
        self.status.kind = super::StatusKind::Info;
        self.dirty = true;
    }

    pub(in crate::app) fn finish_tui_personal_sync_error(
        &mut self,
        flow_id: u64,
        error: crate::sync::service::SyncServiceError,
    ) {
        if !self.personal_state.sync_ui.is_current(flow_id) {
            return;
        }
        self.personal_state.sync_ui.busy = None;
        self.queue_sync_ui_refresh();
        self.status.text = localized_sync_error(error);
        self.status.kind = super::StatusKind::Error;
        self.dirty = true;
    }
}

pub(crate) fn localized_sync_error(error: crate::sync::service::SyncServiceError) -> String {
    use crate::sync::service::SyncServiceError as Error;
    match error {
        Error::NotConfigured => crate::t!(
            "Personal sync is not set up.",
            "개인 동기화가 설정되지 않았어요.",
            "個人同期は設定されていません。"
        ),
        Error::AlreadyConfigured => crate::t!(
            "Personal sync is already set up.",
            "개인 동기화가 이미 설정되어 있어요.",
            "個人同期はすでに設定されています。"
        ),
        Error::PendingApproval => crate::t!(
            "Waiting for device approval.",
            "기기 승인을 기다리고 있어요.",
            "デバイスの承認を待っています。"
        ),
        Error::Revoked => crate::t!(
            "This device no longer has access.",
            "이 기기는 더 이상 접근할 수 없어요.",
            "このデバイスにはアクセス権がありません。"
        ),
        Error::MissingCredential | Error::Authentication => crate::t!(
            "The password was not accepted.",
            "비밀번호를 확인해 주세요.",
            "パスワードを確認してください。"
        ),
        Error::UnsupportedServer => crate::t!(
            "This server cannot store encrypted personal sync.",
            "이 서버에서는 암호화된 개인 동기화를 사용할 수 없어요.",
            "このサーバーでは暗号化された個人同期を利用できません。"
        ),
        Error::Certificate => crate::t!(
            "The server certificate could not be verified.",
            "서버 인증서를 확인할 수 없어요.",
            "サーバー証明書を確認できません。"
        ),
        Error::Offline | Error::RateLimited(_) => crate::t!(
            "Offline — try again when the server is reachable.",
            "오프라인 — 서버에 연결되면 다시 시도하세요.",
            "オフライン — サーバーに接続できたら再試行してください。"
        ),
        Error::InvalidRemoteData | Error::PairingRejected => crate::t!(
            "The encrypted server data could not be verified.",
            "서버의 암호화된 데이터를 확인할 수 없어요.",
            "サーバーの暗号化データを確認できません。"
        ),
        Error::LocalStateChanged => crate::t!(
            "Local changes arrived. Please review the updated result.",
            "로컬 변경이 생겼어요. 갱신된 결과를 확인해 주세요.",
            "ローカルで変更がありました。更新後の結果を確認してください。"
        ),
        Error::Storage | Error::RecoveryKitNotConfirmed => crate::t!(
            "The change could not be saved safely.",
            "변경 내용을 안전하게 저장하지 못했어요.",
            "変更を安全に保存できませんでした。"
        ),
        Error::PairingExpired => crate::t!(
            "The device connection code expired.",
            "기기 연결 코드가 만료됐어요.",
            "デバイス接続コードの有効期限が切れました。"
        ),
        Error::PairingNeedsCleanup => crate::t!(
            "An unfinished device connection needs attention.",
            "완료되지 않은 기기 연결을 정리해야 해요.",
            "未完了のデバイス接続を整理する必要があります。"
        ),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_sync_blocking_busy_variants_are_exact() {
        for busy in [
            SyncBusy::Setup,
            SyncBusy::PairHostCreate,
            SyncBusy::PairHostApprove,
            SyncBusy::PairJoinStart,
            SyncBusy::PairJoinPoll,
        ] {
            assert!(busy.blocks_personal_sync());
        }
        for busy in [
            SyncBusy::Refresh,
            SyncBusy::SyncNow,
            SyncBusy::PairHostPoll,
            SyncBusy::PairHostCancel,
            SyncBusy::PairJoinDiscard,
            SyncBusy::PairJoinApply,
            SyncBusy::Revoke,
            SyncBusy::RecoveryExport,
        ] {
            assert!(!busy.blocks_personal_sync());
        }
    }

    #[test]
    fn local_device_is_never_a_revoke_row() {
        let mut state = SyncUiState::default();
        state.status.configured = true;
        state.status.device_id = Some("this".to_owned());
        state.lifecycle = SyncLifecycleState::Active;
        state.devices = vec![
            DeviceSummary {
                device_id: "this".to_owned(),
                name: "This device".to_owned(),
                active: true,
                keyed: true,
            },
            DeviceSummary {
                device_id: "other".to_owned(),
                name: "Other device".to_owned(),
                active: true,
                keyed: true,
            },
        ];
        assert_eq!(
            state
                .revocable_devices()
                .map(|device| device.device_id.as_str())
                .collect::<Vec<_>>(),
            vec!["other"]
        );
    }
}
