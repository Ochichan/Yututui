//! Local Deck formatting and row helper functions.

use super::App;
use crate::api::Song;
use crate::t;

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
        return if crate::i18n::is_korean() {
            format!(
                "{session_id}  (준비 {ready}/{total} · 로컬 {local_files}/{total} · 실패 {failed} · 검토 {review} · 누락 {missing} · 대기 {})",
                session.counts.pending
            )
        } else {
            format!(
                "{session_id}  (Ready {ready}/{total} · Local {local_files}/{total} · {failed} failed · {review} review · {missing} missing · {} pending)",
                session.counts.pending
            )
        };
    }
    format!("{session_id}  ({track_count} {})", t!("tracks", "곡"))
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
        t!("Rows", "행"),
        format!("{} {}", session.counts.total, t!("rows", "행")),
    );
    let ready_plan = crate::transfer::review_action::plan_ready_candidates(session_id).ok();
    let ready = ready_plan
        .as_ref()
        .map(|plan| plan.ready_count.to_string())
        .unwrap_or_else(|| "?".to_owned());
    let total = ready_plan
        .as_ref()
        .map_or(session.counts.total, |plan| plan.total_count);
    push_detail_line(lines, t!("Ready", "준비"), format!("{ready}/{total}"));
    push_detail_line(lines, t!("Local", "로컬"), format!("{local_files}/{total}"));
    push_detail_line(lines, t!("Failed", "실패"), failed.to_string());
    push_detail_line(
        lines,
        t!("Review", "검토"),
        ready_plan
            .as_ref()
            .map_or(session.counts.ambiguous, |plan| plan.review_left)
            .to_string(),
    );
    push_detail_line(
        lines,
        t!("Missing", "누락"),
        ready_plan
            .as_ref()
            .map_or(session.counts.not_found, |plan| plan.missing_left)
            .to_string(),
    );
    push_detail_line(
        lines,
        t!("Pending", "대기"),
        session.counts.pending.to_string(),
    );
    push_detail_line(lines, t!("Source", "원본"), session.source.display());
    push_detail_line(
        lines,
        t!("Destination", "대상"),
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
        t!("no embedded cover", "내장 커버 없음").to_owned()
    } else if count == 1 {
        t!("1 track with embedded cover", "내장 커버 1곡").to_owned()
    } else {
        format!(
            "{count} {}",
            t!("tracks with embedded cover", "곡에 내장 커버")
        )
    }
}

pub(in crate::app) fn format_local_scan_progress(
    progress: &crate::local::LocalScanProgress,
) -> String {
    let mut text = format!(
        "{}: {} {}, {} {}, {} {}",
        t!("Scanning local music", "로컬 음악 스캔 중"),
        progress.seen,
        t!("seen", "확인"),
        progress.indexed,
        t!("indexed", "인덱싱"),
        progress.skipped,
        t!("skipped", "건너뜀")
    );
    if progress.errors > 0 {
        text.push_str(&format!(", {} {}", progress.errors, t!("errors", "오류")));
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
