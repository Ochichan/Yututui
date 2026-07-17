//! Pure helpers for Local Deck import-session rows.

use crate::t;
use crate::transfer::checkpoint::{ReportCandidate, ReviewDecision};
use crate::transfer::session::{ImportSessionRow, ImportSessionRowStatus};

pub(in crate::app) fn import_session_row_status_label(row: &ImportSessionRow) -> &'static str {
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
        ImportSessionRowStatus::SkippedCapacity => "capacity",
    }
}

pub(in crate::app) fn import_session_row_status_detail(
    row: &ImportSessionRow,
) -> Option<&'static str> {
    match row.status {
        ImportSessionRowStatus::Pending => Some(t!(
            "matching is still running, or the import was interrupted before this row was processed",
            "매칭 중이거나 이 행 처리 전에 가져오기가 중단됐어요",
            "マッチング中か、この行の処理前にインポートが中断されました"
        )),
        ImportSessionRowStatus::Matched if !row.written => Some(t!(
            "Ready; downloading is a separate step",
            "준비 완료; 다운로드는 별도 단계예요",
            "準備完了; ダウンロードは別のステップです"
        )),
        ImportSessionRowStatus::Ambiguous => Some(t!(
            "review candidate scores; A mark all ready, a accept, s search",
            "후보 점수를 확인하세요; A 전체 준비, a 수락, s 검색",
            "候補スコアを確認してください; A 全件準備, a 承認, s 検索"
        )),
        ImportSessionRowStatus::NotFound => Some(t!(
            "no usable YouTube Music candidate was found",
            "사용 가능한 YouTube Music 후보를 찾지 못했어요",
            "使用できるYouTube Music候補が見つかりませんでした"
        )),
        ImportSessionRowStatus::SkippedCapacity => Some(t!(
            "the destination playlist reached its track limit",
            "대상 플레이리스트가 곡 수 제한에 도달했어요",
            "対象のプレイリストが曲数の上限に達しました"
        )),
        _ => None,
    }
}

pub(in crate::app) fn import_session_row_needs_inbox_attention(row: &ImportSessionRow) -> bool {
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

pub(in crate::app) fn path_is_import_inbox(path: &std::path::Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".yututui-inbox")
}

pub(in crate::app) fn import_session_row_artist(row: &ImportSessionRow) -> String {
    if row.artists.is_empty() {
        t!("Local file", "로컬 파일", "ローカルファイル").to_owned()
    } else {
        row.artists.join(", ")
    }
}

pub(in crate::app) fn import_session_row_matches_query(
    session_id: &str,
    row: &ImportSessionRow,
    query: &str,
) -> bool {
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

pub(in crate::app) fn import_session_manual_search_query(row: &ImportSessionRow) -> Option<String> {
    let mut parts = Vec::new();
    push_search_part(&mut parts, &row.title);
    if !row.artists.is_empty() {
        push_search_part(&mut parts, &row.artists.join(" "));
    }
    if parts.is_empty() {
        if let Some(display) = &row.selected_display {
            push_search_part(&mut parts, display);
        }
        if let Some(key) = &row.selected_key {
            push_search_part(&mut parts, key);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn push_search_part(parts: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_owned());
    }
}

pub(in crate::app) fn import_session_row_is_download_accepted(row: &ImportSessionRow) -> bool {
    matches!(row.status, ImportSessionRowStatus::Matched)
        && !matches!(
            row.review_decision,
            Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
        )
}

pub(in crate::app) fn import_session_row_accepts_manual_review_action(
    row: &ImportSessionRow,
) -> bool {
    !row.written
        && row.local_path.is_none()
        && !matches!(
            row.status,
            ImportSessionRowStatus::Matched
                | ImportSessionRowStatus::SkippedLocal
                | ImportSessionRowStatus::SkippedCapacity
        )
}

pub(in crate::app) fn import_session_row_selected_key(row: &ImportSessionRow) -> Option<&str> {
    match &row.review_decision {
        Some(ReviewDecision::Accepted { key, .. }) => Some(key.as_str()),
        _ => row.selected_key.as_deref(),
    }
}

pub(in crate::app) fn import_session_row_candidate_url_key(row: &ImportSessionRow) -> Option<&str> {
    import_session_row_selected_key(row).or_else(|| row.candidates.first().map(|c| c.key.as_str()))
}

pub(in crate::app) fn youtube_watch_url_for_candidate(key: &str) -> Option<String> {
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some(format!("https://www.youtube.com/watch?v={key}"))
}

pub(in crate::app) fn import_session_row_selected_score(row: &ImportSessionRow) -> Option<f32> {
    match row.review_decision {
        Some(ReviewDecision::Accepted { score, .. }) => Some(score),
        _ => row.selected_score,
    }
}

pub(in crate::app) fn import_session_review_decision_label(
    decision: Option<&ReviewDecision>,
) -> &'static str {
    match decision {
        Some(ReviewDecision::Accepted { .. }) => "accepted",
        Some(ReviewDecision::Rejected) => "rejected",
        Some(ReviewDecision::Skipped) => "skipped",
        None => "undecided",
    }
}

pub(in crate::app) fn import_session_download_label(row: &ImportSessionRow) -> &'static str {
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

pub(in crate::app) fn format_candidate(candidate: &ReportCandidate) -> String {
    format!(
        "{} {} ({})",
        format_score(candidate.score),
        candidate.display,
        candidate.key
    )
}

pub(in crate::app) fn format_score_breakdown(
    breakdown: &crate::transfer::matching::MatchScoreBreakdown,
) -> String {
    format!(
        "total {}, title {}, artist {}, duration {}, album +{}",
        format_score(breakdown.total),
        format_score(breakdown.title),
        format_score(breakdown.artist),
        format_score(breakdown.duration),
        format_score(breakdown.album_bonus)
    )
}

pub(in crate::app) fn format_score(score: f32) -> String {
    format!("{score:.2}")
}

pub(in crate::app) fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
