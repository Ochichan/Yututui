//! Local Deck import session rows and playback helpers.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::local_format::*;
use super::*;
use crate::t;
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
                .filter(|row| import_session_row_matches_query(row, query))
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
        let Some(row) = load_import_session_row(session_id, source_order) else {
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
        if let Some(display) = row.selected_display.clone() {
            push_detail_line(lines, t!("Selected", "선택"), display);
        } else if let Some(key) = row.selected_key.clone() {
            push_detail_line(lines, t!("Selected", "선택"), key);
        }
        if let Some(path) = row.local_path.clone() {
            push_detail_line(lines, t!("Path", "경로"), path.display().to_string());
        }
        for warning in row.warnings {
            push_detail_line(lines, t!("Warning", "경고"), warning);
        }
        for error in row.errors {
            push_detail_line(lines, t!("Error", "오류"), error);
        }
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

fn import_session_row_status_label(row: &ImportSessionRow) -> &'static str {
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

fn import_session_row_artist(row: &ImportSessionRow) -> String {
    if row.artists.is_empty() {
        t!("Local file", "로컬 파일").to_owned()
    } else {
        row.artists.join(", ")
    }
}

fn import_session_row_matches_query(row: &ImportSessionRow, query: &str) -> bool {
    let source_order = row.source_order.to_string();
    let status = import_session_row_status_label(row);
    let artist = import_session_row_artist(row);
    let album = row.album.as_deref().unwrap_or_default();
    let selected = row
        .selected_display
        .as_deref()
        .or(row.selected_key.as_deref())
        .unwrap_or_default();
    let path = row
        .local_path
        .as_ref()
        .map(|path| path.to_string_lossy())
        .unwrap_or_default();
    let error = row.errors.first().map(String::as_str).unwrap_or_default();
    crate::local::query::fields_match_query(
        [
            row.row_id.as_str(),
            source_order.as_str(),
            status,
            row.title.as_str(),
            artist.as_str(),
            album,
            row.source_key.as_str(),
            selected,
            path.as_ref(),
            error,
        ],
        query,
    )
}
