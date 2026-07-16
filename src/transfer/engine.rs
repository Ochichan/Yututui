//! The transfer engine: fetch → match → write, with a checkpoint flush at every
//! meaningful boundary so any abort resumes idempotently. One plain async fn — the CLI
//! runs it on its own runtime, the TUI actor on a worker task.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use futures::StreamExt;

use super::checkpoint::ReviewDecision;
use super::checkpoint::{
    Checkpoint, MatchStats, MatchTrace, ReportCandidate, ReportRow, TrackEntry, TransferReport,
};
use super::local_playlist::{
    LocalPlaylistPatch, LocalPlaylistPatchRow, LocalPlaylistStore, LocalPlaylistStoreError,
};
use super::match_cache::TransferMatchCache;
use super::matching::{
    MatchCandidate, MatchConfig, MatchOutcome, Pacing, SharedYtmMatchState, TrackInput,
    best_outcome, match_track_ytm_shared, spotify_query_plan,
};
use super::{
    FileFormat, ImportMediaKind, JobSpec, MatchPolicy, Stage, TransferDest, TransferProgress,
    TransferSource,
};
use crate::api::ytmusic::YtMusicApi;
use crate::api::{Song, SongImportMetadata};
use crate::search_source::SearchConfig;
use crate::spotify::client::{SpotifyClient, SpotifyError};

mod album_prefill;
use album_prefill::{
    apply_persistent_cache, merge_ytm_diagnostics, prefill_album_matches,
    record_match_outcome_stats,
};
mod reporting;
use reporting::*;
mod job_errors;
use job_errors::*;
mod local_write;
#[cfg(test)]
use local_write::song_for_entry;
use local_write::write_local;
mod source;
mod spotify_sync;
use source::*;

/// Every job caps its work list here (YTM's own playlist cap is ~5,000 anyway).
const TRACK_CAP: usize = 10_000;
/// YTM playlist appends: chunk size and the pause between chunks.
const YTM_ADD_CHUNK: usize = 50;
const YTM_ADD_PAUSE: Duration = Duration::from_secs(1);
/// Sequential like pacing (preserves the like-order timeline, stays polite).
const YTM_LIKE_PAUSE: Duration = Duration::from_millis(1200);
/// Spotify's add-items cap per call.
const SPOTIFY_ADD_CHUNK: usize = 100;
/// Checkpoint flush cadence during matching/like-writing.
const CHECKPOINT_EVERY: usize = 10;
/// Matching outcomes use the append-only journal between compact snapshots. Bound the
/// interval between durable appends even when a small job advances slowly.
const MATCH_JOURNAL_DURABLE_EVERY: Duration = Duration::from_secs(1);

struct MatchJournalCadence {
    pending: usize,
    last_durable: Instant,
}

struct MatchCommitContext<'a> {
    cp: &'a mut Checkpoint,
    cache_write_enabled: bool,
    match_cache: &'a mut Option<TransferMatchCache>,
    cache_dirty: &'a mut bool,
    local_capacity_keys: &'a mut Option<HashSet<String>>,
    journal: &'a mut MatchJournalCadence,
    matched_count: &'a mut u32,
    ambiguous_count: &'a mut u32,
    not_found_count: &'a mut u32,
    completed_count: &'a mut u32,
    computed_capacity_skips: &'a mut u32,
    matching_written: u32,
    total: u32,
    progress: &'a mut (dyn FnMut(TransferProgress) + Send),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WriteProgressCounts {
    matched: u32,
    ambiguous: u32,
    not_found: u32,
    written: u32,
}

impl WriteProgressCounts {
    fn from_checkpoint(cp: &Checkpoint) -> Self {
        let (matched, ambiguous, not_found) = outcome_counts(&cp.tracks);
        Self {
            matched,
            ambiguous,
            not_found,
            written: written_count(&cp.tracks),
        }
    }

    fn record_written(&mut self, count: usize) {
        self.written = self.written.saturating_add(count as u32);
    }
}

struct EnsuredDestination {
    id: String,
    created_in_run: bool,
}

impl MatchJournalCadence {
    fn new() -> Self {
        Self {
            pending: 0,
            last_durable: Instant::now(),
        }
    }

    fn next_is_durable(&mut self) -> bool {
        self.pending += 1;
        let durable = self.pending >= CHECKPOINT_EVERY
            || self.last_durable.elapsed() >= MATCH_JOURNAL_DURABLE_EVERY;
        if durable {
            self.pending = 0;
            self.last_durable = Instant::now();
        }
        durable
    }

    fn append(&mut self, cp: &mut Checkpoint, index: usize) -> Result<(), JobError> {
        let durable = self.next_is_durable();
        let bytes = cp
            .append_track_journal(index, durable)
            .map_err(|error| JobError::fatal(error.into()))?;
        cp.match_stats.checkpoint_bytes = cp.match_stats.checkpoint_bytes.saturating_add(bytes);
        if durable && bytes > 0 {
            cp.match_stats.checkpoint_flushes = cp.match_stats.checkpoint_flushes.saturating_add(1);
        }
        Ok(())
    }
}

fn append_write_journal(cp: &mut Checkpoint, indexes: &[usize]) -> Result<(), JobError> {
    let bytes = cp
        .append_tracks_journal(indexes, true)
        .map_err(|error| JobError::fatal(error.into()))?;
    cp.match_stats.checkpoint_bytes = cp.match_stats.checkpoint_bytes.saturating_add(bytes);
    if bytes > 0 {
        cp.match_stats.checkpoint_flushes = cp.match_stats.checkpoint_flushes.saturating_add(1);
    }
    Ok(())
}

/// What a failed job means for the caller: `resumable` failures keep their checkpoint
/// and print/emit the `resume` hint.
pub struct JobError {
    pub resumable: bool,
    pub error: anyhow::Error,
}

impl JobError {
    fn fatal(error: anyhow::Error) -> Self {
        Self {
            resumable: false,
            error,
        }
    }
}

fn local_playlist_job_error(error: LocalPlaylistStoreError) -> JobError {
    JobError {
        resumable: error.is_resumable(),
        error: anyhow::Error::new(error),
    }
}

/// The clients a job may need; flows check for what they actually use and fail with a
/// setup hint otherwise.
pub struct JobCtx {
    pub ytm: Option<YtMusicApi>,
    pub spotify: Option<SpotifyClient>,
    pub search_config: SearchConfig,
    /// Spotify search market (config `spotify.market`).
    pub market: Option<String>,
}

impl JobCtx {
    fn ytm(&self) -> Result<&YtMusicApi, JobError> {
        self.ytm.as_ref().ok_or_else(|| {
            JobError::fatal(anyhow!(
                "this needs a YouTube Music cookie — add cookies.txt (or `cookie`) in Settings › General"
            ))
        })
    }

    fn spotify(&mut self) -> Result<&mut SpotifyClient, JobError> {
        self.spotify.as_mut().ok_or_else(|| {
            JobError::fatal(anyhow!(
                "this needs a connected Spotify account — run `ytt auth spotify` first"
            ))
        })
    }
}

/// Run (or resume) a transfer job to completion. The checkpoint under `job_id` is the
/// single source of truth for what already happened.
pub async fn run_job(
    job_id: String,
    spec: JobSpec,
    resume: Option<Checkpoint>,
    ctx: &mut JobCtx,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
) -> Result<TransferReport, JobError> {
    let mut local_playlists = super::local_playlist::DiskLocalPlaylistStore;
    run_job_with_store(job_id, spec, resume, ctx, &mut local_playlists, progress).await
}

pub(crate) async fn run_job_with_store(
    job_id: String,
    spec: JobSpec,
    resume: Option<Checkpoint>,
    ctx: &mut JobCtx,
    local_playlists: &mut impl LocalPlaylistStore,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
) -> Result<TransferReport, JobError> {
    let _record_guard = super::session::ImportRecordGuard::try_acquire_if_persistable(&job_id)
        .map_err(|error| JobError::fatal(error.into()))?;
    let started = Instant::now();
    if let Some(spotify) = ctx.spotify.as_mut() {
        spotify.reset_rate_budget();
    }

    let local_snapshot = if matches!(spec.source, TransferSource::LocalPlaylist { .. })
        || matches!(spec.dest, TransferDest::LocalPlaylist { .. })
    {
        Some(
            local_playlists
                .snapshot()
                .await
                .map_err(local_playlist_job_error)?,
        )
    } else {
        None
    };

    // File exports never match anything — a straight fetch-and-write.
    if let TransferDest::File { path, format } = &spec.dest {
        let (path, format) = (path.clone(), *format);
        return export_file(
            &job_id,
            &spec,
            ctx,
            local_snapshot.as_ref(),
            &path,
            format,
            started,
        )
        .await;
    }

    // Stage 1 — fetch (or resume from the checkpoint's frozen list).
    let mut cp = match resume {
        Some(mut cp) => {
            // The caller's spec wins over the checkpointed one: this is what turns a
            // finished dry-run into the write pass on resume (and lets --min-score /
            // --take-best be adjusted between runs). Source/dest were used to build
            // the frozen track list, so those stay meaningful either way.
            cp.spec = spec.clone();
            if spec.rematch {
                for entry in &mut cp.tracks {
                    entry.outcome = None;
                    entry.input.known_video_id = None;
                    entry.written = false;
                }
            }
            cp
        }
        None => {
            let (source_name, entries, skipped_local, source_truncated) =
                fetch_source(&job_id, &spec, ctx, local_snapshot.as_ref()).await?;
            let mut cp = Checkpoint::new(job_id.clone(), spec.clone(), entries);
            cp.source_name = Some(source_name.clone());
            cp.dest_name = Some(default_dest_name_for_media(
                &spec.dest,
                &source_name,
                spec.media_kind,
            ));
            cp.skipped_local = skipped_local;
            cp.source_truncated = source_truncated;
            cp.stage = Stage::Matching;
            cp.save().map_err(|e| JobError::fatal(e.into()))?;
            save_import_session(&cp);
            cp
        }
    };
    let skipped_local = cp.skipped_local;

    // Stage 2 — match everything still unresolved.
    if cp.stage == Stage::Fetching {
        cp.stage = Stage::Matching;
    }
    if cp.stage == Stage::Matching {
        match_stage(&mut cp, ctx, local_snapshot.as_ref(), progress).await?;
        cp.stage = Stage::Writing;
        cp.save().map_err(|e| JobError::fatal(e.into()))?;
        save_import_session(&cp);
    }

    let auto_accepted = auto_accept_ambiguous_candidates(&mut cp, local_snapshot.as_ref())?;
    let mut report = build_report(&cp, skipped_local);
    report.auto_accepted = auto_accepted;

    if matches!(cp.spec.dest, TransferDest::SpotifyMirrorPlaylist { .. })
        && (cp.spec.dry_run || cp.spotify_sync.is_none())
    {
        spotify_sync::prepare(&mut cp, ctx).await?;
        report = build_report(&cp, skipped_local);
        report.auto_accepted = auto_accepted;
    }

    if !cp.spec.dry_run
        && cp
            .spotify_sync
            .as_ref()
            .is_some_and(|state| !state.preview.ready)
    {
        report.elapsed_secs = started.elapsed().as_secs();
        let _ = report.save();
        let reasons = cp
            .spotify_sync
            .as_ref()
            .map(|state| state.preview.blocked_reasons.join("; "))
            .unwrap_or_else(|| "preview is not ready".to_owned());
        return Err(checkpointed(
            &mut cp,
            JobError {
                resumable: true,
                error: anyhow!("Spotify exact sync is blocked: {reasons}"),
            },
        ));
    }

    // Stage 3 — write (skipped on dry-run: the checkpoint keeps the matches, and a
    // later `resume` performs exactly the writes).
    if !cp.spec.dry_run {
        write_stage(
            &mut cp,
            ctx,
            local_playlists,
            local_snapshot.as_ref(),
            progress,
            &mut report,
        )
        .await?;
        rebuild_report_after_write(&cp, skipped_local, &mut report);
        cp.stage = Stage::Done;
        cp.save().map_err(|e| JobError::fatal(e.into()))?;
        save_import_session(&cp);
    }

    report.elapsed_secs = started.elapsed().as_secs();
    if let Err(e) = report.save() {
        tracing::warn!(error = %e, "could not save transfer report");
    }
    Ok(report)
}

pub async fn write_reviewed_local_job(
    job_id: &str,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
) -> Result<TransferReport, JobError> {
    let mut local_playlists = super::local_playlist::DiskLocalPlaylistStore;
    write_reviewed_local_job_with_store(job_id, &mut local_playlists, progress).await
}

pub(crate) async fn write_reviewed_local_job_with_store(
    job_id: &str,
    local_playlists: &mut impl LocalPlaylistStore,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
) -> Result<TransferReport, JobError> {
    let _record_guard = super::session::ImportRecordGuard::try_acquire_if_persistable(job_id)
        .map_err(|error| JobError::fatal(error.into()))?;
    let started = Instant::now();
    let mut cp = Checkpoint::load(job_id).map_err(JobError::fatal)?;
    if !matches!(cp.spec.dest, TransferDest::LocalPlaylist { .. }) {
        return Err(JobError::fatal(anyhow!(
            "import session `{job_id}` does not target a local Library playlist"
        )));
    }
    cp.spec.dry_run = false;
    cp.stage = Stage::Writing;
    let local_snapshot = local_playlists
        .snapshot()
        .await
        .map_err(local_playlist_job_error)?;
    if let Some(mut keys) = local_destination_keys(&cp, Some(&local_snapshot)) {
        enforce_matched_capacity(&mut cp, &mut keys);
    }
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    save_import_session(&cp);

    let mut report = build_report(&cp, cp.skipped_local);
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: crate::search_source::SearchConfig::default(),
        market: None,
    };
    write_stage(
        &mut cp,
        &mut ctx,
        local_playlists,
        Some(&local_snapshot),
        progress,
        &mut report,
    )
    .await?;
    rebuild_report_after_write(&cp, cp.skipped_local, &mut report);
    cp.stage = Stage::Done;
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    save_import_session(&cp);

    report.elapsed_secs = started.elapsed().as_secs();
    if let Err(e) = report.save() {
        tracing::warn!(error = %e, "could not save transfer report");
    }
    Ok(report)
}

// Match ---------------------------------------------------------------------------

fn accept_margin_for_policy(policy: MatchPolicy) -> f32 {
    match policy {
        MatchPolicy::Strict => MatchConfig::default().accept_margin,
        MatchPolicy::Balanced => 0.03,
        MatchPolicy::Aggressive => 0.0,
        MatchPolicy::Exhaustive => MatchConfig::default().accept_margin,
    }
}

fn env_concurrency(name: &str, default: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

fn ytm_track_concurrency() -> usize {
    env_concurrency("YTM_TRANSFER_MATCH_CONCURRENCY", 4, 8)
}

fn ytm_catalog_concurrency() -> usize {
    env_concurrency("YTM_TRANSFER_CATALOG_CONCURRENCY", 4, 8)
}

fn ytm_video_concurrency() -> usize {
    env_concurrency("YTM_TRANSFER_VIDEO_CONCURRENCY", 2, 4)
}

fn ytm_preflight_concurrency() -> usize {
    env_concurrency("YTM_TRANSFER_PREFLIGHT_CONCURRENCY", 3, 6)
}

async fn match_stage(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    local_snapshot: Option<&crate::playlists::Playlists>,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
) -> Result<(), JobError> {
    // A prior provider defer describes the previous attempt. Keep the aggregate counter,
    // but only expose an active reason when this attempt also has to pause.
    cp.defer_reason = None;
    let cfg = MatchConfig {
        accept: cp.spec.min_score,
        ambiguous_floor: (cp.spec.min_score - 0.20).max(0.40),
        accept_margin: accept_margin_for_policy(cp.spec.match_policy),
        policy: cp.spec.match_policy,
        allow_user_videos: cp.spec.allow_user_videos,
        media_kind: cp.spec.media_kind,
    };
    let to_spotify = matches!(
        cp.spec.dest,
        TransferDest::SpotifyNewPlaylist { .. } | TransferDest::SpotifyMirrorPlaylist { .. }
    );
    let total = cp.tracks.len() as u32;
    let mut pace = Pacing::ytm_default();
    let search_config = ctx.search_config.clone();
    let market = ctx.market.clone();
    let cache_read_enabled = !to_spotify && !cp.spec.rematch && cp.spec.cache_mode.read_enabled();
    let cache_write_enabled = !to_spotify && cp.spec.cache_mode.write_enabled();
    let mut match_cache = (!to_spotify).then(|| {
        if cache_read_enabled || cache_write_enabled {
            TransferMatchCache::load_for(cp.spec.media_kind)
        } else {
            TransferMatchCache::default()
        }
    });
    let mut cache_dirty = false;
    if cache_read_enabled && let Some(cache) = match_cache.as_mut() {
        cache_dirty |= cache.retain_compatible_matches(&cfg);
        cache_dirty |= apply_persistent_cache(cp, cache);
    }
    let mut local_capacity_keys = local_destination_keys(cp, local_snapshot);
    if let Some(keys) = local_capacity_keys.as_mut() {
        enforce_matched_capacity(cp, keys);
        if keys.len() >= crate::playlists::SONGS_PER_PLAYLIST_MAX {
            mark_pending_capacity_skipped(cp);
        }
    }
    if !to_spotify
        && cp.spec.media_kind == ImportMediaKind::Track
        && cp.tracks.iter().any(|track| track.outcome.is_none())
    {
        let ytm = ctx.ytm()?;
        if let Some(cache) = match_cache.as_mut() {
            cache_dirty |= prefill_album_matches(
                cp,
                ytm,
                &cfg,
                &search_config,
                cache,
                cache_read_enabled,
                &mut pace,
            )
            .await
            .map_err(|error| defer_ytm_match_error(cp, None, error))?;
        }
    }
    if let Some(keys) = local_capacity_keys.as_mut() {
        enforce_matched_capacity(cp, keys);
        if keys.len() >= crate::playlists::SONGS_PER_PLAYLIST_MAX {
            mark_pending_capacity_skipped(cp);
        }
    }
    let (mut matched_count, mut ambiguous_count, mut not_found_count) = outcome_counts(&cp.tracks);
    let mut completed_count = cp
        .tracks
        .iter()
        .filter(|entry| entry.outcome.is_some())
        .count() as u32;
    let matching_written = written_count(&cp.tracks);
    if to_spotify {
        let mut journal = MatchJournalCadence::new();
        for idx in 0..cp.tracks.len() {
            if cp.tracks[idx].outcome.is_some() {
                continue; // resumed work
            }
            let input = cp.tracks[idx].input.clone();
            // Spotify's client already absorbs transient trouble internally; anything
            // surfacing here (rate budget, auth) is systemic — abort resumably.
            let outcome = match_track_spotify(ctx.spotify()?, &input, &cfg, market.clone())
                .await
                .map_err(|e| checkpointed(cp, e))?;
            record_match_outcome_stats(&mut cp.match_stats, &outcome);
            increment_outcome_counts(
                &outcome,
                &mut matched_count,
                &mut ambiguous_count,
                &mut not_found_count,
            );
            cp.tracks[idx].outcome = Some(outcome);
            journal.append(cp, idx)?;
            completed_count = completed_count.saturating_add(1);

            progress(TransferProgress {
                job_id: cp.job_id.clone(),
                stage: Stage::Matching,
                done: completed_count,
                total,
                matched: matched_count,
                auto_accepted: 0,
                ambiguous: ambiguous_count,
                not_found: not_found_count,
                written: matching_written,
                current: input.display(),
            });
        }
    } else if cp.tracks.iter().any(|entry| entry.outcome.is_none()) {
        // Provider/search failures are not negative match evidence. Pause immediately
        // and leave the failed row pending so resume can retry it without --rematch.
        let ytm = ctx.ytm()?;
        let pending = cp
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.outcome.is_none())
            .map(|(idx, entry)| (idx, entry.input.clone()))
            .collect::<Vec<_>>();
        let shared_state = SharedYtmMatchState::new(
            pace,
            ytm_catalog_concurrency(),
            ytm_video_concurrency(),
            ytm_preflight_concurrency(),
        );
        if cache_read_enabled && let Some(cache) = match_cache.as_ref() {
            shared_state
                .seed_persistent_cache(
                    cache.query_cache_entries(),
                    cache.video_meta_cache_entries(),
                )
                .await;
        }
        // Surface matching start immediately — otherwise the CLI looks hung while the
        // first concurrent yt-dlp searches run (often many seconds with no progress).
        progress(TransferProgress {
            job_id: cp.job_id.clone(),
            stage: Stage::Matching,
            done: completed_count,
            total,
            matched: matched_count,
            auto_accepted: 0,
            ambiguous: ambiguous_count,
            not_found: not_found_count,
            written: matching_written,
            current: pending
                .first()
                .map(|(_, input)| input.display())
                .unwrap_or_default(),
        });
        let mut stream = futures::stream::iter(pending.into_iter().enumerate())
            .map(|(source_sequence, (idx, input))| {
                let state = shared_state.clone();
                let match_cfg = cfg;
                let search_config = search_config.clone();
                async move {
                    let outcome =
                        match_track_ytm_shared(ytm, &input, &match_cfg, &search_config, &state)
                            .await;
                    (source_sequence, idx, input, outcome)
                }
            })
            .buffer_unordered(ytm_track_concurrency());
        let mut journal = MatchJournalCadence::new();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        let mut capacity_reached = false;
        let mut computed_capacity_skips = 0u32;
        let mut ready_by_source = BTreeMap::new();
        let mut next_source_sequence = 0usize;
        let mut stream_finished = false;
        'matching: loop {
            let next = tokio::select! {
                item = stream.next() => item,
                _ = heartbeat.tick() => {
                    progress(TransferProgress {
                        job_id: cp.job_id.clone(),
                        stage: Stage::Matching,
                        done: completed_count,
                        total,
                        matched: matched_count,
                        auto_accepted: 0,
                        ambiguous: ambiguous_count,
                        not_found: not_found_count,
                        written: matching_written,
                        current: "provider work in flight".to_owned(),
                    });
                    continue;
                }
            };
            match next {
                Some((source_sequence, idx, input, Ok(result))) => {
                    ready_by_source.insert(source_sequence, (idx, input, result));
                }
                Some((_, idx, input, Err(failure))) => {
                    // A systemic provider failure must stop new buffered work as soon as it is
                    // observed, even when an earlier source row is still in flight. Parking the
                    // error in `ready_by_source` would let later yt-dlp calls continue until the
                    // ordered commit cursor eventually reached this row.
                    drop(stream);
                    tracing::warn!(
                        track = %input.display(),
                        error = %crate::util::sanitize::sanitize_error_text(format!("{:#}", failure.error)),
                        "match lookup failed; deferring the resumable job"
                    );
                    record_match_trace_stats(&mut cp.match_stats, &failure.trace);
                    cp.match_traces.insert(idx as u32 + 1, failure.trace);
                    if cache_write_enabled && let Some(cache) = match_cache.as_mut() {
                        let snapshot = shared_state.cache_snapshot().await;
                        cache_dirty |=
                            cache.merge_query_cache(snapshot.queries, snapshot.video_metadata);
                    }
                    merge_ytm_diagnostics(&mut cp.match_stats, shared_state.diagnostics().await);
                    if cache_dirty
                        && cache_write_enabled
                        && let Some(cache) = match_cache.as_mut()
                    {
                        let maintenance = cache.save();
                        cp.match_stats.cache_evictions = cp
                            .match_stats
                            .cache_evictions
                            .saturating_add(maintenance.removed().try_into().unwrap_or(u32::MAX));
                    }
                    return Err(defer_ytm_match_error(cp, Some(idx), failure.error));
                }
                None => stream_finished = true,
            }

            // Provider calls finish out of order, but committing in frozen source order keeps
            // the last local-playlist slot deterministic. The unordered stream still backfills
            // completed provider slots, so one slow early track does not stop later retrieval.
            while let Some((idx, input, result)) =
                take_ready_in_source_order(&mut ready_by_source, &mut next_source_sequence)
            {
                let mut commit = MatchCommitContext {
                    cp,
                    cache_write_enabled,
                    match_cache: &mut match_cache,
                    cache_dirty: &mut cache_dirty,
                    local_capacity_keys: &mut local_capacity_keys,
                    journal: &mut journal,
                    matched_count: &mut matched_count,
                    ambiguous_count: &mut ambiguous_count,
                    not_found_count: &mut not_found_count,
                    completed_count: &mut completed_count,
                    computed_capacity_skips: &mut computed_capacity_skips,
                    matching_written,
                    total,
                    progress,
                };
                capacity_reached |=
                    commit_ytm_match_result(&mut commit, idx, input, result.outcome, result.trace)?;
                if capacity_reached {
                    break 'matching;
                }
            }
            if stream_finished {
                debug_assert!(ready_by_source.is_empty());
                break;
            }
        }
        if capacity_reached {
            for (_, (idx, input, result)) in std::mem::take(&mut ready_by_source) {
                let mut commit = MatchCommitContext {
                    cp,
                    cache_write_enabled,
                    match_cache: &mut match_cache,
                    cache_dirty: &mut cache_dirty,
                    local_capacity_keys: &mut local_capacity_keys,
                    journal: &mut journal,
                    matched_count: &mut matched_count,
                    ambiguous_count: &mut ambiguous_count,
                    not_found_count: &mut not_found_count,
                    completed_count: &mut completed_count,
                    computed_capacity_skips: &mut computed_capacity_skips,
                    matching_written,
                    total,
                    progress,
                };
                commit_ytm_match_result(&mut commit, idx, input, result.outcome, result.trace)?;
            }
            if computed_capacity_skips > 0 {
                record_capacity_skips(&mut cp.match_stats, computed_capacity_skips);
            }
            // This is a deterministic in-memory bulk transition followed immediately by the
            // compact stage snapshot below. Journaling thousands of synthetic capacity rows
            // would add one open/write per row (and periodic fsyncs) without protecting any
            // provider result; buffered provider results were committed above, and a crash
            // simply recomputes the same boundary on resume.
            mark_pending_capacity_skipped(cp);
        }
        if cache_write_enabled && let Some(cache) = match_cache.as_mut() {
            let snapshot = shared_state.cache_snapshot().await;
            cache_dirty |= cache.merge_query_cache(snapshot.queries, snapshot.video_metadata);
        }
        merge_ytm_diagnostics(&mut cp.match_stats, shared_state.diagnostics().await);
    }
    if cache_dirty
        && cache_write_enabled
        && let Some(cache) = match_cache.as_mut()
    {
        let maintenance = cache.save();
        cp.match_stats.cache_evictions = cp
            .match_stats
            .cache_evictions
            .saturating_add(maintenance.removed().try_into().unwrap_or(u32::MAX));
    }
    Ok(())
}

fn take_ready_in_source_order<T>(
    ready: &mut BTreeMap<usize, T>,
    next_sequence: &mut usize,
) -> Option<T> {
    let value = ready.remove(next_sequence)?;
    *next_sequence = next_sequence.saturating_add(1);
    Some(value)
}

fn commit_ytm_match_result(
    context: &mut MatchCommitContext<'_>,
    idx: usize,
    input: TrackInput,
    outcome: MatchOutcome,
    trace: MatchTrace,
) -> Result<bool, JobError> {
    let current = input.display();
    if context.cache_write_enabled
        && let Some(cache) = context.match_cache.as_mut()
        && matches!(outcome, MatchOutcome::Matched { .. })
    {
        cache.save_match(&input, &outcome);
        *context.cache_dirty = true;
    }
    context.cp.tracks[idx].outcome = Some(outcome);
    let capacity_reached = enforce_local_capacity_for_committed_entry(
        context.cp,
        idx,
        context.local_capacity_keys,
        context.computed_capacity_skips,
    );
    let outcome = context.cp.tracks[idx]
        .outcome
        .as_ref()
        .expect("committed match has an outcome");
    record_match_outcome_stats(&mut context.cp.match_stats, outcome);
    record_match_trace_stats(&mut context.cp.match_stats, &trace);
    increment_outcome_counts(
        outcome,
        context.matched_count,
        context.ambiguous_count,
        context.not_found_count,
    );
    context.cp.match_traces.insert(idx as u32 + 1, trace);
    context.journal.append(context.cp, idx)?;
    *context.completed_count = (*context.completed_count).saturating_add(1);

    (context.progress)(TransferProgress {
        job_id: context.cp.job_id.clone(),
        stage: Stage::Matching,
        done: *context.completed_count,
        total: context.total,
        matched: *context.matched_count,
        auto_accepted: 0,
        ambiguous: *context.ambiguous_count,
        not_found: *context.not_found_count,
        written: context.matching_written,
        current,
    });
    Ok(capacity_reached)
}

fn enforce_local_capacity_for_committed_entry(
    cp: &mut Checkpoint,
    idx: usize,
    local_capacity_keys: &mut Option<HashSet<String>>,
    computed_capacity_skips: &mut u32,
) -> bool {
    let Some(keys) = local_capacity_keys.as_mut() else {
        return false;
    };
    let capacity = crate::playlists::SONGS_PER_PLAYLIST_MAX;
    let Some(key) = effective_write_key(&cp.tracks[idx], cp.spec.take_best).map(str::to_owned)
    else {
        return keys.len() >= capacity;
    };
    if cp.tracks[idx].written || keys.contains(&key) {
        return keys.len() >= capacity;
    }
    if keys.len() < capacity {
        keys.insert(key);
    } else {
        cp.tracks[idx].outcome = Some(MatchOutcome::SkippedCapacity);
        cp.tracks[idx].review_decision = None;
        *computed_capacity_skips = (*computed_capacity_skips).saturating_add(1);
    }
    keys.len() >= capacity
}

fn auto_accept_ambiguous_candidates(
    cp: &mut Checkpoint,
    local_snapshot: Option<&crate::playlists::Playlists>,
) -> Result<u32, JobError> {
    let Some(min_score) = cp.spec.auto_accept_ambiguous_min_score else {
        return Ok(0);
    };
    let mut accepted = 0u32;
    let mut capacity_keys = local_destination_keys(cp, local_snapshot);
    if let Some(keys) = capacity_keys.as_mut() {
        enforce_matched_capacity(cp, keys);
    }
    for entry in &mut cp.tracks {
        if entry.written || entry.review_decision.is_some() {
            continue;
        }
        let Some(MatchOutcome::Ambiguous { candidates }) = &entry.outcome else {
            continue;
        };
        let Some(candidate) = best_safe_ambiguous_candidate(candidates) else {
            continue;
        };
        let candidate = candidate.clone();
        if candidate.score < min_score {
            continue;
        }
        if let Some(keys) = capacity_keys.as_mut()
            && !keys.contains(&candidate.key)
        {
            if keys.len() >= crate::playlists::SONGS_PER_PLAYLIST_MAX {
                entry.outcome = Some(MatchOutcome::SkippedCapacity);
                record_capacity_skips(&mut cp.match_stats, 1);
                continue;
            }
            keys.insert(candidate.key.clone());
        }
        entry.review_decision = Some(ReviewDecision::Accepted {
            key: candidate.key.clone(),
            score: candidate.score,
            display: candidate.display.clone(),
        });
        entry.outcome = Some(MatchOutcome::Matched {
            key: candidate.key.clone(),
            score: candidate.score,
            display: candidate.display.clone(),
            title: None,
            artist: None,
            album: None,
            duration_secs: None,
            score_breakdown: candidate.score_breakdown.clone().map(Box::new),
        });
        accepted += 1;
    }
    if accepted > 0 {
        cp.save().map_err(|e| JobError::fatal(e.into()))?;
        save_import_session(cp);
    }
    Ok(accepted)
}

/// Spotify-direction retrieval: ISRC first when available, then Spotify field filters,
/// release year, and finally plain text.
async fn match_track_spotify(
    spotify: &mut SpotifyClient,
    input: &TrackInput,
    cfg: &MatchConfig,
    market: Option<String>,
) -> Result<MatchOutcome, JobError> {
    let mut candidates: Vec<MatchCandidate> = Vec::new();
    let mut outcome = MatchOutcome::NotFound;
    for query in spotify_query_plan(input) {
        let more = spotify
            .search_track(&query, market.as_deref())
            .await
            .map_err(spotify_job_error)?;
        for track in &more {
            if !candidates.iter().any(|c| c.key == track.uri) {
                candidates.push(MatchCandidate::from(track));
            }
        }
        outcome = best_outcome(input, &candidates, cfg);
        if matches!(outcome, MatchOutcome::Matched { .. }) {
            break;
        }
    }
    if candidates.is_empty() && market.as_deref().is_none_or(str::is_empty) {
        tracing::debug!(
            track = %input.display(),
            "Spotify search returned no candidates and no market is configured"
        );
    }
    Ok(outcome)
}

// Write ---------------------------------------------------------------------------

async fn write_stage(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    local_playlists: &mut impl LocalPlaylistStore,
    local_snapshot: Option<&crate::playlists::Playlists>,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
    report: &mut TransferReport,
) -> Result<(), JobError> {
    let writes = if matches!(
        cp.spec.dest,
        TransferDest::LocalPlaylist { .. } | TransferDest::SpotifyMirrorPlaylist { .. }
    ) {
        collect_writes_without_deduping(cp)
    } else {
        collect_writes(cp, report)
    };
    let total = writes.len() as u32;

    // The local store needs no network at all — handle it before the client-bound arms.
    if matches!(cp.spec.dest, TransferDest::LocalPlaylist { .. }) {
        let observed_revision = local_snapshot.map_or(0, crate::playlists::Playlists::revision);
        return write_local(
            cp,
            writes,
            observed_revision,
            local_playlists,
            progress,
            report,
        )
        .await;
    }

    match cp.spec.dest.clone() {
        TransferDest::YtmLikes => {
            let pending: Vec<(usize, String)> = writes
                .into_iter()
                .filter(|(idx, _)| !cp.tracks[*idx].written)
                .collect();
            let pending_count = pending.len();
            let mut done = total - pending_count as u32;
            report.written += done;
            let mut progress_counts = WriteProgressCounts::from_checkpoint(cp);
            let mut journal_batch = Vec::with_capacity(CHECKPOINT_EVERY);
            for (pending_index, (idx, key)) in pending.into_iter().enumerate() {
                // rate_song is idempotent server-side, so a crash between the call and
                // the checkpoint flush re-likes harmlessly on resume.
                ctx.ytm()?
                    .rate_song_liked(&key)
                    .await
                    .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
                cp.tracks[idx].written = true;
                journal_batch.push(idx);
                progress_counts.record_written(1);
                report.written += 1;
                done += 1;
                progress(progress_write(
                    cp,
                    done,
                    total,
                    idx,
                    report.auto_accepted,
                    progress_counts,
                ));
                if journal_batch.len() >= CHECKPOINT_EVERY {
                    append_write_journal(cp, &journal_batch)?;
                    journal_batch.clear();
                }
                if pending_index + 1 < pending_count {
                    tokio::time::sleep(YTM_LIKE_PAUSE).await;
                }
            }
            append_write_journal(cp, &journal_batch)?;
        }
        TransferDest::YtmNewPlaylist { .. } | TransferDest::YtmExistingPlaylist { .. } => {
            let destination = ensure_ytm_dest(cp, ctx).await?;
            if !destination.created_in_run {
                reconcile_written_ytm(cp, ctx, &destination.id).await?;
            }
            let pending: Vec<(usize, String)> = writes
                .into_iter()
                .filter(|(idx, _)| !cp.tracks[*idx].written)
                .collect();
            let mut done = total - pending.len() as u32;
            report.written += done;
            let mut progress_counts = WriteProgressCounts::from_checkpoint(cp);
            let chunks = pending.chunks(YTM_ADD_CHUNK);
            let chunk_count = chunks.len();
            for (chunk_index, chunk) in chunks.enumerate() {
                let ids: Vec<String> = chunk.iter().map(|(_, key)| key.clone()).collect();
                ctx.ytm()?
                    .add_items_to_account_playlist(&destination.id, &ids)
                    .await
                    .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
                for (idx, _) in chunk {
                    cp.tracks[*idx].written = true;
                }
                append_write_journal(cp, &chunk.iter().map(|(idx, _)| *idx).collect::<Vec<_>>())?;
                progress_counts.record_written(chunk.len());
                done += chunk.len() as u32;
                report.written += chunk.len() as u32;
                progress(progress_write(
                    cp,
                    done,
                    total,
                    chunk.last().unwrap().0,
                    report.auto_accepted,
                    progress_counts,
                ));
                if should_pause_after_chunk(chunk_index, chunk_count) {
                    tokio::time::sleep(YTM_ADD_PAUSE).await;
                }
            }
        }
        TransferDest::SpotifyNewPlaylist { .. } => {
            let destination = ensure_spotify_dest(cp, ctx).await?;
            if !destination.created_in_run {
                reconcile_written_spotify(cp, ctx, &destination.id).await?;
            }
            let pending: Vec<(usize, String)> = writes
                .into_iter()
                .filter(|(idx, _)| !cp.tracks[*idx].written)
                .collect();
            let mut done = total - pending.len() as u32;
            report.written += done;
            let mut progress_counts = WriteProgressCounts::from_checkpoint(cp);
            for chunk in pending.chunks(SPOTIFY_ADD_CHUNK) {
                let uris: Vec<String> = chunk.iter().map(|(_, key)| key.clone()).collect();
                {
                    let spotify = ctx.spotify()?;
                    spotify
                        .add_tracks(&destination.id, &uris)
                        .await
                        .map_err(|e| checkpointed(cp, spotify_job_error(e)))?;
                }
                for (idx, _) in chunk {
                    cp.tracks[*idx].written = true;
                }
                append_write_journal(cp, &chunk.iter().map(|(idx, _)| *idx).collect::<Vec<_>>())?;
                progress_counts.record_written(chunk.len());
                done += chunk.len() as u32;
                report.written += chunk.len() as u32;
                progress(progress_write(
                    cp,
                    done,
                    total,
                    chunk.last().unwrap().0,
                    report.auto_accepted,
                    progress_counts,
                ));
            }
        }
        TransferDest::SpotifyMirrorPlaylist { .. } => {
            spotify_sync::write(cp, ctx, writes, progress, report).await?;
        }
        TransferDest::File { .. } => unreachable!("file exports take the export_file path"),
        TransferDest::LocalPlaylist { .. } => unreachable!("handled before the client arms"),
    }
    Ok(())
}

fn should_pause_after_chunk(chunk_index: usize, chunk_count: usize) -> bool {
    chunk_index < chunk_count.saturating_sub(1)
}

fn collect_writes_without_deduping(cp: &Checkpoint) -> Vec<(usize, String)> {
    let mut writes = Vec::new();
    for (idx, entry) in cp.tracks.iter().enumerate() {
        let Some(key) = effective_write_key(entry, cp.spec.take_best) else {
            continue;
        };
        writes.push((idx, key.to_owned()));
    }
    writes
}

fn collect_writes(cp: &Checkpoint, report: &mut TransferReport) -> Vec<(usize, String)> {
    // Collect the ordered, deduped write list: (entry index, destination key).
    let mut seen: HashSet<String> = HashSet::new();
    let mut writes: Vec<(usize, String)> = Vec::new();
    for (idx, entry) in cp.tracks.iter().enumerate() {
        let Some(key) = effective_write_key(entry, cp.spec.take_best) else {
            continue;
        };
        if !seen.insert(key.to_owned()) {
            report.duplicates_dropped += 1;
            continue;
        }
        writes.push((idx, key.to_owned()));
    }
    writes
}

fn best_safe_ambiguous_candidate(
    candidates: &[super::matching::AmbiguousCandidate],
) -> Option<&super::matching::AmbiguousCandidate> {
    candidates
        .iter()
        .filter(|candidate| {
            candidate
                .score_breakdown
                .as_ref()
                .is_none_or(|score| !score.accept_blocked && score.reject_reason.is_none())
        })
        .max_by(|left, right| left.score.total_cmp(&right.score))
}

/// The exact destination key a row would write under the current review/take-best policy.
/// Capacity accounting and write collection deliberately share this function so a review row
/// cannot pass preflight and then overflow the local store later.
fn effective_write_key(entry: &TrackEntry, take_best: bool) -> Option<&str> {
    match &entry.review_decision {
        Some(ReviewDecision::Rejected | ReviewDecision::Skipped) => return None,
        Some(ReviewDecision::Accepted { key, .. }) => return Some(key),
        None => {}
    }
    match &entry.outcome {
        Some(MatchOutcome::Matched { key, .. }) => Some(key),
        Some(MatchOutcome::Ambiguous { candidates }) if take_best => {
            best_safe_ambiguous_candidate(candidates).map(|candidate| candidate.key.as_str())
        }
        _ => None,
    }
}

/// Create (once) or find the YTM destination playlist; the id is checkpointed before
/// the first add so a resume never creates a duplicate.
async fn ensure_ytm_dest(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
) -> Result<EnsuredDestination, JobError> {
    if let Some(id) = &cp.dest_id {
        return Ok(EnsuredDestination {
            id: id.clone(),
            created_in_run: false,
        });
    }
    let ytm = ctx.ytm()?;
    let existing = ytm
        .library_playlists()
        .await
        .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
    let dest = cp.spec.dest.clone();
    let (id, created_in_run) = match &dest {
        // Append-or-create: a `--to-playlist NAME` target that doesn't exist yet is
        // simply created under that exact name (no collision suffixing).
        TransferDest::YtmExistingPlaylist { name } => {
            match existing.iter().find(|(_, title, _)| title == name) {
                Some((id, _, _)) => (id.clone(), false),
                None => {
                    let description = format!("Imported by YuTuTui! (job {})", cp.job_id);
                    let id = ytm
                        .create_account_playlist(name, &description)
                        .await
                        .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
                    cp.dest_name = Some(name.clone());
                    (id, true)
                }
            }
        }
        _ => {
            let base = cp
                .dest_name
                .clone()
                .unwrap_or_else(|| "Imported playlist".to_owned());
            // Title collision policy: suffix rather than merging into someone's list.
            let mut name = base.clone();
            if existing.iter().any(|(_, title, _)| *title == name) {
                name = format!("{base} (Spotify import)");
            }
            if existing.iter().any(|(_, title, _)| *title == name) {
                name = format!("{base} (Spotify import {})", crate::signals::unix_now());
            }
            let description = format!("Imported by YuTuTui! (job {})", cp.job_id);
            let id = ytm
                .create_account_playlist(&name, &description)
                .await
                .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
            cp.dest_name = Some(name);
            (id, true)
        }
    };
    cp.dest_id = Some(id.clone());
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    Ok(EnsuredDestination { id, created_in_run })
}

async fn ensure_spotify_dest(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
) -> Result<EnsuredDestination, JobError> {
    if let Some(id) = &cp.dest_id {
        return Ok(EnsuredDestination {
            id: id.clone(),
            created_in_run: false,
        });
    }
    let name = cp
        .dest_name
        .clone()
        .unwrap_or_else(|| "Imported playlist".to_owned());
    let description = format!(
        "Imported from YouTube Music by YuTuTui! (job {})",
        cp.job_id
    );
    let spotify = ctx.spotify()?;
    let current_user_id = spotify
        .me()
        .await
        .map_err(|e| checkpointed(cp, spotify_job_error(e)))?
        .id;
    // Prefer an editable exact-name target. Followed read-only playlists can appear in
    // `/me/playlists`, so name equality alone is not enough for a write destination.
    let existing = spotify
        .my_playlists()
        .await
        .map_err(|e| checkpointed(cp, spotify_job_error(e)))?
        .into_iter()
        .find(|p| {
            p.name == name
                && (p.owner_id.as_deref() == Some(current_user_id.as_str()) || p.collaborative)
        });
    let (id, created_in_run) = match existing {
        Some(p) => (p.id, false),
        None => match spotify.create_playlist(&name, &description).await {
            Ok(id) => (id, true),
            Err(e) => return Err(checkpointed(cp, spotify_job_error(e))),
        },
    };
    cp.dest_id = Some(id.clone());
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    Ok(EnsuredDestination { id, created_in_run })
}

/// Belt-and-braces resume idempotency: whatever already made it into the destination
/// (a crash can land between a successful add and the checkpoint flush) is marked
/// written before any new adds.
async fn reconcile_written_ytm(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    dest_id: &str,
) -> Result<(), JobError> {
    let songs = ctx
        .ytm()?
        .playlist_tracks_full(dest_id)
        .await
        .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
    let present: HashSet<String> = songs.into_iter().map(|song| song.video_id).collect();
    if present.is_empty() {
        return Ok(());
    }
    for entry in &mut cp.tracks {
        if let Some(MatchOutcome::Matched { key, .. }) = &entry.outcome
            && present.contains(key)
        {
            entry.written = true;
        }
    }
    Ok(())
}

async fn reconcile_written_spotify(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    dest_id: &str,
) -> Result<(), JobError> {
    let mut on_page = |_done: u32, _total: u32| {};
    let tracks = {
        let spotify = ctx.spotify()?;
        spotify.playlist_tracks(dest_id, &mut on_page).await
    }
    .map_err(|e| checkpointed(cp, spotify_job_error(e)))?;
    let present: HashSet<String> = tracks.into_iter().map(|track| track.uri).collect();
    if present.is_empty() {
        return Ok(());
    }
    for entry in &mut cp.tracks {
        if let Some(MatchOutcome::Matched { key, .. }) = &entry.outcome
            && present.contains(key)
        {
            entry.written = true;
        }
    }
    Ok(())
}

// File export ------------------------------------------------------------------------

async fn export_file(
    job_id: &str,
    spec: &JobSpec,
    ctx: &mut JobCtx,
    local_snapshot: Option<&crate::playlists::Playlists>,
    path: &std::path::Path,
    format: FileFormat,
    started: Instant,
) -> Result<TransferReport, JobError> {
    let (name, source_label, songs): (String, String, Vec<Song>) = match &spec.source {
        TransferSource::YtmPlaylist { id } => {
            let ytm = ctx.ytm()?;
            let title = ytm
                .library_playlists()
                .await
                .map_err(ytm_job_error)?
                .into_iter()
                .find(|(pid, _, _)| pid == id)
                .map(|(_, title, _)| title)
                .unwrap_or_else(|| format!("ytm:{id}"));
            let songs = ytm.playlist_tracks_full(id).await.map_err(ytm_job_error)?;
            (title, format!("ytm:{id}"), songs)
        }
        TransferSource::LocalPlaylist { key } => {
            let playlist = local_snapshot
                .and_then(|store| store.find(key))
                .ok_or_else(|| JobError::fatal(anyhow!("no local playlist named `{key}`")))?;
            (
                playlist.name.clone(),
                format!("local:{key}"),
                playlist.songs.clone(),
            )
        }
        other => {
            return Err(JobError::fatal(anyhow!(
                "file export supports YouTube Music / local playlists (got {other:?})"
            )));
        }
    };
    match format {
        FileFormat::Json => {
            let file = super::json::PlaylistFile::new(name, source_label, songs.clone());
            super::json::write_playlist(path, &file).map_err(JobError::fatal)?;
        }
        FileFormat::Csv => {
            super::csv::write_songs(path, &songs).map_err(JobError::fatal)?;
        }
    }
    Ok(TransferReport {
        job_id: job_id.to_owned(),
        total: songs.len() as u32,
        matched: songs.len() as u32,
        written: songs.len() as u32,
        elapsed_secs: started.elapsed().as_secs(),
        ..TransferReport::default()
    })
}

fn progress_beat(job_id: &str, stage: Stage) -> impl FnMut(u32, u32, String) + use<> {
    let job_id = job_id.to_owned();
    move |_done, _total, _current| {
        // Fetch pagination is fast; per-page progress is deliberately not surfaced (the
        // interesting stages report per-track). Hook kept for symmetry/debugging.
        tracing::debug!(job = %job_id, stage = stage.label(), "fetch page");
    }
}

#[cfg(test)]
mod tests;
