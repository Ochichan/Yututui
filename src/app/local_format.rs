//! Local Deck formatting and row helper functions.

use super::App;
use super::local_import_helpers::*;
use super::types::ImportReviewAction;
use crate::api::Song;
use crate::t;
use crate::transfer::session::{ImportSessionRow, ImportSessionRowStatus};

pub(in crate::app) fn push_local_scan_root(
    roots: &mut Vec<crate::local::LocalScanRoot>,
    root: crate::local::LocalScanRoot,
) {
    if let Some(existing) = roots.iter_mut().find(|existing| existing.path == root.path) {
        existing.recursive |= root.recursive;
        if existing.recursive {
            existing.max_depth = None;
        } else if existing.max_depth.is_none() {
            existing.max_depth = root.max_depth;
        } else if let Some(incoming) = root.max_depth {
            existing.max_depth = Some(existing.max_depth.unwrap_or(0).max(incoming));
        }
    } else {
        roots.push(root);
    }
}

pub(in crate::app) fn local_track_text(app: &App, track: &crate::local::LocalTrack) -> String {
    local_song_text(app, &track.to_song())
}

pub(in crate::app) fn local_song_text(app: &App, song: &Song) -> String {
    let title = app.display_title(song);
    let artist = app.display_artist(song);
    if song.duration.is_empty() {
        format!("{title} - {artist}")
    } else {
        format!("{title} - {artist}  ({})", song.duration)
    }
}

pub(in crate::app) fn local_import_session_text(session_id: &str, track_count: usize) -> String {
    if let Ok(session) = crate::transfer::session::ImportSession::load(session_id) {
        let ready_plan = crate::transfer::review_action::plan_ready_candidates(session_id).ok();
        let ready = ready_plan
            .as_ref()
            .map(|plan| plan.ready_count.to_string())
            .unwrap_or_else(|| "?".to_owned());
        let total = ready_plan
            .as_ref()
            .map_or(session.counts.total, |plan| plan.total_count);
        let review = ready_plan
            .as_ref()
            .map_or(session.counts.ambiguous, |plan| plan.review_left);
        let missing = ready_plan
            .as_ref()
            .map_or(session.counts.not_found, |plan| plan.missing_left);
        let local_files = session
            .rows
            .iter()
            .filter(|row| row.local_path.is_some())
            .count();
        let failed = session
            .rows
            .iter()
            .filter(|row| !row.errors.is_empty())
            .count();
        return match crate::i18n::current() {
            crate::i18n::Language::Korean => format!(
                "{session_id}  (준비 {ready}/{total} · 로컬 {local_files}/{total} · 실패 {failed} · 검토 {review} · 누락 {missing} · 대기 {})",
                session.counts.pending
            ),
            crate::i18n::Language::Japanese => format!(
                "{session_id}  (準備 {ready}/{total} · ローカル {local_files}/{total} · 失敗 {failed} · 要確認 {review} · 欠落 {missing} · 保留 {})",
                session.counts.pending
            ),
            _ => format!(
                "{session_id}  (Ready {ready}/{total} · Local {local_files}/{total} · {failed} failed · {review} review · {missing} missing · {} pending)",
                session.counts.pending
            ),
        };
    }
    format!("{session_id}  ({track_count} {})", t!("tracks", "곡", "曲"))
}

pub(in crate::app) fn push_import_session_summary_details(
    lines: &mut Vec<String>,
    session_id: &str,
) {
    let Ok(session) = crate::transfer::session::ImportSession::load(session_id) else {
        return;
    };
    let local_files = session
        .rows
        .iter()
        .filter(|row| row.local_path.is_some())
        .count();
    let failed = session
        .rows
        .iter()
        .filter(|row| !row.errors.is_empty())
        .count();
    push_detail_line(
        lines,
        t!("Rows", "행", "行"),
        format!("{} {}", session.counts.total, t!("rows", "행", "行")),
    );
    let ready_plan = crate::transfer::review_action::plan_ready_candidates(session_id).ok();
    let ready = ready_plan
        .as_ref()
        .map(|plan| plan.ready_count.to_string())
        .unwrap_or_else(|| "?".to_owned());
    let total = ready_plan
        .as_ref()
        .map_or(session.counts.total, |plan| plan.total_count);
    push_detail_line(
        lines,
        t!("Ready", "준비", "準備"),
        format!("{ready}/{total}"),
    );
    push_detail_line(
        lines,
        t!("Local", "로컬", "ローカル"),
        format!("{local_files}/{total}"),
    );
    push_detail_line(lines, t!("Failed", "실패", "失敗"), failed.to_string());
    push_detail_line(
        lines,
        t!("Review", "검토", "要確認"),
        ready_plan
            .as_ref()
            .map_or(session.counts.ambiguous, |plan| plan.review_left)
            .to_string(),
    );
    push_detail_line(
        lines,
        t!("Missing", "누락", "欠落"),
        ready_plan
            .as_ref()
            .map_or(session.counts.not_found, |plan| plan.missing_left)
            .to_string(),
    );
    push_detail_line(
        lines,
        t!("Pending", "대기", "保留"),
        session.counts.pending.to_string(),
    );
    push_detail_line(
        lines,
        t!("Source", "원본", "インポート元"),
        session.source.display(),
    );
    push_detail_line(
        lines,
        t!("Destination", "대상", "インポート先"),
        session.destination.display(),
    );
}

pub(in crate::app) fn push_detail_line(
    lines: &mut Vec<String>,
    label: &str,
    value: impl AsRef<str>,
) {
    let value = value.as_ref().trim();
    if !value.is_empty() {
        lines.push(format!("{label}: {value}"));
    }
}

pub(in crate::app) fn format_album_year(album: Option<&str>, year: Option<i32>) -> Option<String> {
    match (album.map(str::trim).filter(|album| !album.is_empty()), year) {
        (Some(album), Some(year)) => Some(format!("{album} · {year}")),
        (Some(album), None) => Some(album.to_owned()),
        (None, Some(year)) => Some(year.to_string()),
        (None, None) => None,
    }
}

pub(in crate::app) fn format_disc_track(
    disc_no: Option<u32>,
    track_no: Option<u32>,
) -> Option<String> {
    match (disc_no, track_no) {
        (Some(disc), Some(track)) => Some(format!("disc {disc} · track {track}")),
        (Some(disc), None) => Some(format!("disc {disc}")),
        (None, Some(track)) => Some(format!("track {track}")),
        (None, None) => None,
    }
}

pub(in crate::app) fn format_audio_format(format: &crate::local::AudioFormat) -> String {
    match format {
        crate::local::AudioFormat::Aac => "AAC".to_owned(),
        crate::local::AudioFormat::Flac => "FLAC".to_owned(),
        crate::local::AudioFormat::M4a => "M4A".to_owned(),
        crate::local::AudioFormat::Mp3 => "MP3".to_owned(),
        crate::local::AudioFormat::Ogg => "OGG".to_owned(),
        crate::local::AudioFormat::Opus => "OPUS".to_owned(),
        crate::local::AudioFormat::Wav => "WAV".to_owned(),
        crate::local::AudioFormat::Wma => "WMA".to_owned(),
        crate::local::AudioFormat::Other(ext) => ext.to_ascii_uppercase(),
    }
}

pub(in crate::app) fn format_sample_rate(hz: u32) -> String {
    if hz >= 1000 {
        let whole = hz / 1000;
        let tenth = (hz % 1000) / 100;
        if tenth == 0 {
            format!("{whole} kHz")
        } else {
            format!("{whole}.{tenth} kHz")
        }
    } else {
        format!("{hz} Hz")
    }
}

pub(in crate::app) fn format_bitrate(value: u32) -> String {
    let kbps = if value >= 1000 { value / 1000 } else { value };
    format!("{kbps} kbps")
}

pub(in crate::app) fn format_embedded_cover_count(count: usize) -> String {
    if count == 0 {
        t!("no embedded cover", "내장 커버 없음", "埋め込みカバーなし").to_owned()
    } else if count == 1 {
        t!(
            "1 track with embedded cover",
            "내장 커버 1곡",
            "埋め込みカバー1曲"
        )
        .to_owned()
    } else {
        format!(
            "{count} {}",
            t!(
                "tracks with embedded cover",
                "곡에 내장 커버",
                "曲に埋め込みカバー"
            )
        )
    }
}

pub(in crate::app) fn format_local_scan_progress(
    progress: &crate::local::LocalScanProgress,
) -> String {
    let mut text = format!(
        "{}: {} {}, {} {}, {} {}",
        t!(
            "Scanning local music",
            "로컬 음악 스캔 중",
            "ローカル音楽をスキャン中"
        ),
        progress.seen,
        t!("seen", "확인", "確認"),
        progress.indexed,
        t!("indexed", "인덱싱", "インデックス"),
        progress.skipped,
        t!("skipped", "건너뜀", "スキップ")
    );
    if progress.errors > 0 {
        text.push_str(&format!(
            ", {} {}",
            progress.errors,
            t!("errors", "오류", "エラー")
        ));
    }
    if let Some(current) = &progress.current {
        text.push_str(" - ");
        text.push_str(&current.display().to_string());
    }
    text
}

pub(in crate::app) fn local_album_matches_filter(
    album: &crate::local::LocalAlbum,
    query: &str,
) -> bool {
    let year = album.year.map(|year| year.to_string()).unwrap_or_default();
    let track_count = album.track_count.to_string();
    crate::local::query::fields_match_query(
        [
            album.title.as_str(),
            album.album_artist.as_str(),
            year.as_str(),
            track_count.as_str(),
        ],
        query,
    )
}

pub(in crate::app) fn sort_local_tracks(tracks: &mut Vec<&crate::local::LocalTrack>) {
    tracks.sort_by(|a, b| {
        a.disc_no
            .cmp(&b.disc_no)
            .then_with(|| a.track_no.cmp(&b.track_no))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.title.cmp(&b.title))
    });
}

pub(in crate::app) fn format_local_duration_ms(ms: u64) -> String {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(in crate::app) fn push_import_session_row_metadata_details(
    lines: &mut Vec<String>,
    session_id: &str,
    source_order: u32,
    row: &ImportSessionRow,
) {
    push_detail_line(
        lines,
        t!("Import session", "임포트 세션", "インポートセッション"),
        session_id,
    );
    push_detail_line(lines, t!("Row", "행", "行"), format!("#{source_order}"));
    push_detail_line(
        lines,
        t!("Status", "상태", "状態"),
        import_session_row_status_label(row),
    );
    if let Some(detail) = import_session_row_status_detail(row) {
        push_detail_line(
            lines,
            t!("Status detail", "상태 설명", "状態の説明"),
            detail,
        );
    }
    push_detail_line(lines, t!("Title", "제목", "タイトル"), row.title.clone());
    push_detail_line(
        lines,
        t!("Artist", "아티스트", "アーティスト"),
        import_session_row_artist(row),
    );
    if let Some(album) = row.album.clone() {
        push_detail_line(lines, t!("Album", "앨범", "アルバム"), album);
    }
    if !row.album_artists.is_empty() {
        push_detail_line(
            lines,
            t!("Album artist", "앨범 아티스트", "アルバムアーティスト"),
            row.album_artists.join(", "),
        );
    }
    if let Some(release_date) = row.album_release_date.clone() {
        push_detail_line(
            lines,
            t!("Release date", "발매일", "リリース日"),
            release_date,
        );
    }
    if let Some(number) = format_disc_track(row.disc_number, row.track_number) {
        push_detail_line(lines, t!("Track", "트랙", "トラック"), number);
    }
    if let Some(duration) = row.duration_secs {
        push_detail_line(
            lines,
            t!("Duration", "길이", "再生時間"),
            format_local_duration_ms(u64::from(duration) * 1000),
        );
    }
    if let Some(isrc) = row.isrc.clone() {
        push_detail_line(lines, "ISRC", isrc);
    }
    if let Some(explicit) = row.explicit {
        push_detail_line(
            lines,
            t!("Explicit", "Explicit", "Explicit"),
            yes_no(explicit),
        );
    }
    push_detail_line(
        lines,
        t!("Source", "원본", "ソース"),
        row.source_key.clone(),
    );
    if let Some(url) = row
        .source_url
        .as_deref()
        .filter(|url| *url != row.source_key)
    {
        push_detail_line(lines, t!("Source URL", "원본 URL", "ソースURL"), url);
    }
    if let Some(display) = row.selected_display.clone() {
        push_detail_line(lines, t!("Selected", "선택", "選択"), display);
    } else if let Some(key) = row.selected_key.clone() {
        push_detail_line(lines, t!("Selected", "선택", "選択"), key);
    }
    if let Some(score) = import_session_row_selected_score(row) {
        push_detail_line(lines, t!("Score", "점수", "スコア"), format_score(score));
    }
    if matches!(&row.status, ImportSessionRowStatus::NotFound) && !row.search_queries.is_empty() {
        push_detail_line(
            lines,
            t!("Tried queries", "시도한 검색어", "試した検索語"),
            row.search_queries.join(" | "),
        );
    }
    if let Some(reason) = row.reject_reason.clone() {
        push_detail_line(
            lines,
            t!("Top rejection", "주요 거부 이유", "主な拒否理由"),
            reason,
        );
    }
    push_detail_line(
        lines,
        t!("Decision", "결정", "決定"),
        import_session_review_decision_label(row.review_decision.as_ref()),
    );
    push_detail_line(
        lines,
        t!("Download", "다운로드", "ダウンロード"),
        import_session_download_label(row),
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
            t!("Candidates", "후보", "候補"),
            format!("+{} more", row.candidates.len() - 5),
        );
    }
    if let Some(path) = row.local_path.clone() {
        push_detail_line(
            lines,
            t!("Path", "경로", "パス"),
            path.display().to_string(),
        );
    }
}

pub(in crate::app) fn push_import_session_row_diagnostic_details(
    lines: &mut Vec<String>,
    row: &ImportSessionRow,
) {
    for warning in &row.warnings {
        push_detail_line(lines, t!("Warning", "경고", "警告"), warning);
    }
    for error in &row.errors {
        push_detail_line(lines, t!("Error", "오류", "エラー"), error);
    }
}

pub(in crate::app) fn local_import_record_missing_text() -> &'static str {
    t!(
        "No saved import record exists; imported songs were left unchanged.",
        "저장된 임포트 기록이 없습니다. 임포트한 곡은 변경하지 않았습니다.",
        "保存されたインポート記録はありません。インポートした曲は変更していません。"
    )
}

pub(in crate::app) fn import_review_in_progress_text(session_id: &str) -> String {
    format!(
        "{}: {session_id}",
        t!(
            "Import review already in progress",
            "임포트 검토 진행 중",
            "インポートレビュー進行中"
        )
    )
}

pub(in crate::app) fn import_review_progress_text(
    action: ImportReviewAction,
    source_order: u32,
) -> String {
    format!(
        "{} #{}...",
        import_review_action_progress_label(action),
        source_order
    )
}

pub(in crate::app) fn import_review_action_progress_label(
    action: ImportReviewAction,
) -> &'static str {
    match action {
        ImportReviewAction::AcceptFirst => {
            t!(
                "Accepting import row",
                "임포트 행 수락 중",
                "インポート行を承認中"
            )
        }
        ImportReviewAction::ChooseNext => {
            t!(
                "Selecting import candidate",
                "임포트 후보 선택 중",
                "インポート候補を選択中"
            )
        }
        ImportReviewAction::Reject => t!(
            "Rejecting import row",
            "임포트 행 거부 중",
            "インポート行を拒否中"
        ),
        ImportReviewAction::Skip => t!(
            "Skipping import row",
            "임포트 행 건너뛰는 중",
            "インポート行をスキップ中"
        ),
    }
}

pub(in crate::app) fn local_ready_status_text(
    ready_count: u32,
    total_count: u32,
    local_count: u32,
) -> String {
    use crate::i18n::Language;
    match crate::i18n::current() {
        Language::Korean => {
            format!("준비 {ready_count}/{total_count} · 로컬 {local_count}/{total_count}")
        }
        Language::Japanese => {
            format!("準備 {ready_count}/{total_count} · ローカル {local_count}/{total_count}")
        }
        _ => format!("Ready {ready_count}/{total_count} · Local {local_count}/{total_count}"),
    }
}

pub(in crate::app) fn import_review_success_text(
    action: ImportReviewAction,
    summary: &crate::transfer::review_action::ReviewActionSummary,
) -> String {
    match action {
        ImportReviewAction::AcceptFirst => match &summary.display {
            Some(display) => format!(
                "{} #{}: {display}",
                t!(
                    "Accepted import row",
                    "임포트 행 수락",
                    "インポート行を承認"
                ),
                summary.source_order
            ),
            None => format!(
                "{} #{}",
                t!(
                    "Accepted import row",
                    "임포트 행 수락",
                    "インポート行を承認"
                ),
                summary.source_order
            ),
        },
        ImportReviewAction::ChooseNext => match &summary.display {
            Some(display) => format!(
                "{} #{}: {display}",
                t!(
                    "Selected import candidate",
                    "임포트 후보 선택",
                    "インポート候補を選択"
                ),
                summary.source_order
            ),
            None => format!(
                "{} #{}",
                t!(
                    "Selected import candidate",
                    "임포트 후보 선택",
                    "インポート候補を選択"
                ),
                summary.source_order
            ),
        },
        ImportReviewAction::Reject => format!(
            "{} #{}",
            t!(
                "Rejected import row",
                "임포트 행 거부",
                "インポート行を拒否"
            ),
            summary.source_order
        ),
        ImportReviewAction::Skip => format!(
            "{} #{}",
            t!(
                "Skipped import row",
                "임포트 행 건너뜀",
                "インポート行をスキップ"
            ),
            summary.source_order
        ),
    }
}
