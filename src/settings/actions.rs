use crate::t;

/// How a field is edited / rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text edited in-place (paths). Enter toggles edit mode.
    Text,
    /// On/off, flipped with ←/→ or Enter.
    Toggle,
    /// A value cycled through a set with ←/→.
    Select,
    /// A numeric value nudged with ←/→.
    Slider,
    /// A pressable action (no value); Enter/Confirm triggers it.
    Button,
}

/// Progress shown by the Settings personal-data export row.
///
/// Detailed success paths and errors belong in the Settings status line; this compact state
/// keeps the row useful in narrow terminals and gives the reducer one source of truth for
/// suppressing duplicate activation while an export is running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PersonalDataExportStatus {
    #[default]
    Idle,
    Exporting,
    Succeeded,
    Failed,
}

impl PersonalDataExportStatus {
    pub fn from_busy(busy: bool) -> Self {
        if busy { Self::Exporting } else { Self::Idle }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Exporting)
    }

    pub fn value_display(&self) -> String {
        match self {
            Self::Idle => t!(
                "↵ Export to Downloads",
                "↵ 다운로드 폴더로 내보내기",
                "↵ ダウンロードへエクスポート"
            )
            .to_owned(),
            Self::Exporting => t!("Exporting…", "내보내는 중…", "エクスポート中…").to_owned(),
            Self::Succeeded => t!("✓ Exported", "✓ 내보내기 완료", "✓ エクスポート完了").to_owned(),
            Self::Failed => {
                t!("Failed · ↵ retry", "실패 · ↵ 다시 시도", "失敗 · ↵ 再試行").to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;
    use crate::settings::Field;

    #[test]
    fn personal_data_export_status_is_compact_and_bilingual() {
        let _guard = crate::i18n::lock_for_test();
        assert_eq!(Field::ExportPersonalData.label(), "Export personal data");
        assert_eq!(
            PersonalDataExportStatus::Idle.value_display(),
            "↵ Export to Downloads"
        );
        assert_eq!(
            PersonalDataExportStatus::Exporting.value_display(),
            "Exporting…"
        );
        assert!(PersonalDataExportStatus::Exporting.is_busy());
        assert!(!PersonalDataExportStatus::Failed.is_busy());

        crate::i18n::set_language(Language::Korean);
        assert_eq!(Field::ExportPersonalData.label(), "개인 데이터 내보내기");
        assert_eq!(
            PersonalDataExportStatus::Idle.value_display(),
            "↵ 다운로드 폴더로 내보내기"
        );
        assert_eq!(
            PersonalDataExportStatus::Succeeded.value_display(),
            "✓ 내보내기 완료"
        );
        assert_eq!(
            PersonalDataExportStatus::Failed.value_display(),
            "실패 · ↵ 다시 시도"
        );
        crate::i18n::set_language(Language::English);
    }
}
