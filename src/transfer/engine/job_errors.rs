//! Checkpoint-preserving provider error classification for resumable transfers.

use super::*;
use crate::transfer::checkpoint::JobDeferReason;

/// Persist the checkpoint before surfacing an error — the resume story depends on it.
pub(super) fn checkpointed(cp: &mut Checkpoint, err: JobError) -> JobError {
    if let Err(error) = cp.save() {
        tracing::warn!(error = %error, "could not save checkpoint while failing");
    } else {
        save_import_session(cp);
        if cp.defer_reason.is_some() {
            let report = build_report(cp, cp.skipped_local);
            if let Err(error) = report.save() {
                tracing::warn!(error = %error, "could not save deferred transfer report");
            }
        }
    }
    err
}

pub(super) fn save_import_session(cp: &Checkpoint) {
    let session = super::super::session::ImportSession::from_checkpoint(cp);
    if let Err(error) = session.save_unlocked() {
        tracing::warn!(error = %error, "could not save import session");
    }
}

pub(super) fn spotify_job_error(error: SpotifyError) -> JobError {
    let resumable = matches!(
        error,
        SpotifyError::RateLimited | SpotifyError::Network(_) | SpotifyError::Auth(_)
    );
    JobError {
        resumable,
        error: anyhow!("{error}"),
    }
}

fn is_youtube_search_failure(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("yt-dlp")
        || detail.contains("unable to download api page")
        || detail.contains("http error 403")
        || detail.contains("403 forbidden")
        || detail.contains("http error 429")
        || detail.contains("too many requests")
        || detail.contains("rate-limited")
        || crate::tools::classify_ytdlp_failure(&detail).is_some()
}

pub(super) fn defer_ytm_match_error(
    cp: &mut Checkpoint,
    failed_index: Option<usize>,
    error: anyhow::Error,
) -> JobError {
    let detail = format!("{error:#}");
    let budget_exhausted = detail.contains("provider budget");
    let (code, backend, message) = if budget_exhausted {
        (
            "provider_budget_exhausted",
            "youtube",
            "YouTube matching exceeded its bounded provider budget; resume to retry pending tracks",
        )
    } else if is_youtube_search_failure(&detail) {
        (
            "youtube_search_unavailable",
            "youtube",
            "YouTube search is temporarily unavailable; resume this job after retrying",
        )
    } else {
        (
            "ytm_catalog_unavailable",
            "youtube_music",
            "YouTube Music catalog matching is temporarily unavailable; resume this job after restoring access",
        )
    };

    cp.defer_reason = Some(JobDeferReason::new(code, Some(backend), message));
    cp.match_stats.deferred_jobs = cp.match_stats.deferred_jobs.saturating_add(1);
    cp.match_stats.bump_provider_error(code);
    cp.match_stats
        .bump_terminal_reason("deferred_provider_error");
    if let Some(index) = failed_index {
        cp.match_traces
            .entry(index as u32 + 1)
            .or_default()
            .terminal_reason = Some(code.to_owned());
    }

    checkpointed(cp, ytm_job_error(error))
}

pub(super) fn ytm_job_error(error: anyhow::Error) -> JobError {
    // Mid-job YTM/yt-dlp failures are resumable. Do not blame cookies when the cause is
    // clearly a public yt-dlp search / YouTube API rejection.
    let detail = format!("{error:#}");
    let context = if detail.contains("provider budget") {
        "YouTube matching exceeded its bounded provider budget — resume this job to retry pending tracks"
    } else if is_youtube_search_failure(&detail) {
        "YouTube/yt-dlp search failed — wait and retry, or run `ytt tools update` / `ytt doctor --verbose`; then resume this job"
    } else {
        "YouTube Music request failed — after fixing the cookie you can resume this job"
    };
    JobError {
        resumable: true,
        error: error.context(context),
    }
}
