//! Explicit online-search handoff for Local Deck import-session rows.

use super::local_import::load_import_session_row;
use super::local_import_helpers::import_session_manual_search_query;
use super::*;

impl App {
    pub fn local_import_search_pending(&self) -> bool {
        self.local_mode.pending_import_search.is_some()
    }

    pub(in crate::app) fn local_search_selected_import_row(&mut self) -> Option<Vec<Cmd>> {
        let row = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()?;
        let crate::local::LocalRowId::ImportSessionRow {
            session_id,
            source_order,
        } = row
        else {
            return None;
        };
        let row = load_import_session_row(&session_id, source_order)?;
        let Some(query) = import_session_manual_search_query(&row) else {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Import row has no searchable metadata",
                "검색할 임포트 메타데이터가 없음"
            )
            .to_owned();
            self.dirty = true;
            return Some(Vec::new());
        };
        let query = match crate::util::query::sanitize_query_for_submit(
            &query,
            crate::util::query::MAX_SEARCH_QUERY_BYTES,
        ) {
            Ok(query) => query,
            Err(reason) => {
                self.set_query_reject_status(reason);
                return Some(Vec::new());
            }
        };
        if !self.config.effective_search().youtube {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "Enable YouTube search before searching an import row",
                "임포트 행을 검색하려면 YouTube 검색을 먼저 켜세요"
            )
            .to_owned();
            self.dirty = true;
            return Some(Vec::new());
        }
        // Reuse the admission-safe Local exit confirmation. The confirmation renderer detects
        // this continuation and explains that the next surface is an online YouTube search.
        let effects = self.request_local_mode_switch();
        let Some(confirmation_token) = self.local_mode.pending_confirm_token else {
            return Some(effects);
        };
        self.local_mode.pending_import_search = Some(LocalImportSearchContinuation {
            confirmation_token,
            query,
            session_id,
            row_id: row.row_id,
            source_order,
            source_revision: self.local_mode.rows_revision.get(),
        });
        Some(effects)
    }

    /// Consume the manual-search handoff after Local exit has committed. A stale or changed
    /// origin fails closed; the ordinary online Search state is updated exactly once only here.
    pub(in crate::app) fn complete_local_import_search_continuation(
        &mut self,
        expected_confirmation_token: Option<u64>,
    ) -> Vec<Cmd> {
        let Some(expected_confirmation_token) = expected_confirmation_token else {
            return Vec::new();
        };
        if self
            .local_mode
            .pending_import_search
            .as_ref()
            .map(|pending| pending.confirmation_token)
            != Some(expected_confirmation_token)
        {
            return Vec::new();
        }
        let Some(pending) = self.local_mode.pending_import_search.take() else {
            return Vec::new();
        };
        let Some(row) = load_import_session_row(&pending.session_id, pending.source_order) else {
            return Vec::new();
        };
        if self.local_mode.rows_revision.get() != pending.source_revision
            || row.row_id != pending.row_id
            || import_session_manual_search_query(&row).as_deref() != Some(pending.query.as_str())
        {
            return Vec::new();
        }
        // The pre-confirmation check normally makes this invariant stable. Recheck at commit so a
        // concurrent config change can fail closed instead of silently normalizing YouTube to a
        // different provider after the user authorized only YouTube.
        if !self.config.effective_search().youtube {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "YouTube search was disabled before the confirmed search could run",
                "확인한 검색을 실행하기 전에 YouTube 검색이 꺼졌어요"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        self.mode = Mode::Search;
        self.dropdowns.search_source_open = false;
        self.search_filter.close();
        self.search.input = pending.query;
        self.search.focus = SearchFocus::Input;
        self.search.kind = SearchKind::Songs;
        self.search.source = crate::search_source::SearchSource::Youtube;
        self.submit_search_query()
    }
}
