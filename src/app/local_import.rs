//! Local Deck import session rows and playback helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::local_format::*;
use super::local_import_helpers::*;
use super::*;
use crate::t;
use crate::transfer::checkpoint::Checkpoint;
use crate::transfer::download_plan::{
    ImportDownloadDecision, ImportDownloadDedupeIndex, build_import_download_plan,
};
use crate::transfer::organize_plan::{
    ImportOrganizeDecision, ImportOrganizeOptions, apply_import_organize_plan,
    build_import_organize_plan,
};
use crate::transfer::session::{ImportSession, ImportSessionRow, ImportSessionRowStatus};

impl App {
    pub(in crate::app) fn local_import_session_rows_for_query(
        &self,
        query: &str,
    ) -> Vec<crate::local::LocalRowId> {
        let mut sessions = BTreeMap::<
            String,
            (
                usize,
                i64,
                Option<crate::transfer::session::ImportSessionSummary>,
            ),
        >::new();
        for track in self.local_mode.index.index.tracks() {
            let Some(session_id) = track
                .import_session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let entry =
                sessions
                    .entry(session_id.to_owned())
                    .or_insert((0, track.modified_at, None));
            entry.0 += 1;
            entry.1 = entry.1.max(track.modified_at);
        }
        for summary in ImportSession::list_all() {
            let entry =
                sessions
                    .entry(summary.session_id.clone())
                    .or_insert((0, summary.updated_at, None));
            entry.1 = entry.1.max(summary.updated_at);
            entry.2 = Some(summary);
        }
        let mut rows: Vec<_> = sessions.into_iter().collect();
        rows.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
        rows.into_iter()
            .filter(|(session_id, (count, _, summary))| {
                let count = count.to_string();
                let total = summary
                    .as_ref()
                    .map(|summary| summary.counts.total.to_string())
                    .unwrap_or_default();
                let review = summary
                    .as_ref()
                    .map(|summary| summary.counts.ambiguous.to_string())
                    .unwrap_or_default();
                let missing = summary
                    .as_ref()
                    .map(|summary| summary.counts.not_found.to_string())
                    .unwrap_or_default();
                crate::local::query::fields_match_query(
                    [
                        session_id.as_str(),
                        count.as_str(),
                        total.as_str(),
                        review.as_str(),
                        missing.as_str(),
                    ],
                    query,
                )
            })
            .map(|(session_id, _)| crate::local::LocalRowId::ImportSession(session_id))
            .collect()
    }

    pub(in crate::app) fn local_import_session_drill_rows(
        &self,
        session_id: &str,
        query: &str,
    ) -> Vec<crate::local::LocalRowId> {
        if let Ok(mut session) = load_import_session_recovering(session_id) {
            session.rows.sort_by(|a, b| {
                a.source_order
                    .cmp(&b.source_order)
                    .then_with(|| a.row_id.cmp(&b.row_id))
            });
            return session
                .rows
                .into_iter()
                .filter(|row| import_session_row_matches_query(session_id, row, query))
                .map(|row| crate::local::LocalRowId::ImportSessionRow {
                    session_id: session_id.to_owned(),
                    source_order: row.source_order,
                })
                .collect();
        }

        self.local_tracks_for_import_session(session_id)
            .into_iter()
            .filter(|track| crate::local::query::track_matches_filter(track, query))
            .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
            .collect()
    }

    pub(in crate::app) fn local_inbox_rows_for_query(
        &self,
        query: &str,
    ) -> Vec<crate::local::LocalRowId> {
        let mut rows = Vec::<(i64, String, u32)>::new();
        for summary in ImportSession::list_all() {
            let Ok(mut session) = load_import_session_recovering(&summary.session_id) else {
                continue;
            };
            session.rows.sort_by(|a, b| {
                a.source_order
                    .cmp(&b.source_order)
                    .then_with(|| a.row_id.cmp(&b.row_id))
            });
            for row in session.rows {
                if import_session_row_needs_inbox_attention(&row)
                    && import_session_row_matches_query(&summary.session_id, &row, query)
                {
                    rows.push((
                        summary.updated_at,
                        summary.session_id.clone(),
                        row.source_order,
                    ));
                }
            }
        }
        rows.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
        });
        rows.into_iter()
            .map(
                |(_, session_id, source_order)| crate::local::LocalRowId::ImportSessionRow {
                    session_id,
                    source_order,
                },
            )
            .collect()
    }

    pub(in crate::app) fn local_tracks_for_import_session(
        &self,
        session_id: &str,
    ) -> Vec<&crate::local::LocalTrack> {
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| track.import_session_id.as_deref() == Some(session_id))
            .collect();
        tracks.sort_by(|a, b| {
            a.import_source_order
                .unwrap_or(u32::MAX)
                .cmp(&b.import_source_order.unwrap_or(u32::MAX))
                .then_with(|| a.path.cmp(&b.path))
        });
        tracks
    }

    pub(in crate::app) fn local_import_session_row_text(
        &self,
        session_id: &str,
        source_order: u32,
    ) -> String {
        let Some(row) = load_import_session_row(session_id, source_order) else {
            return t!("Missing import row", "없는 임포트 행").to_owned();
        };
        let artist = import_session_row_artist(&row);
        let status = import_session_row_status_label(&row);
        let title = row.title.trim();
        if title.is_empty() {
            format!("#{source_order} {status} - {artist}")
        } else {
            format!("#{source_order} {status} {title} - {artist}")
        }
    }

    pub(crate) fn local_import_action_hint(&self) -> Option<String> {
        let row = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()?;
        match row {
            crate::local::LocalRowId::ImportSession(session_id) => {
                let failed = import_session_failed_download_count(&session_id).unwrap_or_default();
                let mut actions = vec![t!("Enter rows", "Enter 행 보기")];
                if failed > 0 {
                    actions.push(t!("r retry failed", "r 실패 재시도"));
                }
                if load_import_session_recovering(&session_id)
                    .ok()
                    .is_some_and(|session| {
                        import_session_accept_all_candidate_count(&session)
                            + import_session_unwritten_ready_count(&session)
                            > 0
                    })
                {
                    actions.push(t!("A accept & write", "A 수락 및 작성"));
                }
                actions.push(t!("d download accepted", "d 수락 곡 다운로드"));
                actions.push(t!("m commit inbox", "m 인박스 커밋"));
                Some(actions.join("  |  "))
            }
            crate::local::LocalRowId::ImportSessionRow {
                session_id,
                source_order,
            } => {
                let row = load_import_session_row(&session_id, source_order)?;
                let mut actions = Vec::new();
                if import_session_row_accepts_manual_review_action(&row) {
                    actions.extend([
                        t!("a accept", "a 수락"),
                        t!("r reject", "r 거부"),
                        t!("c candidate", "c 후보"),
                        t!("x skip", "x 건너뜀"),
                    ]);
                }
                if load_import_session_recovering(&session_id)
                    .ok()
                    .is_some_and(|session| {
                        import_session_accept_all_candidate_count(&session)
                            + import_session_unwritten_ready_count(&session)
                            > 0
                    })
                {
                    actions.push(t!("A accept & write", "A 수락 및 작성"));
                }
                if !row.errors.is_empty() {
                    actions.push(t!("r retry failed", "r 실패 재시도"));
                }
                if import_session_row_is_download_accepted(&row) && row.local_path.is_none() {
                    actions.push(t!("d download", "d 다운로드"));
                }
                if import_session_row_candidate_url_key(&row).is_some() {
                    actions.push(t!("o open candidate", "o 후보 열기"));
                }
                actions.push(t!("s search", "s 검색"));
                if row.local_path.as_deref().is_some_and(path_is_import_inbox) {
                    actions.push(t!("m commit", "m 커밋"));
                }
                (!actions.is_empty()).then(|| actions.join("  |  "))
            }
            _ => None,
        }
    }

    pub(in crate::app) fn push_import_session_row_details(
        &self,
        lines: &mut Vec<String>,
        session_id: &str,
        source_order: u32,
    ) {
        let Some((session, row)) = load_import_session_and_row(session_id, source_order) else {
            push_detail_line(lines, t!("Import session", "임포트 세션"), session_id);
            push_detail_line(lines, t!("Row", "행"), format!("#{source_order}"));
            return;
        };
        push_detail_line(lines, t!("Import session", "임포트 세션"), session_id);
        push_detail_line(lines, t!("Row", "행"), format!("#{source_order}"));
        push_detail_line(
            lines,
            t!("Status", "상태"),
            import_session_row_status_label(&row),
        );
        if let Some(detail) = import_session_row_status_detail(&row) {
            push_detail_line(lines, t!("Status detail", "상태 설명"), detail);
        }
        push_detail_line(lines, t!("Title", "제목"), row.title.clone());
        push_detail_line(
            lines,
            t!("Artist", "아티스트"),
            import_session_row_artist(&row),
        );
        if let Some(album) = row.album.clone() {
            push_detail_line(lines, t!("Album", "앨범"), album);
        }
        if !row.album_artists.is_empty() {
            push_detail_line(
                lines,
                t!("Album artist", "앨범 아티스트"),
                row.album_artists.join(", "),
            );
        }
        if let Some(release_date) = row.album_release_date.clone() {
            push_detail_line(lines, t!("Release date", "발매일"), release_date);
        }
        if let Some(number) = format_disc_track(row.disc_number, row.track_number) {
            push_detail_line(lines, t!("Track", "트랙"), number);
        }
        if let Some(duration) = row.duration_secs {
            push_detail_line(
                lines,
                t!("Duration", "길이"),
                format_local_duration_ms(u64::from(duration) * 1000),
            );
        }
        if let Some(isrc) = row.isrc.clone() {
            push_detail_line(lines, "ISRC", isrc);
        }
        if let Some(explicit) = row.explicit {
            push_detail_line(lines, t!("Explicit", "Explicit"), yes_no(explicit));
        }
        push_detail_line(lines, t!("Source", "원본"), row.source_key.clone());
        if let Some(url) = row
            .source_url
            .as_deref()
            .filter(|url| *url != row.source_key)
        {
            push_detail_line(lines, t!("Source URL", "원본 URL"), url);
        }
        if let Some(display) = row.selected_display.clone() {
            push_detail_line(lines, t!("Selected", "선택"), display);
        } else if let Some(key) = row.selected_key.clone() {
            push_detail_line(lines, t!("Selected", "선택"), key);
        }
        if let Some(score) = import_session_row_selected_score(&row) {
            push_detail_line(lines, t!("Score", "점수"), format_score(score));
        }
        if matches!(row.status, ImportSessionRowStatus::NotFound) && !row.search_queries.is_empty()
        {
            push_detail_line(
                lines,
                t!("Tried queries", "시도한 검색어"),
                row.search_queries.join(" | "),
            );
        }
        if let Some(reason) = row.reject_reason.clone() {
            push_detail_line(lines, t!("Top rejection", "주요 거부 이유"), reason);
        }
        push_detail_line(
            lines,
            t!("Decision", "결정"),
            import_session_review_decision_label(row.review_decision.as_ref()),
        );
        push_detail_line(
            lines,
            t!("Download", "다운로드"),
            import_session_download_label(&row),
        );
        for (index, candidate) in row.candidates.iter().take(5).enumerate() {
            let number = index + 1;
            push_detail_line(
                lines,
                &format!("Candidate {number}"),
                format_candidate(candidate),
            );
            if let Some(breakdown) = candidate.score_breakdown.as_ref() {
                push_detail_line(
                    lines,
                    &format!("Score detail {number}"),
                    format_score_breakdown(breakdown),
                );
            }
        }
        if row.candidates.len() > 5 {
            push_detail_line(
                lines,
                t!("Candidates", "후보"),
                format!("+{} more", row.candidates.len() - 5),
            );
        }
        if let Some(path) = row.local_path.clone() {
            push_detail_line(lines, t!("Path", "경로"), path.display().to_string());
        }
        self.push_import_session_organize_preview(lines, &session, source_order);
        for warning in &row.warnings {
            push_detail_line(lines, t!("Warning", "경고"), warning);
        }
        for error in &row.errors {
            push_detail_line(lines, t!("Error", "오류"), error);
        }
    }

    pub(in crate::app) fn import_session_download_songs(
        &self,
        session_id: &str,
        source_order: Option<u32>,
    ) -> Vec<Song> {
        let Ok(mut session) = load_import_session_recovering(session_id) else {
            return Vec::new();
        };
        session.rows.sort_by(|a, b| {
            a.source_order
                .cmp(&b.source_order)
                .then_with(|| a.row_id.cmp(&b.row_id))
        });
        let existing = self.import_download_dedupe_index();
        let plan = build_import_download_plan(&session, &existing);
        let enqueue_orders: BTreeSet<_> = plan
            .rows
            .into_iter()
            .filter(|row| matches!(row.decision, ImportDownloadDecision::Enqueue))
            .filter(|row| source_order.is_none_or(|wanted| wanted == row.source_order))
            .map(|row| row.source_order)
            .collect();
        session
            .rows
            .iter()
            .filter(|row| enqueue_orders.contains(&row.source_order))
            .filter_map(|row| remote_song_from_import_session_row(&session.session_id, row))
            .collect()
    }

    pub(in crate::app) fn local_retry_failed_import_downloads(&mut self) -> Option<Vec<Cmd>> {
        let row = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()?;
        let (session_id, source_order, failed_count) = match row {
            crate::local::LocalRowId::ImportSession(session_id) => {
                let failed_count = import_session_failed_download_count(&session_id)?;
                if failed_count == 0 {
                    return None;
                }
                (session_id, None, failed_count)
            }
            crate::local::LocalRowId::ImportSessionRow {
                session_id,
                source_order,
            } => {
                let row = load_import_session_row(&session_id, source_order)?;
                if row.errors.is_empty() {
                    return None;
                }
                (session_id, Some(source_order), 1)
            }
            _ => return None,
        };
        let songs = self.import_session_failed_download_songs(&session_id, source_order);
        if songs.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {failed_count}",
                t!(
                    "No failed import downloads can be retried",
                    "재시도할 수 있는 실패 다운로드 없음"
                )
            );
            self.dirty = true;
            return Some(Vec::new());
        }
        Some(self.start_or_confirm_local_download(songs))
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
        self.mode = Mode::Search;
        self.dropdowns.search_source_open = false;
        self.search_filter.close();
        self.search.input = query;
        self.search.focus = SearchFocus::Input;
        self.search.kind = SearchKind::Songs;
        self.search.source = crate::search_source::SearchSource::Youtube;
        Some(self.submit_search_query())
    }

    pub(in crate::app) fn local_open_selected_import_candidate_url(&mut self) -> bool {
        let Some(row) = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()
        else {
            return false;
        };
        let crate::local::LocalRowId::ImportSessionRow {
            session_id,
            source_order,
        } = row
        else {
            return false;
        };
        let Some(row) = load_import_session_row(&session_id, source_order) else {
            return false;
        };
        let Some(key) = import_session_row_candidate_url_key(&row) else {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Import row has no candidate URL",
                "열 후보 URL이 없는 임포트 행"
            )
            .to_owned();
            self.dirty = true;
            return true;
        };
        let Some(url) = youtube_watch_url_for_candidate(key) else {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "Import candidate key is not a YouTube video id",
                "임포트 후보 키가 YouTube 동영상 ID가 아님"
            )
            .to_owned();
            self.dirty = true;
            return true;
        };
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{}: {url}",
            t!("Opening import candidate", "임포트 후보 열기")
        );
        #[cfg(not(test))]
        open_in_browser(&url);
        self.dirty = true;
        true
    }

    fn import_session_failed_download_songs(
        &self,
        session_id: &str,
        source_order: Option<u32>,
    ) -> Vec<Song> {
        let Ok(mut session) = load_import_session_recovering(session_id) else {
            return Vec::new();
        };
        session.rows.sort_by(|a, b| {
            a.source_order
                .cmp(&b.source_order)
                .then_with(|| a.row_id.cmp(&b.row_id))
        });
        let existing = self.import_download_dedupe_index();
        let plan = build_import_download_plan(&session, &existing);
        let retry_orders: BTreeSet<_> = plan
            .rows
            .into_iter()
            .filter(|row| matches!(row.decision, ImportDownloadDecision::Enqueue))
            .filter(|row| source_order.is_none_or(|wanted| wanted == row.source_order))
            .filter(|planned| {
                session
                    .rows
                    .iter()
                    .find(|row| row.source_order == planned.source_order)
                    .is_some_and(|row| !row.errors.is_empty())
            })
            .map(|row| row.source_order)
            .collect();
        session
            .rows
            .iter()
            .filter(|row| retry_orders.contains(&row.source_order))
            .filter_map(|row| remote_song_from_import_session_row(&session.session_id, row))
            .collect()
    }

    pub(in crate::app) fn request_local_accept_selected_import_candidate(
        &mut self,
    ) -> Option<Vec<Cmd>> {
        self.request_local_import_review_action(ImportReviewAction::AcceptFirst)
    }

    pub(in crate::app) fn request_local_choose_next_import_candidate(
        &mut self,
    ) -> Option<Vec<Cmd>> {
        self.request_local_import_review_action(ImportReviewAction::ChooseNext)
    }

    pub(in crate::app) fn request_local_reject_selected_import_row(&mut self) -> Option<Vec<Cmd>> {
        self.request_local_import_review_action(ImportReviewAction::Reject)
    }

    pub(in crate::app) fn request_local_skip_selected_import_row(&mut self) -> Option<Vec<Cmd>> {
        self.request_local_import_review_action(ImportReviewAction::Skip)
    }

    fn request_local_import_review_action(
        &mut self,
        action: ImportReviewAction,
    ) -> Option<Vec<Cmd>> {
        let (session_id, source_order) = self.selected_manual_review_import_row()?;
        if self
            .local_mode
            .pending_import_reviews
            .contains_key(&session_id)
        {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {session_id}",
                t!("Import review already in progress", "임포트 검토 진행 중")
            );
            self.dirty = true;
            return Some(Vec::new());
        }
        let op_id = self.next_local_import_review_op_id();
        self.local_mode
            .pending_import_reviews
            .insert(session_id.clone(), op_id);
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{} #{}...",
            import_review_action_progress_label(action),
            source_order
        );
        self.dirty = true;
        Some(vec![Cmd::Local(LocalCmd::ReviewImport {
            op_id,
            session_id,
            source_order,
            action,
        })])
    }

    pub(in crate::app) fn request_local_import_accept_all(&mut self) -> Option<Vec<Cmd>> {
        let session_id = self.selected_import_session_id_for_organize()?;
        if self
            .local_mode
            .pending_import_reviews
            .contains_key(&session_id)
        {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {session_id}",
                t!("Import review already in progress", "임포트 검토 진행 중")
            );
            self.dirty = true;
            return Some(Vec::new());
        }
        let Ok(session) = load_import_session_recovering(&session_id) else {
            self.status.kind = StatusKind::Error;
            self.status.text = format!(
                "{}: {session_id}",
                t!("Import session not found", "임포트 세션을 찾을 수 없음")
            );
            self.dirty = true;
            return Some(Vec::new());
        };
        let candidate_count = import_session_accept_all_candidate_count(&session);
        let ready_count = import_session_unwritten_ready_count(&session) + candidate_count;
        if candidate_count == 0 && ready_count == 0 {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "No import candidates or ready rows to write",
                "수락할 후보나 작성할 준비 행이 없음"
            )
            .to_owned();
            self.dirty = true;
            return Some(Vec::new());
        }
        self.local_mode.pending_accept_all_confirm = Some(LocalImportAcceptAllConfirm {
            session_id,
            candidate_count,
            ready_count,
            review_left: session.counts.ambiguous.saturating_sub(candidate_count),
            missing_left: session.counts.not_found,
        });
        self.status.kind = StatusKind::Info;
        self.status.text = t!(
            "Confirm accepting and writing the import",
            "임포트 수락 및 작성을 확인하세요"
        )
        .to_owned();
        self.dirty = true;
        Some(Vec::new())
    }

    pub(in crate::app) fn apply_local_import_accept_all_confirm(
        &mut self,
        confirm: LocalImportAcceptAllConfirm,
    ) -> Vec<Cmd> {
        self.local_mode.pending_accept_all_confirm = None;
        if self
            .local_mode
            .pending_import_reviews
            .contains_key(&confirm.session_id)
        {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {}",
                t!("Import review already in progress", "임포트 검토 진행 중"),
                confirm.session_id
            );
            self.dirty = true;
            return Vec::new();
        }
        let op_id = self.next_local_import_review_op_id();
        self.local_mode
            .pending_import_reviews
            .insert(confirm.session_id.clone(), op_id);
        self.status.kind = StatusKind::Info;
        if confirm.candidate_count == 0 {
            self.local_mode
                .pending_import_reviews
                .remove(&confirm.session_id);
            self.local_mode
                .pending_accept_write_summaries
                .insert(confirm.session_id.clone(), 0);
            self.transfer_running = true;
            self.status.text = t!(
                "Writing accepted import rows to Library playlist...",
                "수락된 임포트 행을 Library 플레이리스트에 쓰는 중..."
            )
            .to_owned();
            self.dirty = true;
            return vec![Cmd::Transfer(
                crate::transfer::actor::TransferCmd::WriteReviewedLocal {
                    job_id: confirm.session_id,
                },
            )];
        }
        self.status.text = format!(
            "{} {}...",
            t!("Accepting import candidates", "임포트 후보 수락 중"),
            confirm.candidate_count
        );
        self.dirty = true;
        vec![Cmd::Local(LocalCmd::ReviewImportAcceptAll {
            op_id,
            session_id: confirm.session_id,
        })]
    }

    pub(in crate::app) fn apply_local_import_review_finished(
        &mut self,
        op_id: u64,
        session_id: String,
        action: ImportReviewAction,
        result: Result<crate::transfer::review_action::ReviewActionSummary, String>,
    ) -> Vec<Cmd> {
        if self
            .local_mode
            .pending_import_reviews
            .get(&session_id)
            .copied()
            != Some(op_id)
        {
            return Vec::new();
        }
        self.local_mode.pending_import_reviews.remove(&session_id);
        match result {
            Ok(summary) => {
                self.status.kind = StatusKind::Info;
                self.status.text = import_review_success_text(action, &summary);
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error}",
                    t!("Import review failed", "임포트 검토 실패")
                );
            }
        }
        self.clamp_local_after_import_change();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn apply_local_import_accept_all_finished(
        &mut self,
        op_id: u64,
        session_id: String,
        result: Result<crate::transfer::review_action::ReviewBatchSummary, String>,
    ) -> Vec<Cmd> {
        if self
            .local_mode
            .pending_import_reviews
            .get(&session_id)
            .copied()
            != Some(op_id)
        {
            return Vec::new();
        }
        self.local_mode.pending_import_reviews.remove(&session_id);
        match result {
            Ok(summary) => {
                self.status.kind = StatusKind::Info;
                self.local_mode
                    .pending_accept_write_summaries
                    .insert(session_id.clone(), summary.accepted_count);
                self.transfer_running = true;
                self.status.text = if crate::i18n::is_korean() {
                    format!(
                        "임포트 후보 {}개 수락 · Library 플레이리스트 작성 중...",
                        summary.accepted_count
                    )
                } else {
                    format!(
                        "Accepted {} import candidate{} · writing Library playlist...",
                        summary.accepted_count,
                        if summary.accepted_count == 1 { "" } else { "s" }
                    )
                };
                self.clamp_local_after_import_change();
                self.dirty = true;
                return vec![Cmd::Transfer(
                    crate::transfer::actor::TransferCmd::WriteReviewedLocal { job_id: session_id },
                )];
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error}",
                    t!("Import review failed", "임포트 검토 실패")
                );
            }
        }
        self.clamp_local_after_import_change();
        self.dirty = true;
        Vec::new()
    }

    fn next_local_import_review_op_id(&mut self) -> u64 {
        self.local_mode.next_import_review_op_id =
            self.local_mode.next_import_review_op_id.wrapping_add(1);
        self.local_mode.next_import_review_op_id
    }

    fn clamp_local_after_import_change(&mut self) {
        self.local_mode.ui.selected = self
            .local_mode
            .ui
            .selected
            .min(self.local_rows_len().saturating_sub(1));
        self.local_mode.ui.anchor = self.local_mode.ui.selected;
    }

    pub(in crate::app) fn request_local_import_organize_apply(&mut self) -> Vec<Cmd> {
        let Some(session_id) = self.selected_import_session_id_for_organize() else {
            self.status.kind = StatusKind::Info;
            self.status.text = t!(
                "Select an import session or inbox row to organize",
                "정리할 임포트 세션 또는 인박스 행을 선택하세요"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        };
        let Ok(session) = load_import_session_recovering(&session_id) else {
            self.status.kind = StatusKind::Error;
            self.status.text = format!(
                "{}: {session_id}",
                t!("Import session not found", "임포트 세션을 찾을 수 없음")
            );
            self.dirty = true;
            return Vec::new();
        };
        let plan = match build_import_organize_plan(&session, &self.import_organize_options()) {
            Ok(plan) => plan,
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}",
                    t!("Import organize failed", "임포트 정리 실패")
                );
                self.dirty = true;
                return Vec::new();
            }
        };
        if plan.move_count == 0 {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {} {} / {} {}",
                t!("Nothing to organize", "정리할 항목 없음"),
                plan.already_count,
                t!("already", "이미 정리됨"),
                plan.skipped_count,
                t!("skipped", "건너뜀")
            );
            self.dirty = true;
            return Vec::new();
        }
        self.local_mode.pending_organize_confirm = Some(LocalOrganizeConfirm {
            session_id: plan.session_id,
            root: plan.root,
            move_count: plan.move_count,
            already_count: plan.already_count,
            skipped_count: plan.skipped_count,
        });
        self.status.kind = StatusKind::Info;
        self.status.text = t!(
            "Confirm import organize to move inbox files",
            "인박스 파일 이동을 확인하세요"
        )
        .to_owned();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn apply_local_import_organize_confirm(
        &mut self,
        confirm: LocalOrganizeConfirm,
    ) -> Vec<Cmd> {
        self.local_mode.pending_organize_confirm = None;
        let result = (|| {
            let session = load_import_session_recovering(&confirm.session_id)?;
            let plan = build_import_organize_plan(
                &session,
                &ImportOrganizeOptions {
                    root: confirm.root.clone(),
                    template: self.config.local.import_path_template().to_owned(),
                },
            )?;
            apply_import_organize_plan(&plan)
        })();
        match result {
            Ok(report) => {
                let cmds = self.request_local_scan(false);
                self.status.kind = StatusKind::Info;
                self.status.text = format!(
                    "{} {}: {} {}, {} {}, {} {}",
                    t!("Organized import session", "임포트 세션 정리됨"),
                    confirm.session_id,
                    report.moved_count,
                    t!("moved", "이동됨"),
                    report.already_count,
                    t!("already", "이미 정리됨"),
                    report.skipped_count,
                    t!("skipped", "건너뜀")
                );
                self.local_mode.ui.selected = self
                    .local_mode
                    .ui
                    .selected
                    .min(self.local_rows_len().saturating_sub(1));
                self.local_mode.ui.anchor = self.local_mode.ui.selected;
                self.dirty = true;
                cmds
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}",
                    t!("Import organize failed", "임포트 정리 실패")
                );
                self.dirty = true;
                Vec::new()
            }
        }
    }

    fn selected_manual_review_import_row(&self) -> Option<(String, u32)> {
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
        import_session_row_accepts_manual_review_action(&row).then_some((session_id, source_order))
    }

    fn selected_import_session_id_for_organize(&self) -> Option<String> {
        match self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()?
        {
            crate::local::LocalRowId::ImportSession(session_id)
            | crate::local::LocalRowId::ImportSessionRow { session_id, .. } => Some(session_id),
            _ => None,
        }
    }

    fn import_download_dedupe_index(&self) -> ImportDownloadDedupeIndex {
        let mut existing = ImportDownloadDedupeIndex::from_download_store(&self.download_store);
        for song in &self.library_ui.downloaded {
            existing.add_downloaded_song(song);
        }
        existing.add_local_index(&self.local_mode.index.index);
        existing
    }

    fn push_import_session_organize_preview(
        &self,
        lines: &mut Vec<String>,
        session: &ImportSession,
        source_order: u32,
    ) {
        let options = self.import_organize_options();
        let Ok(plan) = build_import_organize_plan(session, &options) else {
            return;
        };
        let Some(row) = plan
            .rows
            .iter()
            .find(|row| row.source_order == source_order)
        else {
            return;
        };
        match row.decision {
            ImportOrganizeDecision::Move => {
                if let Some(target) = &row.target_path {
                    push_detail_line(lines, t!("Target", "대상"), target.display().to_string());
                }
            }
            ImportOrganizeDecision::AlreadyAtTarget => {
                push_detail_line(
                    lines,
                    t!("Target", "대상"),
                    t!("already organized", "이미 정리됨"),
                );
            }
            ImportOrganizeDecision::NotAccepted | ImportOrganizeDecision::MissingLocalPath => {}
        }
        for warning in &row.warnings {
            push_detail_line(lines, t!("Organize warning", "정리 경고"), warning);
        }
    }

    fn import_organize_options(&self) -> ImportOrganizeOptions {
        ImportOrganizeOptions {
            root: self.import_organize_root(),
            template: self.config.local.import_path_template().to_owned(),
        }
    }

    fn import_organize_root(&self) -> PathBuf {
        self.config
            .local
            .roots
            .iter()
            .find(|root| root.enabled())
            .and_then(|root| root.normalized_path())
            .unwrap_or_else(|| self.config.effective_download_dir())
    }

    pub(in crate::app) fn import_session_row_song(
        &self,
        session_id: &str,
        source_order: u32,
    ) -> Option<Song> {
        if let Some(track) = self
            .local_tracks_for_import_session(session_id)
            .into_iter()
            .find(|track| track.import_source_order == Some(source_order))
        {
            return Some(track.to_song());
        }

        let row = load_import_session_row(session_id, source_order)?;
        let path = row.local_path.clone()?;
        Some(song_from_import_session_row(session_id, &row, path))
    }

    pub(in crate::app) fn import_session_songs(&self, session_id: &str) -> Vec<Song> {
        if let Ok(mut session) = load_import_session_recovering(session_id) {
            session.rows.sort_by(|a, b| {
                a.source_order
                    .cmp(&b.source_order)
                    .then_with(|| a.row_id.cmp(&b.row_id))
            });
            return session
                .rows
                .into_iter()
                .filter_map(|row| self.import_session_row_song(session_id, row.source_order))
                .collect();
        }
        self.local_tracks_for_import_session(session_id)
            .into_iter()
            .map(|track| track.to_song())
            .collect()
    }
}

fn load_import_session_recovering(session_id: &str) -> anyhow::Result<ImportSession> {
    match ImportSession::load(session_id) {
        Ok(session) => Ok(session),
        Err(session_error) => {
            let cp = Checkpoint::load(session_id).map_err(|checkpoint_error| {
                anyhow::anyhow!(
                    "load import session `{session_id}` failed ({session_error:#}); checkpoint recovery failed ({checkpoint_error:#})"
                )
            })?;
            let session = ImportSession::from_checkpoint(&cp);
            session.save().map_err(|save_error| {
                anyhow::anyhow!(
                    "recover import session `{session_id}` from checkpoint failed while saving: {save_error}"
                )
            })?;
            tracing::warn!(
                session_id,
                error = %session_error,
                "recovered import session from checkpoint"
            );
            Ok(session)
        }
    }
}

fn load_import_session_row(session_id: &str, source_order: u32) -> Option<ImportSessionRow> {
    load_import_session_recovering(session_id)
        .ok()?
        .rows
        .into_iter()
        .find(|row| row.source_order == source_order)
}

fn load_import_session_and_row(
    session_id: &str,
    source_order: u32,
) -> Option<(ImportSession, ImportSessionRow)> {
    let session = load_import_session_recovering(session_id).ok()?;
    let row = session
        .rows
        .iter()
        .find(|row| row.source_order == source_order)
        .cloned()?;
    Some((session, row))
}

fn import_session_failed_download_count(session_id: &str) -> Option<usize> {
    Some(
        load_import_session_recovering(session_id)
            .ok()?
            .rows
            .into_iter()
            .filter(|row| !row.errors.is_empty())
            .count(),
    )
}

fn song_from_import_session_row(session_id: &str, row: &ImportSessionRow, path: PathBuf) -> Song {
    let mut song = Song::local_file(path);
    if !row.title.trim().is_empty() {
        song.title = row.title.clone();
    }
    let artist = import_session_row_artist(row);
    if !artist.trim().is_empty() {
        song.artist = artist;
    }
    song.album = row.album.clone();
    song.duration_secs = row.duration_secs;
    song.duration = row
        .duration_secs
        .map(|secs| format_local_duration_ms(u64::from(secs) * 1000))
        .unwrap_or_default();
    let album_artist = (!row.album_artists.is_empty()).then(|| row.album_artists.join(", "));
    song = song
        .with_catalog_metadata(
            album_artist,
            row.disc_number,
            row.track_number,
            row.isrc.clone(),
            Some(row.source_key.clone()),
            row.source_url.clone(),
        )
        .with_import_metadata(crate::api::SongImportMetadata {
            artists: row.artists.clone(),
            album_artists: row.album_artists.clone(),
            album_release_date: row.album_release_date.clone(),
            album_release_date_precision: row.album_release_date_precision.clone(),
            album_total_tracks: row.album_total_tracks,
            album_type: row.album_type.clone(),
            album_art_url: row.album_art_url.clone(),
            explicit: row.explicit,
        })
        .with_import_session(Some(session_id.to_owned()), Some(row.source_order));
    if let Some(key) = row.selected_key.clone() {
        song = song.with_yt_id(key);
    }
    song
}

fn remote_song_from_import_session_row(session_id: &str, row: &ImportSessionRow) -> Option<Song> {
    if row.local_path.is_some() || !import_session_row_is_download_accepted(row) {
        return None;
    }
    let selected_key = import_session_row_selected_key(row)?.to_owned();
    let title = if row.title.trim().is_empty() {
        row.selected_display
            .clone()
            .unwrap_or_else(|| selected_key.clone())
    } else {
        row.title.clone()
    };
    let artist = import_session_row_artist(row);
    let duration = row
        .duration_secs
        .map(|secs| format_local_duration_ms(u64::from(secs) * 1000))
        .unwrap_or_default();
    let mut song = Song::from_search(selected_key, title, artist, duration, row.album.clone());
    song.duration_secs = row.duration_secs;
    let album_artist = (!row.album_artists.is_empty()).then(|| row.album_artists.join(", "));
    Some(
        song.with_catalog_metadata(
            album_artist,
            row.disc_number,
            row.track_number,
            row.isrc.clone(),
            Some(row.source_key.clone()),
            row.source_url.clone(),
        )
        .with_import_metadata(crate::api::SongImportMetadata {
            artists: row.artists.clone(),
            album_artists: row.album_artists.clone(),
            album_release_date: row.album_release_date.clone(),
            album_release_date_precision: row.album_release_date_precision.clone(),
            album_total_tracks: row.album_total_tracks,
            album_type: row.album_type.clone(),
            album_art_url: row.album_art_url.clone(),
            explicit: row.explicit,
        })
        .with_import_session(Some(session_id.to_owned()), Some(row.source_order)),
    )
}

fn import_review_action_progress_label(action: ImportReviewAction) -> &'static str {
    match action {
        ImportReviewAction::AcceptFirst => {
            t!("Accepting import row", "임포트 행 수락 중")
        }
        ImportReviewAction::ChooseNext => {
            t!("Selecting import candidate", "임포트 후보 선택 중")
        }
        ImportReviewAction::Reject => t!("Rejecting import row", "임포트 행 거부 중"),
        ImportReviewAction::Skip => t!("Skipping import row", "임포트 행 건너뛰는 중"),
    }
}

fn import_review_success_text(
    action: ImportReviewAction,
    summary: &crate::transfer::review_action::ReviewActionSummary,
) -> String {
    match action {
        ImportReviewAction::AcceptFirst => match &summary.display {
            Some(display) => format!(
                "{} #{}: {display}",
                t!("Accepted import row", "임포트 행 수락"),
                summary.source_order
            ),
            None => format!(
                "{} #{}",
                t!("Accepted import row", "임포트 행 수락"),
                summary.source_order
            ),
        },
        ImportReviewAction::ChooseNext => match &summary.display {
            Some(display) => format!(
                "{} #{}: {display}",
                t!("Selected import candidate", "임포트 후보 선택"),
                summary.source_order
            ),
            None => format!(
                "{} #{}",
                t!("Selected import candidate", "임포트 후보 선택"),
                summary.source_order
            ),
        },
        ImportReviewAction::Reject => format!(
            "{} #{}",
            t!("Rejected import row", "임포트 행 거부"),
            summary.source_order
        ),
        ImportReviewAction::Skip => format!(
            "{} #{}",
            t!("Skipped import row", "임포트 행 건너뜀"),
            summary.source_order
        ),
    }
}
