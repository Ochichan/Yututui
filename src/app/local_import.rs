//! Local Deck import session rows and playback helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::local_format::*;
use super::*;
use crate::t;
use crate::transfer::checkpoint::{ReportCandidate, ReviewDecision};
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
        if let Ok(mut session) = ImportSession::load(session_id) {
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
            let Ok(mut session) = ImportSession::load(&summary.session_id) else {
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
            if let Some(breakdown) = candidate.score_breakdown {
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
        let Ok(mut session) = ImportSession::load(session_id) else {
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

    pub(in crate::app) fn local_accept_selected_import_candidate(&mut self) -> bool {
        let Some((session_id, source_order)) = self.selected_manual_review_import_row() else {
            return false;
        };
        match crate::transfer::review_action::accept_first_candidate(&session_id, source_order) {
            Ok(summary) => {
                self.status.kind = StatusKind::Info;
                self.status.text = match summary.display {
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
                };
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}",
                    t!("Import review failed", "임포트 검토 실패")
                );
            }
        }
        self.dirty = true;
        true
    }

    pub(in crate::app) fn local_choose_next_import_candidate(&mut self) -> bool {
        let Some((session_id, source_order)) = self.selected_manual_review_import_row() else {
            return false;
        };
        match crate::transfer::review_action::choose_next_candidate(&session_id, source_order) {
            Ok(summary) => {
                self.status.kind = StatusKind::Info;
                self.status.text = match summary.display {
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
                };
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}",
                    t!("Import review failed", "임포트 검토 실패")
                );
            }
        }
        self.dirty = true;
        true
    }

    pub(in crate::app) fn local_reject_selected_import_row(&mut self) -> bool {
        let Some((session_id, source_order)) = self.selected_manual_review_import_row() else {
            return false;
        };
        match crate::transfer::review_action::reject_row(&session_id, source_order) {
            Ok(summary) => {
                self.status.kind = StatusKind::Info;
                self.status.text = format!(
                    "{} #{}",
                    t!("Rejected import row", "임포트 행 거부"),
                    summary.source_order
                );
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}",
                    t!("Import review failed", "임포트 검토 실패")
                );
            }
        }
        self.dirty = true;
        true
    }

    pub(in crate::app) fn local_skip_selected_import_row(&mut self) -> bool {
        let Some((session_id, source_order)) = self.selected_manual_review_import_row() else {
            return false;
        };
        match crate::transfer::review_action::skip_row(&session_id, source_order) {
            Ok(summary) => {
                self.status.kind = StatusKind::Info;
                self.status.text = format!(
                    "{} #{}",
                    t!("Skipped import row", "임포트 행 건너뜀"),
                    summary.source_order
                );
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}",
                    t!("Import review failed", "임포트 검토 실패")
                );
            }
        }
        self.dirty = true;
        true
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
        let Ok(session) = ImportSession::load(&session_id) else {
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
            let session = ImportSession::load(&confirm.session_id)?;
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
        if let Ok(mut session) = ImportSession::load(session_id) {
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

fn load_import_session_row(session_id: &str, source_order: u32) -> Option<ImportSessionRow> {
    ImportSession::load(session_id)
        .ok()?
        .rows
        .into_iter()
        .find(|row| row.source_order == source_order)
}

fn load_import_session_and_row(
    session_id: &str,
    source_order: u32,
) -> Option<(ImportSession, ImportSessionRow)> {
    let session = ImportSession::load(session_id).ok()?;
    let row = session
        .rows
        .iter()
        .find(|row| row.source_order == source_order)
        .cloned()?;
    Some((session, row))
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
        .with_import_session(Some(session_id.to_owned()), Some(row.source_order)),
    )
}

fn import_session_row_status_label(row: &ImportSessionRow) -> &'static str {
    if row.local_path.as_deref().is_some_and(path_is_import_inbox) {
        return "inbox";
    }
    if row.local_path.is_some() {
        return "local";
    }
    if !row.errors.is_empty() {
        return "failed";
    }
    match row.status {
        ImportSessionRowStatus::Pending => "pending",
        ImportSessionRowStatus::Matched => "ready",
        ImportSessionRowStatus::Ambiguous => "review",
        ImportSessionRowStatus::NotFound => "missing",
        ImportSessionRowStatus::SkippedLocal => "skipped",
    }
}

fn import_session_row_needs_inbox_attention(row: &ImportSessionRow) -> bool {
    if matches!(
        row.review_decision,
        Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
    ) {
        return false;
    }
    row.local_path.as_deref().is_some_and(path_is_import_inbox)
        || !row.errors.is_empty()
        || matches!(
            row.status,
            ImportSessionRowStatus::Pending
                | ImportSessionRowStatus::Ambiguous
                | ImportSessionRowStatus::NotFound
        )
}

fn path_is_import_inbox(path: &std::path::Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".yututui-inbox")
}

fn import_session_row_artist(row: &ImportSessionRow) -> String {
    if row.artists.is_empty() {
        t!("Local file", "로컬 파일").to_owned()
    } else {
        row.artists.join(", ")
    }
}

fn import_session_row_matches_query(session_id: &str, row: &ImportSessionRow, query: &str) -> bool {
    let source_order = row.source_order.to_string();
    let status = import_session_row_status_label(row);
    let artist = import_session_row_artist(row);
    let album = row.album.as_deref().unwrap_or_default();
    let album_artists = row.album_artists.join(" ");
    let release_date = row.album_release_date.as_deref().unwrap_or_default();
    let duration = row
        .duration_secs
        .map(|value| value.to_string())
        .unwrap_or_default();
    let isrc = row.isrc.as_deref().unwrap_or_default();
    let explicit = row.explicit.map(yes_no).unwrap_or_default();
    let source_url = row.source_url.as_deref().unwrap_or_default();
    let selected = row
        .selected_display
        .as_deref()
        .or(row.selected_key.as_deref())
        .unwrap_or_default();
    let selected_score = import_session_row_selected_score(row)
        .map(format_score)
        .unwrap_or_default();
    let decision = import_session_review_decision_label(row.review_decision.as_ref());
    let candidates = row
        .candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} {} {}",
                candidate.display, candidate.key, candidate.score
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let path = row
        .local_path
        .as_ref()
        .map(|path| path.to_string_lossy())
        .unwrap_or_default();
    let warnings = row.warnings.join(" ");
    let errors = row.errors.join(" ");
    crate::local::query::fields_match_query(
        [
            row.row_id.as_str(),
            session_id,
            source_order.as_str(),
            status,
            row.title.as_str(),
            artist.as_str(),
            album,
            album_artists.as_str(),
            release_date,
            duration.as_str(),
            isrc,
            explicit,
            row.source_key.as_str(),
            source_url,
            selected,
            selected_score.as_str(),
            decision,
            candidates.as_str(),
            path.as_ref(),
            warnings.as_str(),
            errors.as_str(),
        ],
        query,
    )
}

fn import_session_row_is_download_accepted(row: &ImportSessionRow) -> bool {
    matches!(row.status, ImportSessionRowStatus::Matched)
        && !matches!(
            row.review_decision,
            Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
        )
}

fn import_session_row_accepts_manual_review_action(row: &ImportSessionRow) -> bool {
    !row.written
        && row.local_path.is_none()
        && !matches!(
            row.status,
            ImportSessionRowStatus::Matched | ImportSessionRowStatus::SkippedLocal
        )
}

fn import_session_row_selected_key(row: &ImportSessionRow) -> Option<&str> {
    match &row.review_decision {
        Some(ReviewDecision::Accepted { key, .. }) => Some(key.as_str()),
        _ => row.selected_key.as_deref(),
    }
}

fn import_session_row_selected_score(row: &ImportSessionRow) -> Option<f32> {
    match row.review_decision {
        Some(ReviewDecision::Accepted { score, .. }) => Some(score),
        _ => row.selected_score,
    }
}

fn import_session_review_decision_label(decision: Option<&ReviewDecision>) -> &'static str {
    match decision {
        Some(ReviewDecision::Accepted { .. }) => "accepted",
        Some(ReviewDecision::Rejected) => "rejected",
        Some(ReviewDecision::Skipped) => "skipped",
        None => "undecided",
    }
}

fn import_session_download_label(row: &ImportSessionRow) -> &'static str {
    if row.local_path.is_some() {
        "downloaded"
    } else if !row.errors.is_empty() {
        "failed"
    } else if matches!(row.review_decision, Some(ReviewDecision::Rejected)) {
        "rejected"
    } else if matches!(row.review_decision, Some(ReviewDecision::Skipped)) {
        "skipped"
    } else if import_session_row_is_download_accepted(row)
        && import_session_row_selected_key(row).is_some()
    {
        "ready"
    } else if matches!(row.status, ImportSessionRowStatus::NotFound) {
        "missing"
    } else {
        "needs review"
    }
}

fn format_candidate(candidate: &ReportCandidate) -> String {
    format!(
        "{} {} ({})",
        format_score(candidate.score),
        candidate.display,
        candidate.key
    )
}

fn format_score_breakdown(breakdown: crate::transfer::matching::MatchScoreBreakdown) -> String {
    format!(
        "total {}, title {}, artist {}, duration {}, album +{}",
        format_score(breakdown.total),
        format_score(breakdown.title),
        format_score(breakdown.artist),
        format_score(breakdown.duration),
        format_score(breakdown.album_bonus)
    )
}

fn format_score(score: f32) -> String {
    format!("{score:.2}")
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
