//! Privacy-safe presentation state for the Sync settings tab.
//!
//! This module deliberately has no endpoint, credential, recovery identity, pairing secret, or
//! filesystem-path field. Network workers keep those values in their short-lived request state
//! and project only these typed summaries into the renderer.

use crate::personal_state::DeviceRecord;
use crate::sync::{
    SyncAuditAction, SyncAuditEntry, SyncAuditOutcome, SyncFailureKind, SyncHealthState,
};
use crate::t;

/// The Sync tab's current screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncDisplayPage {
    #[default]
    Status,
    Create,
    Join,
    PairDevice,
    MergePreview,
    Devices,
    Recovery,
    Activity,
    RevokeDevice,
}

impl SyncDisplayPage {
    pub fn title(self) -> &'static str {
        match self {
            Self::Status => t!("Personal sync", "개인 동기화", "個人データ同期"),
            Self::Create => t!(
                "Set up personal sync",
                "개인 동기화 설정",
                "個人データ同期を設定"
            ),
            Self::Join => t!(
                "Join personal sync",
                "개인 동기화에 연결",
                "個人データ同期に参加"
            ),
            Self::PairDevice => t!("Add a device", "기기 추가", "デバイスを追加"),
            Self::MergePreview => t!("Review changes", "변경 사항 확인", "変更内容を確認"),
            Self::Devices => t!("Devices", "기기", "デバイス"),
            Self::Recovery => t!("Recovery", "복구", "復旧"),
            Self::Activity => t!("Sync activity", "동기화 활동", "同期アクティビティ"),
            Self::RevokeDevice => t!("Remove device", "기기 제거", "デバイスを削除"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Status => t!(
                "Your listening data stays available on this device.",
                "감상 데이터는 이 기기에서 계속 사용할 수 있습니다.",
                "再生データはこのデバイスでいつでも利用できます。"
            ),
            Self::Create => t!(
                "Save a recovery kit before connecting another device.",
                "다른 기기를 연결하기 전에 복구 키트를 저장하세요.",
                "別のデバイスを接続する前に復旧キットを保存します。"
            ),
            Self::Join => t!(
                "Enter the connection details and one-time code from an approved device.",
                "연결 정보와 승인된 기기의 일회용 코드를 입력하세요.",
                "接続情報と承認済みデバイスの一回限りのコードを入力します。"
            ),
            Self::PairDevice => t!(
                "The code expires after 10 minutes. Review the device before approving it.",
                "코드는 10분 후 만료됩니다. 승인 전에 기기를 확인하세요.",
                "コードは10分後に期限切れになります。承認前にデバイスを確認してください。"
            ),
            Self::MergePreview => t!(
                "The first merge keeps both sides. Nothing is deleted.",
                "첫 병합은 양쪽 데이터를 모두 유지하며 삭제하지 않습니다.",
                "最初の統合では両方のデータを残し、削除しません。"
            ),
            Self::Devices => t!(
                "Only remove a device you no longer trust or use.",
                "더 이상 신뢰하거나 사용하지 않는 기기만 제거하세요.",
                "信頼しない、または使わなくなったデバイスだけを削除してください。"
            ),
            Self::Recovery => t!(
                "Keep the recovery kit separate from your sync password.",
                "복구 키트는 동기화 비밀번호와 별도로 보관하세요.",
                "復旧キットは同期パスワードとは別に保管してください。"
            ),
            Self::Activity => t!(
                "Recent results are shown without passwords, addresses, or file locations.",
                "최근 결과에는 비밀번호, 주소, 파일 위치가 표시되지 않습니다.",
                "最近の結果にはパスワード、アドレス、ファイルの場所は表示されません。"
            ),
            Self::RevokeDevice => t!(
                "Removed devices cannot receive future changes.",
                "제거된 기기는 이후 변경 사항을 받을 수 없습니다.",
                "削除したデバイスは今後の変更を受け取れません。"
            ),
        }
    }
}

/// Stable actions a Sync row can invoke. Secret-bearing form values live outside this model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncRowAction {
    Create,
    Join,
    SyncNow,
    Retry,
    ResumeConnection,
    ReenterConnection,
    DiscardConnection,
    ReviewDevice,
    ReviewMerge,
    ViewMergeResult,
    AddDevice,
    Devices,
    Recovery,
    Activity,
    SaveRecoveryKit,
    ApproveDevice,
    RejectDevice,
    ApplyMerge,
    RemoveDevice,
    ConfirmRemoveDevice,
    Back,
}

impl SyncRowAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Create => t!(
                "Create encrypted sync",
                "암호화 동기화 만들기",
                "暗号化同期を作成"
            ),
            Self::Join => t!(
                "Join existing sync",
                "기존 동기화에 연결",
                "既存の同期に参加"
            ),
            Self::SyncNow => t!("Sync now", "지금 동기화", "今すぐ同期"),
            Self::Retry => t!("Retry", "다시 시도", "再試行"),
            Self::ResumeConnection => t!(
                "Continue saved connection",
                "저장된 연결 이어가기",
                "保存した接続を再開"
            ),
            Self::ReenterConnection => t!(
                "Enter connection details again",
                "연결 정보 다시 입력",
                "接続情報を再入力"
            ),
            Self::DiscardConnection => t!(
                "Discard unfinished connection",
                "완료되지 않은 연결 버리기",
                "未完了の接続を破棄"
            ),
            Self::ReviewDevice => t!("Review device", "기기 확인", "デバイスを確認"),
            Self::ReviewMerge => t!("Review changes", "변경 사항 확인", "変更内容を確認"),
            Self::ViewMergeResult => {
                t!("View merge result", "병합 결과 보기", "統合結果を表示")
            }
            Self::AddDevice => t!("Add device", "기기 추가", "デバイスを追加"),
            Self::Devices => t!("Devices & recovery", "기기 및 복구", "デバイスと復旧"),
            Self::Recovery => t!("Recovery", "복구", "復旧"),
            Self::Activity => t!("View sync activity", "동기화 활동 보기", "同期履歴を表示"),
            Self::SaveRecoveryKit => {
                t!("Save recovery kit", "복구 키트 저장", "復旧キットを保存")
            }
            Self::ApproveDevice => t!("Approve device", "기기 승인", "デバイスを承認"),
            Self::RejectDevice => t!("Reject device", "기기 거절", "デバイスを拒否"),
            Self::ApplyMerge => t!("Merge these changes", "변경 사항 병합", "この変更を統合"),
            Self::RemoveDevice => t!("Remove device", "기기 제거", "デバイスを削除"),
            Self::ConfirmRemoveDevice => {
                t!("Remove this device", "이 기기 제거", "このデバイスを削除")
            }
            Self::Back => t!("Back", "뒤로", "戻る"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncNotice {
    RecoveryKitRequired,
    WaitingForDevice,
    FirstMergeKeepsEverything,
    NeedsCleanup,
    DeviceRevoked,
    RecoveryKitSaved,
    NoOtherDevices,
    NoActivity,
}

impl SyncNotice {
    pub fn label(self) -> &'static str {
        match self {
            Self::RecoveryKitRequired => t!(
                "Save and confirm your recovery kit to finish setup.",
                "설정을 마치려면 복구 키트를 저장하고 확인하세요.",
                "設定を完了するには復旧キットを保存して確認してください。"
            ),
            Self::WaitingForDevice => t!(
                "Waiting for the other device…",
                "다른 기기를 기다리는 중…",
                "もう一方のデバイスを待っています…"
            ),
            Self::FirstMergeKeepsEverything => t!(
                "Both sides will be kept. Nothing will be deleted.",
                "양쪽 데이터를 모두 유지하며 삭제하지 않습니다.",
                "両方のデータを残し、削除しません。"
            ),
            Self::NeedsCleanup => t!(
                "A connection did not finish. Local listening data is safe. Try the saved connection or enter the same details again.",
                "연결이 완료되지 않았습니다. 이 기기의 감상 데이터는 안전합니다. 저장된 연결을 이어가거나 같은 정보를 다시 입력하세요.",
                "接続が完了しませんでした。ローカルの再生データは安全です。保存した接続を再開するか、同じ情報を再入力してください。"
            ),
            Self::DeviceRevoked => t!(
                "This device was removed from sync. Its local listening data was not deleted.",
                "이 기기는 동기화에서 제거되었습니다. 로컬 감상 데이터는 삭제되지 않았습니다.",
                "このデバイスは同期から削除されました。ローカルの再生データは削除されていません。"
            ),
            Self::RecoveryKitSaved => {
                t!(
                    "Recovery kit saved",
                    "복구 키트 저장됨",
                    "復旧キットを保存しました"
                )
            }
            Self::NoOtherDevices => t!(
                "No other connected devices",
                "연결된 다른 기기 없음",
                "ほかに接続済みのデバイスはありません"
            ),
            Self::NoActivity => t!(
                "No sync activity yet",
                "아직 동기화 활동 없음",
                "同期履歴はまだありません"
            ),
        }
    }
}

/// A single device projected for display. Values are normalized to bounded, single-line text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDeviceRow {
    name: String,
    fingerprint: String,
    pub current: bool,
    pub active: bool,
}

impl SyncDeviceRow {
    pub fn new(name: &str, fingerprint: &str, current: bool, active: bool) -> Self {
        Self {
            name: safe_one_line(
                name,
                80,
                t!("Unnamed device", "이름 없는 기기", "名前のないデバイス"),
            ),
            fingerprint: safe_one_line(fingerprint, 96, "—"),
            current,
            active,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// A redacted activity row. Counts and typed outcomes are safe to retain in Settings state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncAuditRow {
    pub at_unix: i64,
    pub action: SyncAuditAction,
    pub outcome: SyncAuditOutcome,
    pub failure: Option<SyncFailureKind>,
    pub local_changes: usize,
    pub remote_changes: usize,
}

impl From<&SyncAuditEntry> for SyncAuditRow {
    fn from(entry: &SyncAuditEntry) -> Self {
        Self {
            at_unix: entry.at_unix,
            action: entry.action,
            outcome: entry.outcome,
            failure: entry.failure,
            local_changes: entry.local_operations,
            remote_changes: entry.remote_operations,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncMergeSummary {
    pub local_changes: usize,
    pub remote_changes: usize,
    pub duplicates_skipped: usize,
}

/// A presentation row. It contains no arbitrary endpoint, path, credential, or error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncRow {
    Action(SyncRowAction),
    Device(SyncDeviceRow),
    Audit(SyncAuditRow),
    MergeSummary(SyncMergeSummary),
    Notice(SyncNotice),
}

/// Cloneable, redacted snapshot consumed by the Sync settings renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSettingsModel {
    pub page: SyncDisplayPage,
    pub health: SyncHealthState,
    pub failure: Option<SyncFailureKind>,
    pub configured: bool,
    pub busy: bool,
    pub selected: usize,
    pub rows: Vec<SyncRow>,
}

impl Default for SyncSettingsModel {
    fn default() -> Self {
        Self::off()
    }
}

impl SyncSettingsModel {
    pub fn off() -> Self {
        Self {
            page: SyncDisplayPage::Status,
            health: SyncHealthState::Off,
            failure: None,
            configured: false,
            busy: false,
            selected: 0,
            rows: vec![
                SyncRow::Action(SyncRowAction::Create),
                SyncRow::Action(SyncRowAction::Join),
            ],
        }
    }

    pub fn configured(health: SyncHealthState, failure: Option<SyncFailureKind>) -> Self {
        let mut rows = Vec::new();
        if let Some(failure) = failure {
            rows.push(SyncRow::Action(failure_action(failure)));
        }
        rows.extend([
            SyncRow::Action(SyncRowAction::SyncNow),
            SyncRow::Action(SyncRowAction::AddDevice),
            SyncRow::Action(SyncRowAction::Devices),
            SyncRow::Action(SyncRowAction::Activity),
        ]);
        Self {
            page: SyncDisplayPage::Status,
            health,
            failure,
            configured: true,
            busy: health == SyncHealthState::Syncing,
            selected: 0,
            rows,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        (!self.rows.is_empty()).then(|| self.selected.min(self.rows.len() - 1))
    }
}

fn failure_action(failure: SyncFailureKind) -> SyncRowAction {
    match failure {
        SyncFailureKind::RemoteChanged => SyncRowAction::ViewMergeResult,
        SyncFailureKind::DeviceApproval => SyncRowAction::ReviewDevice,
        SyncFailureKind::InvalidRemoteData | SyncFailureKind::Storage => SyncRowAction::Activity,
        SyncFailureKind::Authentication
        | SyncFailureKind::Certificate
        | SyncFailureKind::Offline
        | SyncFailureKind::LocalStateChanged => SyncRowAction::Retry,
    }
}

pub fn health_label(state: SyncHealthState) -> &'static str {
    match state {
        SyncHealthState::Off => t!("Off", "꺼짐", "オフ"),
        SyncHealthState::UpToDate => t!("Up to date", "최신 상태", "最新"),
        SyncHealthState::Syncing => t!("Syncing", "동기화 중", "同期中"),
        SyncHealthState::OfflineWillRetry => t!(
            "Offline — will retry",
            "오프라인 — 다시 시도함",
            "オフライン — 再試行します"
        ),
        SyncHealthState::NeedsAttention => t!("Needs attention", "확인 필요", "確認が必要"),
    }
}

pub fn failure_label(failure: SyncFailureKind) -> &'static str {
    match failure {
        SyncFailureKind::Authentication => t!(
            "The password needs updating",
            "비밀번호를 업데이트해야 합니다",
            "パスワードの更新が必要です"
        ),
        SyncFailureKind::Certificate => t!(
            "The certificate needs attention",
            "인증서를 확인해야 합니다",
            "証明書の確認が必要です"
        ),
        SyncFailureKind::Offline => t!(
            "Sync storage cannot be reached",
            "동기화 저장소에 연결할 수 없습니다",
            "同期ストレージに接続できません"
        ),
        SyncFailureKind::RemoteChanged => t!(
            "New changes were found",
            "새 변경 사항을 찾았습니다",
            "新しい変更が見つかりました"
        ),
        SyncFailureKind::DeviceApproval => t!(
            "A device is waiting for approval",
            "승인을 기다리는 기기가 있습니다",
            "承認待ちのデバイスがあります"
        ),
        SyncFailureKind::InvalidRemoteData => t!(
            "Sync data needs review",
            "동기화 데이터를 확인해야 합니다",
            "同期データの確認が必要です"
        ),
        SyncFailureKind::LocalStateChanged => t!(
            "Local changes arrived during sync",
            "동기화 중 로컬 변경 사항이 생겼습니다",
            "同期中にこのデバイスのデータが変更されました"
        ),
        SyncFailureKind::Storage => t!(
            "Sync files need review",
            "동기화 파일을 확인해야 합니다",
            "同期ファイルの確認が必要です"
        ),
    }
}

pub fn failure_recovery_label(failure: SyncFailureKind) -> &'static str {
    match failure {
        SyncFailureKind::Authentication
        | SyncFailureKind::Certificate
        | SyncFailureKind::Offline
        | SyncFailureKind::LocalStateChanged => {
            t!("Retry", "다시 시도", "再試行")
        }
        SyncFailureKind::RemoteChanged => {
            t!("View merge result", "병합 결과 보기", "統合結果を表示")
        }
        SyncFailureKind::DeviceApproval => {
            t!("Review device", "기기 확인", "デバイスを確認")
        }
        SyncFailureKind::InvalidRemoteData | SyncFailureKind::Storage => {
            t!("Review sync activity", "동기화 활동 확인", "同期履歴を確認")
        }
    }
}

pub fn audit_action_label(action: SyncAuditAction) -> &'static str {
    match action {
        SyncAuditAction::Setup => t!(
            "Personal sync was set up",
            "개인 동기화를 설정했습니다",
            "個人データ同期を設定しました"
        ),
        SyncAuditAction::ManualSync => t!(
            "Personal data was synced",
            "개인 데이터를 동기화했습니다",
            "個人データを同期しました"
        ),
        SyncAuditAction::PairCreate => t!(
            "A device connection was started",
            "기기 연결을 시작했습니다",
            "デバイス接続を開始しました"
        ),
        SyncAuditAction::PairJoin => t!(
            "This device joined personal sync",
            "이 기기를 개인 동기화에 연결했습니다",
            "このデバイスが個人データ同期に参加しました"
        ),
        SyncAuditAction::RevokeDevice => t!(
            "A device was removed",
            "기기를 제거했습니다",
            "デバイスを削除しました"
        ),
        SyncAuditAction::RecoveryExport => t!(
            "A recovery kit was saved",
            "복구 키트를 저장했습니다",
            "復旧キットを保存しました"
        ),
    }
}

pub fn audit_outcome_label(outcome: SyncAuditOutcome) -> &'static str {
    match outcome {
        SyncAuditOutcome::Succeeded => t!("Completed", "완료", "完了"),
        SyncAuditOutcome::NoChanges => t!("No changes", "변경 없음", "変更なし"),
        SyncAuditOutcome::Failed => t!("Needs attention", "확인 필요", "確認が必要"),
    }
}

fn safe_one_line(value: &str, max_chars: usize, fallback: &'static str) -> String {
    let value: String = value
        .chars()
        .filter(|character| !DeviceRecord::is_forbidden_name_char(*character))
        .take(max_chars)
        .collect();
    let value = value.trim();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;

    #[test]
    fn labels_are_localized_for_all_public_sync_states() {
        let _guard = crate::i18n::lock_for_test();
        for language in [Language::English, Language::Korean, Language::Japanese] {
            crate::i18n::set_language(language);
            for state in [
                SyncHealthState::Off,
                SyncHealthState::UpToDate,
                SyncHealthState::Syncing,
                SyncHealthState::OfflineWillRetry,
                SyncHealthState::NeedsAttention,
            ] {
                assert!(!health_label(state).is_empty());
            }
            for failure in [
                SyncFailureKind::Authentication,
                SyncFailureKind::Certificate,
                SyncFailureKind::Offline,
                SyncFailureKind::RemoteChanged,
                SyncFailureKind::DeviceApproval,
                SyncFailureKind::InvalidRemoteData,
                SyncFailureKind::LocalStateChanged,
                SyncFailureKind::Storage,
            ] {
                assert!(!failure_label(failure).is_empty());
                assert!(!failure_recovery_label(failure).is_empty());
            }
            for action in [
                SyncRowAction::ResumeConnection,
                SyncRowAction::ReenterConnection,
                SyncRowAction::DiscardConnection,
            ] {
                assert!(!action.label().is_empty());
            }
            assert!(!SyncNotice::NeedsCleanup.label().is_empty());
        }
    }

    #[test]
    fn device_rows_are_bounded_single_line_display_values() {
        let row = SyncDeviceRow::new(" living\nroom\u{7} ", &"a".repeat(120), false, true);
        assert_eq!(row.name(), "livingroom");
        assert_eq!(row.fingerprint().chars().count(), 96);
        assert!(!row.name().chars().any(char::is_control));
    }

    #[test]
    fn device_rows_remove_every_forbidden_format_character_and_controls() {
        for character in [
            '\u{061c}', '\u{200b}', '\u{200c}', '\u{200d}', '\u{200e}', '\u{200f}', '\u{202a}',
            '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
            '\u{2069}', '\u{feff}', '\0', '\n', '\u{007f}', '\u{0085}',
        ] {
            let row =
                SyncDeviceRow::new(&format!("safe{character}name"), "fingerprint", false, true);
            assert_eq!(
                row.name(),
                "safename",
                "U+{:04X} must be removed",
                u32::from(character)
            );
        }
    }

    #[test]
    fn default_model_contains_no_free_form_values() {
        let model = SyncSettingsModel::default();
        assert_eq!(model.health, SyncHealthState::Off);
        assert!(
            model
                .rows
                .iter()
                .all(|row| matches!(row, SyncRow::Action(_)))
        );
        assert_eq!(model.selected(), Some(0));
    }
}
