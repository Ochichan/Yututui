//! The transfer engine: fetch → match → write, with a checkpoint flush at every
//! meaningful boundary so any abort resumes idempotently. One plain async fn — the CLI
//! runs it on its own runtime, the TUI actor on a worker task.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};

use super::checkpoint::ReviewDecision;
use super::checkpoint::{Checkpoint, ReportCandidate, ReportRow, TrackEntry, TransferReport};
use super::matching::{
    MatchCandidate, MatchConfig, MatchOutcome, Pacing, TrackInput, YtmMatchState, best_outcome,
    match_track_ytm, spotify_query_plan, ytm_query_plan,
};
use super::{FileFormat, JobSpec, Stage, TransferDest, TransferProgress, TransferSource};
use crate::api::ytmusic::YtMusicApi;
use crate::api::{Song, SongImportMetadata};
use crate::search_source::SearchConfig;
use crate::spotify::client::{SpotifyClient, SpotifyError};

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
    let started = Instant::now();
    if let Some(spotify) = ctx.spotify.as_mut() {
        spotify.reset_rate_budget();
    }

    // File exports never match anything — a straight fetch-and-write.
    if let TransferDest::File { path, format } = &spec.dest {
        let (path, format) = (path.clone(), *format);
        return export_file(&job_id, &spec, ctx, &path, format, started).await;
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
            let (source_name, entries, skipped_local) = fetch_source(&job_id, &spec, ctx).await?;
            let mut cp = Checkpoint::new(job_id.clone(), spec.clone(), entries);
            cp.source_name = Some(source_name.clone());
            cp.dest_name = Some(default_dest_name(&spec.dest, &source_name));
            cp.skipped_local = skipped_local;
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
        match_stage(&mut cp, ctx, progress).await?;
        cp.stage = Stage::Writing;
        cp.save().map_err(|e| JobError::fatal(e.into()))?;
        save_import_session(&cp);
    }

    let mut report = build_report(&cp, skipped_local);

    // Stage 3 — write (skipped on dry-run: the checkpoint keeps the matches, and a
    // later `resume` performs exactly the writes).
    if !cp.spec.dry_run {
        write_stage(&mut cp, ctx, progress, &mut report).await?;
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

fn default_dest_name(dest: &TransferDest, source_name: &str) -> String {
    match dest {
        TransferDest::YtmNewPlaylist { name }
        | TransferDest::SpotifyNewPlaylist { name }
        | TransferDest::LocalPlaylist { name } => name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| source_name.to_owned()),
        TransferDest::YtmExistingPlaylist { name } => name.clone(),
        TransferDest::YtmLikes => "Liked Music".to_owned(),
        TransferDest::File { path, .. } => path.display().to_string(),
    }
}

// Fetch ---------------------------------------------------------------------------

async fn fetch_source(
    job_id: &str,
    spec: &JobSpec,
    ctx: &mut JobCtx,
) -> Result<(String, Vec<TrackEntry>, u32), JobError> {
    let mut beat = progress_beat(job_id, Stage::Fetching);
    let (name, inputs, skipped_local): (String, Vec<TrackInput>, u32) = match &spec.source {
        TransferSource::SpotifyPlaylist { id } => {
            let spotify = ctx.spotify()?;
            let meta = spotify.playlist_meta(id).await.map_err(spotify_job_error)?;
            let mut on_page = |done: u32, total: u32| beat(done, total, meta.name.clone());
            let tracks = spotify
                .playlist_tracks(id, &mut on_page)
                .await
                .map_err(spotify_job_error)?;
            let skipped = meta.total.saturating_sub(tracks.len() as u32);
            (
                meta.name,
                tracks.iter().map(TrackInput::from_spotify).collect(),
                skipped,
            )
        }
        TransferSource::SpotifyLiked => {
            let spotify = ctx.spotify()?;
            let mut on_page = |done: u32, total: u32| beat(done, total, "Liked Songs".to_owned());
            let mut tracks = spotify
                .liked_tracks(&mut on_page)
                .await
                .map_err(spotify_job_error)?;
            // Spotify returns newest-first; import oldest-first so the YTM like
            // timeline ends up in the original order.
            tracks.reverse();
            (
                "Spotify Liked Songs".to_owned(),
                tracks.iter().map(TrackInput::from_spotify).collect(),
                0,
            )
        }
        TransferSource::YtmPlaylist { id } => {
            let ytm = ctx.ytm()?;
            let name = ytm
                .library_playlists()
                .await
                .map_err(JobError::fatal)?
                .into_iter()
                .find(|(pid, _, _)| pid == id)
                .map(|(_, title, _)| title)
                .unwrap_or_else(|| format!("ytm:{id}"));
            let songs = ytm.playlist_tracks_full(id).await.map_err(ytm_job_error)?;
            (name, songs.iter().map(TrackInput::from_song).collect(), 0)
        }
        TransferSource::LocalPlaylist { key } => {
            let store = crate::playlists::Playlists::load();
            let playlist = store
                .find(key)
                .ok_or_else(|| JobError::fatal(anyhow!("no local playlist named `{key}`")))?;
            (
                playlist.name.clone(),
                playlist.songs.iter().map(TrackInput::from_song).collect(),
                0,
            )
        }
        TransferSource::File { path } => {
            let (name, inputs) = read_file_inputs(path).map_err(JobError::fatal)?;
            (name, inputs, 0)
        }
    };
    let mut inputs = inputs;
    if inputs.len() > TRACK_CAP {
        tracing::warn!(
            dropped = inputs.len() - TRACK_CAP,
            "transfer capped at {TRACK_CAP} tracks"
        );
        inputs.truncate(TRACK_CAP);
    }
    let entries = inputs
        .into_iter()
        .map(|input| TrackEntry {
            input,
            outcome: None,
            review_decision: None,
            written: false,
        })
        .collect();
    Ok((name, entries, skipped_local))
}

fn read_file_inputs(path: &std::path::Path) -> anyhow::Result<(String, Vec<TrackInput>)> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "json" => {
            let file = super::json::read_playlist(path)?;
            let inputs = file.to_track_inputs();
            Ok((file.name, inputs))
        }
        "csv" => {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Imported playlist")
                .to_owned();
            Ok((name, super::csv::read_tracks(path)?))
        }
        _ => bail!("unsupported file type `{ext}` — use .json (yututui) or .csv (Exportify)"),
    }
}

// Match ---------------------------------------------------------------------------

async fn match_stage(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
) -> Result<(), JobError> {
    let cfg = MatchConfig {
        accept: cp.spec.min_score,
        ambiguous_floor: (cp.spec.min_score - 0.20).max(0.40),
        accept_margin: MatchConfig::default().accept_margin,
    };
    let to_spotify = matches!(cp.spec.dest, TransferDest::SpotifyNewPlaylist { .. });
    let total = cp.tracks.len() as u32;
    let mut memo = std::collections::HashMap::new();
    let mut query_memo = std::collections::HashMap::new();
    let mut video_memo = std::collections::HashMap::new();
    let mut pace = Pacing::ytm_default();
    let search_config = ctx.search_config.clone();
    let market = ctx.market.clone();
    // One weird track must not kill a 300-track job: an isolated search failure
    // becomes NotFound (reported, retryable via --rematch). A *streak* means the
    // backend itself died (cookie, network) — abort resumably then.
    let mut consecutive_failures = 0u32;

    for idx in 0..cp.tracks.len() {
        if cp.tracks[idx].outcome.is_some() {
            continue; // resumed work
        }
        let input = cp.tracks[idx].input.clone();
        let outcome = if to_spotify {
            // Spotify's client already absorbs transient trouble internally; anything
            // surfacing here (rate budget, auth) is systemic — abort resumably.
            match_track_spotify(ctx.spotify()?, &input, &cfg, market.clone())
                .await
                .map_err(|e| checkpointed(cp, e))?
        } else {
            let mut ytm_state =
                YtmMatchState::new(&mut memo, &mut query_memo, &mut video_memo, &mut pace);
            match match_track_ytm(ctx.ytm()?, &input, &cfg, &search_config, &mut ytm_state).await {
                Ok(outcome) => {
                    consecutive_failures = 0;
                    outcome
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= 5 {
                        return Err(checkpointed(cp, ytm_job_error(e)));
                    }
                    tracing::warn!(
                        track = %input.display(),
                        error = %crate::util::sanitize::sanitize_error_text(format!("{e:#}")),
                        "match lookup failed; recording as not-found"
                    );
                    MatchOutcome::NotFound
                }
            }
        };
        cp.tracks[idx].outcome = Some(outcome);

        let (matched, ambiguous, not_found) = outcome_counts(&cp.tracks);
        progress(TransferProgress {
            job_id: cp.job_id.clone(),
            stage: Stage::Matching,
            done: (idx + 1) as u32,
            total,
            matched,
            ambiguous,
            not_found,
            current: input.display(),
        });
        if (idx + 1) % CHECKPOINT_EVERY == 0 {
            cp.save().map_err(|e| JobError::fatal(e.into()))?;
        }
    }
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    Ok(())
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
    progress: &mut (dyn FnMut(TransferProgress) + Send),
    report: &mut TransferReport,
) -> Result<(), JobError> {
    let writes = collect_writes(cp, report);
    let total = writes.len() as u32;

    // The local store needs no network at all — handle it before the client-bound arms.
    if matches!(cp.spec.dest, TransferDest::LocalPlaylist { .. }) {
        return write_local(cp, writes, progress, report);
    }

    match cp.spec.dest.clone() {
        TransferDest::YtmLikes => {
            let mut done = 0u32;
            for (idx, key) in writes {
                if cp.tracks[idx].written {
                    done += 1;
                    continue;
                }
                // rate_song is idempotent server-side, so a crash between the call and
                // the checkpoint flush re-likes harmlessly on resume.
                ctx.ytm()?
                    .rate_song_liked(&key)
                    .await
                    .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
                cp.tracks[idx].written = true;
                report.written += 1;
                done += 1;
                progress(progress_write(cp, done, total, idx));
                if (done as usize).is_multiple_of(CHECKPOINT_EVERY) {
                    cp.save().map_err(|e| JobError::fatal(e.into()))?;
                }
                tokio::time::sleep(YTM_LIKE_PAUSE).await;
            }
        }
        TransferDest::YtmNewPlaylist { .. } | TransferDest::YtmExistingPlaylist { .. } => {
            let dest_id = ensure_ytm_dest(cp, ctx).await?;
            reconcile_written_ytm(cp, ctx, &dest_id).await?;
            let pending: Vec<(usize, String)> = writes
                .into_iter()
                .filter(|(idx, _)| !cp.tracks[*idx].written)
                .collect();
            let mut done = total - pending.len() as u32;
            report.written += done;
            for chunk in pending.chunks(YTM_ADD_CHUNK) {
                let ids: Vec<String> = chunk.iter().map(|(_, key)| key.clone()).collect();
                ctx.ytm()?
                    .add_items_to_account_playlist(&dest_id, &ids)
                    .await
                    .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
                for (idx, _) in chunk {
                    cp.tracks[*idx].written = true;
                }
                done += chunk.len() as u32;
                report.written += chunk.len() as u32;
                progress(progress_write(cp, done, total, chunk.last().unwrap().0));
                cp.save().map_err(|e| JobError::fatal(e.into()))?;
                tokio::time::sleep(YTM_ADD_PAUSE).await;
            }
        }
        TransferDest::SpotifyNewPlaylist { .. } => {
            let dest_id = ensure_spotify_dest(cp, ctx).await?;
            reconcile_written_spotify(cp, ctx, &dest_id).await?;
            let pending: Vec<(usize, String)> = writes
                .into_iter()
                .filter(|(idx, _)| !cp.tracks[*idx].written)
                .collect();
            let mut done = total - pending.len() as u32;
            report.written += done;
            for chunk in pending.chunks(SPOTIFY_ADD_CHUNK) {
                let uris: Vec<String> = chunk.iter().map(|(_, key)| key.clone()).collect();
                {
                    let spotify = ctx.spotify()?;
                    spotify
                        .add_tracks(&dest_id, &uris)
                        .await
                        .map_err(|e| checkpointed(cp, spotify_job_error(e)))?;
                }
                for (idx, _) in chunk {
                    cp.tracks[*idx].written = true;
                }
                done += chunk.len() as u32;
                report.written += chunk.len() as u32;
                progress(progress_write(cp, done, total, chunk.last().unwrap().0));
                cp.save().map_err(|e| JobError::fatal(e.into()))?;
            }
        }
        TransferDest::File { .. } => unreachable!("file exports take the export_file path"),
        TransferDest::LocalPlaylist { .. } => unreachable!("handled before the client arms"),
    }
    Ok(())
}

fn collect_writes(cp: &Checkpoint, report: &mut TransferReport) -> Vec<(usize, String)> {
    // Collect the ordered, deduped write list: (entry index, destination key).
    let mut seen: HashSet<String> = HashSet::new();
    let mut writes: Vec<(usize, String)> = Vec::new();
    for (idx, entry) in cp.tracks.iter().enumerate() {
        let key = match (&entry.review_decision, &entry.outcome, cp.spec.take_best) {
            (Some(ReviewDecision::Rejected | ReviewDecision::Skipped), _, _) => continue,
            (Some(ReviewDecision::Accepted { key, .. }), _, _) => key.clone(),
            (_, Some(MatchOutcome::Matched { key, .. }), _) => key.clone(),
            (_, Some(MatchOutcome::Ambiguous { candidates }), true) => {
                match candidates.iter().find(|candidate| {
                    candidate
                        .score_breakdown
                        .as_ref()
                        .is_none_or(|score| !score.accept_blocked && score.reject_reason.is_none())
                }) {
                    Some(best) => best.key.clone(),
                    None => continue,
                }
            }
            _ => continue,
        };
        if !seen.insert(key.clone()) {
            report.duplicates_dropped += 1;
            continue;
        }
        writes.push((idx, key));
    }
    writes
}

/// Write matches into the app's own playlist store (`playlists.json`): visible in the
/// TUI Library immediately. Create-or-append by name; the store itself dedupes by
/// `video_id`, so resumes and re-runs are naturally idempotent.
fn write_local(
    cp: &mut Checkpoint,
    writes: Vec<(usize, String)>,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
    report: &mut TransferReport,
) -> Result<(), JobError> {
    let name = cp
        .dest_name
        .clone()
        .unwrap_or_else(|| "Imported playlist".to_owned());
    let mut store = crate::playlists::Playlists::load();
    let key = match store.list().iter().find(|p| p.name == name) {
        Some(existing) => existing.id.clone(),
        None => store.create(&name).ok_or_else(|| {
            JobError::fatal(anyhow!("could not create the local playlist `{name}`"))
        })?,
    };
    cp.dest_id = Some(key.clone());

    let total = writes.len() as u32;
    let mut done = 0u32;
    for (idx, video_id) in writes {
        let song = song_for_entry(&cp.tracks[idx], &video_id, &cp.job_id, idx as u32 + 1);
        match store.add(&key, song) {
            crate::playlists::AddResult::Added | crate::playlists::AddResult::Duplicate => {
                cp.tracks[idx].written = true;
                report.written += 1;
            }
            crate::playlists::AddResult::Full => {
                tracing::warn!("local playlist `{name}` is full; remaining tracks skipped");
                break;
            }
            crate::playlists::AddResult::NotFound => {
                return Err(JobError::fatal(anyhow!(
                    "local playlist `{name}` vanished mid-write"
                )));
            }
        }
        done += 1;
        progress(progress_write(cp, done, total, idx));
    }
    store
        .save()
        .map_err(|e| JobError::fatal(anyhow::Error::from(e).context("saving playlists.json")))?;
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    Ok(())
}

/// Reconstruct a playable `Song` for a write target. The YouTube candidate supplies the
/// playable key; the source input supplies canonical music metadata so downloaded imports
/// carry Spotify/CSV album grouping and track ordering into Local Deck.
fn song_for_entry(entry: &TrackEntry, video_id: &str, job_id: &str, source_order: u32) -> Song {
    let (matched_title, matched_artist, matched_album, matched_duration_secs) = match &entry.outcome
    {
        Some(MatchOutcome::Matched {
            title,
            artist,
            album,
            duration_secs,
            ..
        }) => (title.clone(), artist.clone(), album.clone(), *duration_secs),
        _ => (None, None, None, None),
    };
    let title = if entry.input.title.trim().is_empty() {
        matched_title.unwrap_or_default()
    } else {
        entry.input.title.clone()
    };
    let artist = if entry.input.artists.is_empty() {
        matched_artist.unwrap_or_default()
    } else {
        entry.input.artists.join(", ")
    };
    let album = entry.input.album.clone().or(matched_album);
    let duration_secs = entry.input.duration_secs.or(matched_duration_secs);
    let duration = duration_secs
        .map(|s| crate::util::format::time(f64::from(s)))
        .unwrap_or_default();
    let mut song = Song::from_search(video_id, title, artist, duration, album);
    song.duration_secs = duration_secs;
    let album_artist =
        (!entry.input.album_artists.is_empty()).then(|| entry.input.album_artists.join(", "));
    song.with_catalog_metadata(
        album_artist,
        entry.input.disc_number,
        entry.input.track_number,
        entry.input.isrc.clone(),
        Some(entry.input.source_key.clone()),
        entry.input.source_url.clone(),
    )
    .with_import_metadata(SongImportMetadata {
        artists: entry.input.artists.clone(),
        album_artists: entry.input.album_artists.clone(),
        album_release_date: entry.input.album_release_date.clone(),
        album_release_date_precision: entry.input.album_release_date_precision.clone(),
        album_total_tracks: entry.input.album_total_tracks,
        album_type: entry.input.album_type.clone(),
        album_art_url: entry.input.album_art_url.clone(),
        explicit: entry.input.explicit,
    })
    .with_import_session(Some(job_id.to_owned()), Some(source_order))
}

/// Create (once) or find the YTM destination playlist; the id is checkpointed before
/// the first add so a resume never creates a duplicate.
async fn ensure_ytm_dest(cp: &mut Checkpoint, ctx: &mut JobCtx) -> Result<String, JobError> {
    if let Some(id) = &cp.dest_id {
        return Ok(id.clone());
    }
    let ytm = ctx.ytm()?;
    let existing = ytm
        .library_playlists()
        .await
        .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
    let dest = cp.spec.dest.clone();
    let id = match &dest {
        // Append-or-create: a `--to-playlist NAME` target that doesn't exist yet is
        // simply created under that exact name (no collision suffixing).
        TransferDest::YtmExistingPlaylist { name } => {
            match existing.iter().find(|(_, title, _)| title == name) {
                Some((id, _, _)) => id.clone(),
                None => {
                    let description = format!("Imported by YuTuTui! (job {})", cp.job_id);
                    let id = ytm
                        .create_account_playlist(name, &description)
                        .await
                        .map_err(|e| checkpointed(cp, ytm_job_error(e)))?;
                    cp.dest_name = Some(name.clone());
                    id
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
            id
        }
    };
    cp.dest_id = Some(id.clone());
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    Ok(id)
}

async fn ensure_spotify_dest(cp: &mut Checkpoint, ctx: &mut JobCtx) -> Result<String, JobError> {
    if let Some(id) = &cp.dest_id {
        return Ok(id.clone());
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
    // Prefer an existing playlist with the exact name: Development Mode apps created
    // after the March 2026 policy can't create playlists at all, but appending to the
    // user's existing ones works fine.
    let existing = spotify
        .my_playlists()
        .await
        .map_err(|e| checkpointed(cp, spotify_job_error(e)))?
        .into_iter()
        .find(|p| p.name == name);
    let id = match existing {
        Some(p) => p.id,
        None => match spotify.create_playlist(&name, &description).await {
            Ok(id) => id,
            Err(SpotifyError::NotAllowlisted) => {
                return Err(checkpointed(
                    cp,
                    JobError {
                        resumable: true,
                        error: anyhow!(
                            "Spotify blocks playlist creation for new apps — create an empty \
                             playlist named \"{name}\" in Spotify yourself, then resume this job"
                        ),
                    },
                ));
            }
            Err(e) => return Err(checkpointed(cp, spotify_job_error(e))),
        },
    };
    cp.dest_id = Some(id.clone());
    cp.save().map_err(|e| JobError::fatal(e.into()))?;
    Ok(id)
}

/// Belt-and-braces resume idempotency: whatever already made it into the destination
/// (a crash can land between a successful add and the checkpoint flush) is marked
/// written before any new adds.
async fn reconcile_written_ytm(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    dest_id: &str,
) -> Result<(), JobError> {
    if !cp.tracks.iter().any(|t| t.written) && cp.stage == Stage::Writing {
        // Fresh write stage on a fresh playlist: nothing to reconcile unless resuming.
        if cp.dest_id.is_none() {
            return Ok(());
        }
    }
    let present: HashSet<String> = ctx
        .ytm()?
        .playlist_tracks_full(dest_id)
        .await
        .map(|songs| songs.into_iter().map(|s| s.video_id).collect())
        .unwrap_or_default();
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
    let spotify = ctx.spotify()?;
    let mut on_page = |_done: u32, _total: u32| {};
    let present: HashSet<String> = spotify
        .playlist_tracks(dest_id, &mut on_page)
        .await
        .map(|tracks| tracks.into_iter().map(|t| t.uri).collect())
        .unwrap_or_default();
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
            let store = crate::playlists::Playlists::load();
            let playlist = store
                .find(key)
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

// Shared helpers -----------------------------------------------------------------------

fn outcome_counts(tracks: &[TrackEntry]) -> (u32, u32, u32) {
    let mut matched = 0;
    let mut ambiguous = 0;
    let mut not_found = 0;
    for t in tracks {
        match &t.outcome {
            Some(MatchOutcome::Matched { .. }) => matched += 1,
            Some(MatchOutcome::Ambiguous { .. }) => ambiguous += 1,
            Some(MatchOutcome::NotFound) => not_found += 1,
            _ => {}
        }
    }
    (matched, ambiguous, not_found)
}

fn build_report(cp: &Checkpoint, skipped_local: u32) -> TransferReport {
    let (matched, _, _) = outcome_counts(&cp.tracks);
    let searched_ytm = !matches!(cp.spec.dest, TransferDest::SpotifyNewPlaylist { .. });
    let mut report = TransferReport {
        job_id: cp.job_id.clone(),
        total: cp.tracks.len() as u32,
        matched,
        skipped_local,
        ..TransferReport::default()
    };
    for (idx, entry) in cp.tracks.iter().enumerate() {
        match &entry.outcome {
            Some(MatchOutcome::Ambiguous { candidates }) => {
                let report_candidates: Vec<ReportCandidate> = candidates
                    .iter()
                    .map(|c| ReportCandidate {
                        key: c.key.clone(),
                        score: c.score,
                        display: c.display.clone(),
                        score_breakdown: c.score_breakdown.clone(),
                    })
                    .collect();
                let note = candidates
                    .iter()
                    .map(|c| format!("{} ({:.2})", c.display, c.score))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let mut row = report_row_base(entry, idx, note);
                if searched_ytm && entry.input.known_video_id.is_none() {
                    row.search_queries = ytm_query_plan(&entry.input);
                }
                row.selected_key = report_candidates.first().map(|c| c.key.clone());
                row.selected_score = report_candidates.first().map(|c| c.score);
                if let Some(score) = report_candidates
                    .first()
                    .and_then(|candidate| candidate.score_breakdown.as_ref())
                {
                    apply_report_quality(&mut row, score);
                }
                row.candidates = report_candidates;
                report.ambiguous.push(row);
            }
            Some(MatchOutcome::NotFound) => {
                let mut row = report_row_base(entry, idx, "no match on the destination".to_owned());
                if searched_ytm && entry.input.known_video_id.is_none() {
                    row.search_queries = ytm_query_plan(&entry.input);
                }
                report.not_found.push(row);
            }
            _ => {}
        }
    }
    report
}

fn report_row_base(entry: &TrackEntry, idx: usize, note: String) -> ReportRow {
    ReportRow {
        title: entry.input.title.clone(),
        artists: entry.input.artists.join(", "),
        note,
        source_order: Some(idx as u32 + 1),
        source_key: Some(entry.input.source_key.clone()),
        source_url: entry.input.source_url.clone(),
        album: entry.input.album.clone(),
        album_artists: entry.input.album_artists.clone(),
        album_id: entry.input.album_id.clone(),
        album_uri: entry.input.album_uri.clone(),
        album_release_date: entry.input.album_release_date.clone(),
        album_release_date_precision: entry.input.album_release_date_precision.clone(),
        album_total_tracks: entry.input.album_total_tracks,
        album_type: entry.input.album_type.clone(),
        album_art_url: entry.input.album_art_url.clone(),
        disc_number: entry.input.disc_number,
        track_number: entry.input.track_number,
        duration_secs: entry.input.duration_secs,
        isrc: entry.input.isrc.clone(),
        explicit: entry.input.explicit,
        candidates: Vec::new(),
        selected_key: None,
        selected_score: None,
        search_queries: Vec::new(),
        source_kind: None,
        quality_tier: None,
        reject_reason: None,
        reason_codes: Vec::new(),
        duration_delta_secs: None,
    }
}

fn apply_report_quality(row: &mut ReportRow, score: &super::matching::MatchScoreBreakdown) {
    if !score.source_kind.is_empty() {
        row.source_kind = Some(score.source_kind.clone());
    }
    if !score.quality_tier.is_empty() {
        row.quality_tier = Some(score.quality_tier.clone());
    }
    row.reject_reason = score.reject_reason.clone();
    row.reason_codes = score.reason_codes.clone();
    row.duration_delta_secs = score.duration_delta_secs;
}

fn progress_write(cp: &Checkpoint, done: u32, total: u32, idx: usize) -> TransferProgress {
    let (matched, ambiguous, not_found) = outcome_counts(&cp.tracks);
    TransferProgress {
        job_id: cp.job_id.clone(),
        stage: Stage::Writing,
        done,
        total,
        matched,
        ambiguous,
        not_found,
        current: cp
            .tracks
            .get(idx)
            .map(|t| t.input.display())
            .unwrap_or_default(),
    }
}

fn progress_beat(job_id: &str, stage: Stage) -> impl FnMut(u32, u32, String) + use<> {
    let job_id = job_id.to_owned();
    move |_done, _total, _current| {
        // Fetch pagination is fast; per-page progress is deliberately not surfaced (the
        // interesting stages report per-track). Hook kept for symmetry/debugging.
        tracing::debug!(job = %job_id, stage = stage.label(), "fetch page");
    }
}

/// Persist the checkpoint before surfacing an error — the resume story depends on it.
fn checkpointed(cp: &mut Checkpoint, err: JobError) -> JobError {
    if let Err(e) = cp.save() {
        tracing::warn!(error = %e, "could not save checkpoint while failing");
    } else {
        save_import_session(cp);
    }
    err
}

fn save_import_session(cp: &Checkpoint) {
    let session = super::session::ImportSession::from_checkpoint(cp);
    if let Err(e) = session.save() {
        tracing::warn!(error = %e, "could not save import session");
    }
}

fn spotify_job_error(e: SpotifyError) -> JobError {
    let resumable = matches!(
        e,
        SpotifyError::RateLimited | SpotifyError::Network(_) | SpotifyError::Auth(_)
    );
    JobError {
        resumable,
        error: anyhow!("{e}"),
    }
}

fn ytm_job_error(e: anyhow::Error) -> JobError {
    // YTM failures mid-job (expired cookie, throttling) are the classic resume case.
    JobError {
        resumable: true,
        error: e.context(
            "YouTube Music request failed — after fixing the cookie you can resume this job",
        ),
    }
}

#[cfg(test)]
mod tests;
