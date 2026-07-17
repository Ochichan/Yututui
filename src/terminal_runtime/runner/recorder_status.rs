//! User-facing startup status for recorder recovery capacity gates.

pub(super) fn recorder_capacity_blocked_status(
    report: &crate::recorder::job::RecoveryReport,
) -> String {
    if report.admission_uncertain {
        let detail = report
            .warnings
            .first()
            .map(String::as_str)
            .unwrap_or("recovery inventory could not be verified");
        match crate::i18n::current() {
            crate::i18n::Language::Korean => {
                format!("자동 녹음 일시 중지: 복구 저장 목록을 확인할 수 없음 — {detail}")
            }
            crate::i18n::Language::Japanese => {
                format!("自動録音一時停止: 復旧保存リストを確認できません — {detail}")
            }
            _ => format!("Automatic recording paused: recovery inventory is uncertain — {detail}"),
        }
    } else {
        match crate::i18n::current() {
            crate::i18n::Language::Korean => format!(
                "자동 녹음 일시 중지: 저장 대기 {}개 / {}바이트",
                report.pending, report.pending_bytes
            ),
            crate::i18n::Language::Japanese => format!(
                "自動録音一時停止: 保存待ち{}件 / {}バイト",
                report.pending, report.pending_bytes
            ),
            _ => format!(
                "Automatic recording paused: {} pending / {} bytes",
                report.pending, report.pending_bytes
            ),
        }
    }
}
