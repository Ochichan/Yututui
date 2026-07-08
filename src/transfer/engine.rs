//! The transfer engine: fetch → match → write, with a checkpoint flush at every
//! meaningful boundary so any abort resumes idempotently. One plain async fn — the CLI
//! runs it on its own runtime, the TUI actor on a worker task.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};

use super::checkpoint::{Checkpoint, ReportCandidate, ReportRow, TrackEntry, TransferReport};
use super::matching::{
    MatchCandidate, MatchConfig, MatchOutcome, Pacing, TrackInput, best_outcome, match_track_ytm,
    normalize_stripped, ytm_query_plan,
};
use super::{FileFormat, JobSpec, Stage, TransferDest, TransferProgress, TransferSource};
use crate::api::Song;
use crate::api::ytmusic::YtMusicApi;
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
    };
    let to_spotify = matches!(cp.spec.dest, TransferDest::SpotifyNewPlaylist { .. });
    let total = cp.tracks.len() as u32;
    let mut memo = std::collections::HashMap::new();
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
            match match_track_ytm(
                ctx.ytm()?,
                &input,
                &cfg,
                &search_config,
                &mut memo,
                &mut pace,
            )
            .await
            {
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

/// Spotify-direction retrieval: `track:"…" artist:"…"` first, plain text second.
async fn match_track_spotify(
    spotify: &mut SpotifyClient,
    input: &TrackInput,
    cfg: &MatchConfig,
    market: Option<String>,
) -> Result<MatchOutcome, JobError> {
    let stripped = normalize_stripped(&input.title);
    let artist = input.artists.first().cloned().unwrap_or_default();
    let fielded = if artist.is_empty() {
        format!("track:\"{stripped}\"")
    } else {
        format!("track:\"{stripped}\" artist:\"{artist}\"")
    };
    let mut candidates: Vec<MatchCandidate> = spotify
        .search_track(&fielded, market.as_deref())
        .await
        .map_err(spotify_job_error)?
        .iter()
        .map(MatchCandidate::from)
        .collect();
    let mut outcome = best_outcome(input, &candidates, cfg);
    if !matches!(outcome, MatchOutcome::Matched { .. }) {
        let plain = format!("{artist} {}", input.title);
        let more = spotify
            .search_track(plain.trim(), market.as_deref())
            .await
            .map_err(spotify_job_error)?;
        for track in &more {
            if !candidates.iter().any(|c| c.key == track.uri) {
                candidates.push(MatchCandidate::from(track));
            }
        }
        outcome = best_outcome(input, &candidates, cfg);
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
    // Collect the ordered, deduped write list: (entry index, destination key).
    let mut seen: HashSet<String> = HashSet::new();
    let mut writes: Vec<(usize, String)> = Vec::new();
    for (idx, entry) in cp.tracks.iter().enumerate() {
        let key = match (&entry.outcome, cp.spec.take_best) {
            (Some(MatchOutcome::Matched { key, .. }), _) => key.clone(),
            (Some(MatchOutcome::Ambiguous { candidates }), true) => match candidates.first() {
                Some(best) => best.key.clone(),
                None => continue,
            },
            _ => continue,
        };
        if !seen.insert(key.clone()) {
            report.duplicates_dropped += 1;
            continue;
        }
        writes.push((idx, key));
    }
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
                        score_breakdown: c.score_breakdown,
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
        disc_number: entry.input.disc_number,
        track_number: entry.input.track_number,
        duration_secs: entry.input.duration_secs,
        isrc: entry.input.isrc.clone(),
        explicit: entry.input.explicit,
        candidates: Vec::new(),
        selected_key: None,
        selected_score: None,
        search_queries: Vec::new(),
    }
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
mod tests {
    use super::*;
    use crate::transfer::matching::{AmbiguousCandidate, MatchScoreBreakdown};

    fn spec(dest: TransferDest) -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyLiked,
            dest,
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        }
    }

    fn input(title: &str, artists: &[&str]) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: artists.iter().map(|s| (*s).to_owned()).collect(),
            album_artists: Vec::new(),
            album: Some("Input Album".to_owned()),
            album_id: None,
            album_uri: None,
            album_release_date: None,
            disc_number: None,
            track_number: None,
            duration_secs: Some(62),
            isrc: None,
            explicit: None,
            source_url: None,
            source_key: format!("src:{title}"),
            known_video_id: None,
        }
    }

    fn entry(input: TrackInput, outcome: Option<MatchOutcome>) -> TrackEntry {
        TrackEntry {
            input,
            outcome,
            written: false,
        }
    }

    fn matched(key: &str) -> MatchOutcome {
        MatchOutcome::Matched {
            key: key.to_owned(),
            score: 0.91,
            display: format!("Matched {key}"),
            title: None,
            artist: None,
            album: None,
            duration_secs: None,
            score_breakdown: None,
        }
    }

    fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).expect("random temp suffix");
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-engine-{name}-{}-{suffix}.{ext}",
            std::process::id()
        ))
    }

    #[test]
    fn default_dest_name_uses_explicit_names_and_source_fallbacks() {
        assert_eq!(
            default_dest_name(
                &TransferDest::YtmNewPlaylist {
                    name: Some("Road Trip".to_owned()),
                },
                "Source"
            ),
            "Road Trip"
        );
        assert_eq!(
            default_dest_name(&TransferDest::YtmNewPlaylist { name: None }, "Source"),
            "Source"
        );
        assert_eq!(
            default_dest_name(
                &TransferDest::LocalPlaylist {
                    name: Some("   ".to_owned()),
                },
                "Local Source"
            ),
            "Local Source"
        );
        assert_eq!(
            default_dest_name(
                &TransferDest::YtmExistingPlaylist {
                    name: "Existing".to_owned(),
                },
                "Ignored"
            ),
            "Existing"
        );
        assert_eq!(
            default_dest_name(&TransferDest::YtmLikes, "Ignored"),
            "Liked Music"
        );
        assert_eq!(
            default_dest_name(
                &TransferDest::File {
                    path: std::path::PathBuf::from("backup.json"),
                    format: FileFormat::Json,
                },
                "Ignored"
            ),
            "backup.json"
        );
    }

    #[test]
    fn read_file_inputs_rejects_unsupported_extensions_before_reading() {
        let err = read_file_inputs(std::path::Path::new("playlist.TXT")).unwrap_err();

        assert!(
            err.to_string().contains("unsupported file type `txt`"),
            "{err}"
        );

        let err = read_file_inputs(std::path::Path::new("playlist")).unwrap_err();
        assert!(
            err.to_string().contains("unsupported file type ``"),
            "{err}"
        );
    }

    #[test]
    fn read_file_inputs_loads_json_playlist_envelope() {
        let path = temp_path("playlist", "json");
        let mut song = Song::from_search(
            "dQw4w9WgXcQ",
            "Catalog Song",
            "Catalog Artist",
            "3:32",
            Some("Catalog Album".to_owned()),
        );
        song.duration_secs = Some(212);
        let file = crate::transfer::json::PlaylistFile::new(
            "Roadtrip".to_owned(),
            "ytm:PL123".to_owned(),
            vec![song],
        );
        crate::transfer::json::write_playlist(&path, &file).expect("write playlist json");

        let (name, inputs) = read_file_inputs(&path).expect("read playlist json");
        let _ = std::fs::remove_file(&path);

        assert_eq!(name, "Roadtrip");
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].title, "Catalog Song");
        assert_eq!(inputs[0].artists, vec!["Catalog Artist"]);
        assert_eq!(inputs[0].album.as_deref(), Some("Catalog Album"));
        assert_eq!(inputs[0].duration_secs, Some(212));
        assert_eq!(inputs[0].known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn read_file_inputs_loads_csv_with_file_stem_name() {
        let path = temp_path("csv-source", "csv");
        std::fs::write(
            &path,
            "\
\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\",\"YouTube ID\"
spotify:track:1,\"CSV Song\",\"Artist A, Artist B\",\"CSV Album\",90000,ISRC1,dQw4w9WgXcQ
",
        )
        .expect("write csv");

        let expected_name = path.file_stem().unwrap().to_string_lossy().to_string();
        let (name, inputs) = read_file_inputs(&path).expect("read csv");
        let _ = std::fs::remove_file(&path);

        assert_eq!(name, expected_name);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].title, "CSV Song");
        assert_eq!(inputs[0].artists, vec!["Artist A", "Artist B"]);
        assert_eq!(inputs[0].album.as_deref(), Some("CSV Album"));
        assert_eq!(inputs[0].duration_secs, Some(90));
        assert_eq!(inputs[0].isrc.as_deref(), Some("ISRC1"));
        assert_eq!(inputs[0].source_key, "spotify:track:1");
        assert_eq!(inputs[0].known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn outcome_counts_ignore_unresolved_and_skipped_entries() {
        let tracks = vec![
            entry(input("Matched", &["A"]), Some(matched("vid-a"))),
            entry(
                input("Ambiguous", &["B"]),
                Some(MatchOutcome::Ambiguous {
                    candidates: vec![AmbiguousCandidate {
                        key: "vid-b".to_owned(),
                        score: 0.70,
                        display: "B - Ambiguous".to_owned(),
                        score_breakdown: None,
                    }],
                }),
            ),
            entry(input("Missing", &["C"]), Some(MatchOutcome::NotFound)),
            entry(input("Skipped", &["D"]), Some(MatchOutcome::SkippedLocal)),
            entry(input("Pending", &["E"]), None),
        ];

        assert_eq!(outcome_counts(&tracks), (1, 1, 1));
    }

    #[tokio::test]
    async fn fetch_source_reads_file_inputs_without_service_clients() {
        let path = temp_path("fetch-file", "csv");
        std::fs::write(
            &path,
            "\
\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\",\"YouTube ID\"
spotify:track:1,\"CSV Song\",\"Artist A\",\"CSV Album\",90000,ISRC1,dQw4w9WgXcQ
",
        )
        .expect("write csv");
        let spec = JobSpec {
            source: TransferSource::File { path: path.clone() },
            dest: TransferDest::YtmLikes,
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        };
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };

        let (name, entries, skipped_local) = fetch_source("job-fetch-file", &spec, &mut ctx)
            .await
            .unwrap_or_else(|err| panic!("fetch source failed: {}", err.error));
        let _ = std::fs::remove_file(&path);

        assert_eq!(name, path.file_stem().unwrap().to_string_lossy());
        assert_eq!(skipped_local, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input.title, "CSV Song");
        assert_eq!(
            entries[0].input.known_video_id.as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert!(entries[0].outcome.is_none());
        assert!(!entries[0].written);
    }

    #[tokio::test]
    async fn fetch_source_caps_large_file_inputs_before_checkpointing() {
        let path = temp_path("fetch-cap", "csv");
        let mut csv = String::from(
            "\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\",\"YouTube ID\"\n",
        );
        for i in 0..(TRACK_CAP + 2) {
            csv.push_str(&format!(
                "spotify:track:{i},\"Song {i}\",\"Artist\",\"Album\",60000,,\n"
            ));
        }
        std::fs::write(&path, csv).expect("write csv");
        let spec = JobSpec {
            source: TransferSource::File { path: path.clone() },
            dest: TransferDest::YtmLikes,
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        };
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };

        let (_name, entries, skipped_local) = fetch_source("job-fetch-cap", &spec, &mut ctx)
            .await
            .unwrap_or_else(|err| panic!("fetch source failed: {}", err.error));
        let _ = std::fs::remove_file(&path);

        assert_eq!(entries.len(), TRACK_CAP);
        assert_eq!(entries.last().unwrap().input.title, "Song 9999");
        assert_eq!(skipped_local, 0);
    }

    #[test]
    fn missing_clients_return_setup_errors() {
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };

        let Err(error) = ctx.ytm() else {
            panic!("missing ytm client should fail");
        };
        assert!(!error.resumable);
        assert!(error.error.to_string().contains("YouTube Music cookie"));

        let Err(error) = ctx.spotify() else {
            panic!("missing spotify client should fail");
        };
        assert!(!error.resumable);
        assert!(
            error
                .error
                .to_string()
                .contains("connected Spotify account")
        );
    }

    #[test]
    fn provider_errors_are_classified_for_resume_safety() {
        for error in [
            SpotifyError::RateLimited,
            SpotifyError::Network("timeout".to_owned()),
            SpotifyError::Auth("expired".to_owned()),
        ] {
            assert!(
                spotify_job_error(error).resumable,
                "transient/auth Spotify failures should checkpoint and resume"
            );
        }

        for error in [
            SpotifyError::NotAllowlisted,
            SpotifyError::Api {
                status: 400,
                message: "bad request".to_owned(),
            },
            SpotifyError::Decode("bad json".to_owned()),
        ] {
            assert!(
                !spotify_job_error(error).resumable,
                "permanent Spotify failures should be fatal"
            );
        }

        let ytm_error = ytm_job_error(anyhow!("expired cookie"));
        assert!(ytm_error.resumable);
        assert!(
            ytm_error
                .error
                .to_string()
                .contains("YouTube Music request failed")
        );
    }

    #[test]
    fn progress_beat_accepts_fetch_progress_without_surface_state() {
        let mut beat = progress_beat("job-fetch", Stage::Fetching);
        beat(1, 3, "Page 1".to_owned());
        beat(3, 3, "Done".to_owned());
    }

    #[test]
    fn report_counts_matched_and_preserves_ambiguous_and_not_found_rows() {
        let breakdown = MatchScoreBreakdown {
            total: 0.79,
            title: 0.90,
            artist: 0.80,
            duration: 0.50,
            album_bonus: 0.0,
        };
        let mut maybe = input("Maybe Song", &["Artist B"]);
        maybe.album_artists = vec!["Album Artist B".to_owned()];
        maybe.album_id = Some("spotify:album-id:b".to_owned());
        maybe.album_uri = Some("spotify:album:b".to_owned());
        maybe.album_release_date = Some("2026-07-01".to_owned());
        maybe.disc_number = Some(1);
        maybe.track_number = Some(2);
        maybe.isrc = Some("ISRC-B".to_owned());
        maybe.explicit = Some(false);
        maybe.source_url = Some("https://open.spotify.com/track/b".to_owned());
        let cp = Checkpoint::new(
            "job-report".to_owned(),
            spec(TransferDest::YtmLikes),
            vec![
                entry(input("Matched Song", &["Artist A"]), Some(matched("vid-a"))),
                entry(
                    maybe,
                    Some(MatchOutcome::Ambiguous {
                        candidates: vec![
                            AmbiguousCandidate {
                                key: "vid-b".to_owned(),
                                score: 0.79,
                                display: "Artist B - Candidate 1".to_owned(),
                                score_breakdown: Some(breakdown),
                            },
                            AmbiguousCandidate {
                                key: "vid-c".to_owned(),
                                score: 0.64,
                                display: "Artist B - Candidate 2".to_owned(),
                                score_breakdown: None,
                            },
                        ],
                    }),
                ),
                entry(
                    input("Missing Song", &["Artist C", "Feat D"]),
                    Some(MatchOutcome::NotFound),
                ),
                entry(
                    input("Skipped Local", &["Artist E"]),
                    Some(MatchOutcome::SkippedLocal),
                ),
            ],
        );

        let report = build_report(&cp, 1);

        assert_eq!(report.job_id, "job-report");
        assert_eq!(report.schema_version, 3);
        assert_eq!(report.total, 4);
        assert_eq!(report.matched, 1);
        assert_eq!(report.skipped_local, 1);
        assert_eq!(report.ambiguous.len(), 1);
        assert_eq!(report.ambiguous[0].title, "Maybe Song");
        assert_eq!(report.ambiguous[0].artists, "Artist B");
        assert_eq!(
            report.ambiguous[0].note,
            "Artist B - Candidate 1 (0.79) | Artist B - Candidate 2 (0.64)"
        );
        assert_eq!(report.ambiguous[0].source_order, Some(2));
        assert_eq!(
            report.ambiguous[0].source_key.as_deref(),
            Some("src:Maybe Song")
        );
        assert_eq!(
            report.ambiguous[0].source_url.as_deref(),
            Some("https://open.spotify.com/track/b")
        );
        assert_eq!(report.ambiguous[0].album.as_deref(), Some("Input Album"));
        assert_eq!(
            report.ambiguous[0].album_artists,
            vec!["Album Artist B".to_owned()]
        );
        assert_eq!(
            report.ambiguous[0].album_id.as_deref(),
            Some("spotify:album-id:b")
        );
        assert_eq!(
            report.ambiguous[0].album_uri.as_deref(),
            Some("spotify:album:b")
        );
        assert_eq!(
            report.ambiguous[0].album_release_date.as_deref(),
            Some("2026-07-01")
        );
        assert_eq!(report.ambiguous[0].disc_number, Some(1));
        assert_eq!(report.ambiguous[0].track_number, Some(2));
        assert_eq!(report.ambiguous[0].duration_secs, Some(62));
        assert_eq!(report.ambiguous[0].isrc.as_deref(), Some("ISRC-B"));
        assert_eq!(report.ambiguous[0].explicit, Some(false));
        assert_eq!(report.ambiguous[0].selected_key.as_deref(), Some("vid-b"));
        assert_eq!(report.ambiguous[0].selected_score, Some(0.79));
        assert_eq!(
            report.ambiguous[0].search_queries,
            vec![
                "Artist B Maybe Song".to_owned(),
                "Maybe Song Input Album".to_owned(),
                "Maybe Song".to_owned(),
            ]
        );
        assert_eq!(report.ambiguous[0].candidates.len(), 2);
        assert_eq!(report.ambiguous[0].candidates[0].key, "vid-b");
        assert_eq!(
            report.ambiguous[0].candidates[0].score_breakdown,
            Some(breakdown)
        );
        assert_eq!(report.not_found.len(), 1);
        assert_eq!(report.not_found[0].artists, "Artist C, Feat D");
        assert_eq!(report.not_found[0].note, "no match on the destination");
        assert_eq!(report.not_found[0].source_order, Some(3));
        assert_eq!(
            report.not_found[0].search_queries,
            vec![
                "Artist C Missing Song".to_owned(),
                "Artist C Feat D Missing Song".to_owned(),
                "Missing Song Input Album".to_owned(),
                "Missing Song".to_owned(),
            ]
        );
    }

    #[test]
    fn song_for_entry_prefers_source_catalog_metadata_for_local_downloads() {
        let mut source = input("Input Title", &["Input Artist"]);
        source.album_artists = vec!["Input Album Artist".to_owned()];
        source.disc_number = Some(1);
        source.track_number = Some(7);
        source.isrc = Some("ISRC123".to_owned());
        source.source_url = Some("https://open.spotify.com/track/input".to_owned());
        let track = entry(
            source,
            Some(MatchOutcome::Matched {
                key: "vid-1".to_owned(),
                score: 0.95,
                display: "Matched Artist - Matched Title".to_owned(),
                title: Some("Matched Title".to_owned()),
                artist: Some("Matched Artist".to_owned()),
                album: Some("Matched Album".to_owned()),
                duration_secs: Some(225),
                score_breakdown: None,
            }),
        );

        let song = song_for_entry(&track, "vid-1", "sp2yt-test", 7);

        assert_eq!(song.video_id, "vid-1");
        assert_eq!(song.title, "Input Title");
        assert_eq!(song.artist, "Input Artist");
        assert_eq!(song.album.as_deref(), Some("Input Album"));
        assert_eq!(song.duration, "1:02");
        assert_eq!(song.duration_secs, Some(62));
        assert_eq!(song.album_artist.as_deref(), Some("Input Album Artist"));
        assert_eq!(song.disc_number, Some(1));
        assert_eq!(song.track_number, Some(7));
        assert_eq!(song.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(song.origin_key.as_deref(), Some("src:Input Title"));
        assert_eq!(
            song.origin_url.as_deref(),
            Some("https://open.spotify.com/track/input")
        );
        assert_eq!(song.import_session_id.as_deref(), Some("sp2yt-test"));
        assert_eq!(song.import_source_order, Some(7));
    }

    #[test]
    fn song_for_entry_falls_back_to_source_input_when_candidate_metadata_is_missing() {
        let track = entry(input("Input Title", &["Artist A", "Artist B"]), None);

        let song = song_for_entry(&track, "vid-fallback", "sp2yt-test", 3);

        assert_eq!(song.video_id, "vid-fallback");
        assert_eq!(song.title, "Input Title");
        assert_eq!(song.artist, "Artist A, Artist B");
        assert_eq!(song.album.as_deref(), Some("Input Album"));
        assert_eq!(song.duration, "1:02");
        assert_eq!(song.duration_secs, Some(62));
        assert_eq!(song.import_session_id.as_deref(), Some("sp2yt-test"));
        assert_eq!(song.import_source_order, Some(3));
    }

    #[test]
    fn song_for_entry_falls_back_to_candidate_when_source_fields_are_missing() {
        let mut source = input("", &[]);
        source.album = None;
        source.duration_secs = None;
        let track = entry(
            source,
            Some(MatchOutcome::Matched {
                key: "vid-empty".to_owned(),
                score: 0.91,
                display: "Matched Title".to_owned(),
                title: Some("Matched Title".to_owned()),
                artist: None,
                album: None,
                duration_secs: None,
                score_breakdown: None,
            }),
        );

        let song = song_for_entry(&track, "vid-empty", "sp2yt-test", 11);

        assert_eq!(song.title, "Matched Title");
        assert_eq!(song.artist, "");
        assert_eq!(song.album, None);
        assert_eq!(song.duration, "");
        assert_eq!(song.duration_secs, None);
        assert_eq!(song.import_session_id.as_deref(), Some("sp2yt-test"));
        assert_eq!(song.import_source_order, Some(11));
    }

    #[test]
    fn progress_write_reports_current_track_and_outcome_counts() {
        let cp = Checkpoint::new(
            "job-progress".to_owned(),
            spec(TransferDest::YtmLikes),
            vec![
                entry(input("Matched Song", &["Artist A"]), Some(matched("vid-a"))),
                entry(
                    input("Maybe Song", &["Artist B"]),
                    Some(MatchOutcome::Ambiguous {
                        candidates: vec![AmbiguousCandidate {
                            key: "vid-b".to_owned(),
                            score: 0.70,
                            display: "Artist B - Candidate".to_owned(),
                            score_breakdown: None,
                        }],
                    }),
                ),
                entry(
                    input("Missing Song", &["Artist C"]),
                    Some(MatchOutcome::NotFound),
                ),
            ],
        );

        let progress = progress_write(&cp, 2, 3, 1);

        assert_eq!(progress.job_id, "job-progress");
        assert_eq!(progress.stage, Stage::Writing);
        assert_eq!(progress.done, 2);
        assert_eq!(progress.total, 3);
        assert_eq!(progress.matched, 1);
        assert_eq!(progress.ambiguous, 1);
        assert_eq!(progress.not_found, 1);
        assert_eq!(progress.current, "Artist B — Maybe Song");
    }

    #[test]
    fn progress_write_handles_missing_index_without_panicking() {
        let cp = Checkpoint::new(
            "job-progress-missing".to_owned(),
            spec(TransferDest::YtmLikes),
            vec![entry(
                input("Matched Song", &["Artist A"]),
                Some(matched("vid-a")),
            )],
        );

        let progress = progress_write(&cp, 1, 1, 99);

        assert_eq!(progress.job_id, "job-progress-missing");
        assert_eq!(progress.current, "");
        assert_eq!(progress.matched, 1);
    }

    #[tokio::test]
    async fn run_job_resume_rematch_resets_checkpointed_matches_without_persisting() {
        let original_spec = spec(TransferDest::YtmLikes);
        let mut cp = Checkpoint::new(
            "job_with_underscores_no_persist".to_owned(),
            original_spec.clone(),
            vec![
                TrackEntry {
                    input: TrackInput {
                        known_video_id: Some("cached-yt-id".to_owned()),
                        ..input("Known Id", &["Artist A"])
                    },
                    outcome: Some(matched("cached-yt-id")),
                    written: true,
                },
                entry(
                    input("Already Missing", &["Artist B"]),
                    Some(MatchOutcome::NotFound),
                ),
            ],
        );
        cp.stage = Stage::Writing;
        cp.skipped_local = 2;

        let mut resumed_spec = original_spec;
        resumed_spec.dry_run = true;
        resumed_spec.rematch = true;
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };
        let mut progress = Vec::new();

        let report = run_job(
            cp.job_id.clone(),
            resumed_spec,
            Some(cp),
            &mut ctx,
            &mut |p| progress.push(p),
        )
        .await
        .unwrap_or_else(|err| panic!("resume dry-run should not need clients: {}", err.error));

        assert_eq!(report.job_id, "job_with_underscores_no_persist");
        assert_eq!(report.total, 2);
        assert_eq!(report.matched, 0);
        assert_eq!(report.skipped_local, 2);
        assert!(report.ambiguous.is_empty());
        assert!(report.not_found.is_empty());
        assert!(
            progress.is_empty(),
            "writing-stage dry-run should not emit progress"
        );
    }

    #[tokio::test]
    async fn match_stage_requires_ytm_client_for_unresolved_tracks() {
        let mut cp = Checkpoint::new(
            "job_match_missing_client".to_owned(),
            spec(TransferDest::YtmLikes),
            vec![entry(input("Needs Match", &["Artist"]), None)],
        );
        cp.stage = Stage::Matching;
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };

        let err = match_stage(&mut cp, &mut ctx, &mut |_p| {})
            .await
            .expect_err("unresolved YTM match should require a YTM client");

        assert!(!err.resumable);
        assert!(err.error.to_string().contains("YouTube Music cookie"));
        assert!(cp.tracks[0].outcome.is_none());
    }

    #[tokio::test]
    async fn write_stage_dedupes_matches_before_missing_client_error() {
        let mut cp = Checkpoint::new(
            "job_write_missing_client".to_owned(),
            spec(TransferDest::YtmLikes),
            vec![
                entry(input("First", &["A"]), Some(matched("dup-id"))),
                entry(input("Duplicate", &["B"]), Some(matched("dup-id"))),
                entry(
                    input("Take Best", &["C"]),
                    Some(MatchOutcome::Ambiguous {
                        candidates: vec![AmbiguousCandidate {
                            key: "ambig-id".to_owned(),
                            score: 0.79,
                            display: "C - Candidate".to_owned(),
                            score_breakdown: None,
                        }],
                    }),
                ),
                entry(input("Missing", &["D"]), Some(MatchOutcome::NotFound)),
            ],
        );
        cp.stage = Stage::Writing;
        cp.spec.take_best = true;
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };
        let mut report = build_report(&cp, 0);
        let mut progress = Vec::new();

        let err = write_stage(&mut cp, &mut ctx, &mut |p| progress.push(p), &mut report)
            .await
            .expect_err("YTM likes write should require a YTM client");

        assert!(!err.resumable);
        assert!(err.error.to_string().contains("YouTube Music cookie"));
        assert_eq!(report.duplicates_dropped, 1);
        assert_eq!(report.written, 0);
        assert!(progress.is_empty());
        assert!(cp.tracks.iter().all(|track| !track.written));
    }

    #[tokio::test]
    async fn write_stage_creates_local_playlist_for_spotify_liked_source() {
        let playlist_name = "Spotify Liked Songs Local Write Test".to_owned();
        let mut cp = Checkpoint::new(
            "job-liked-local-playlist".to_owned(),
            spec(TransferDest::LocalPlaylist {
                name: Some(playlist_name.clone()),
            }),
            vec![entry(
                input("Liked Song", &["Artist A"]),
                Some(matched("vid-liked")),
            )],
        );
        cp.stage = Stage::Writing;
        cp.dest_name = Some(default_dest_name(&cp.spec.dest, "Spotify Liked Songs"));
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };
        let mut report = build_report(&cp, 0);
        let mut progress = Vec::new();

        write_stage(&mut cp, &mut ctx, &mut |p| progress.push(p), &mut report)
            .await
            .unwrap_or_else(|err| {
                panic!("local write should not need service clients: {}", err.error)
            });

        let store = crate::playlists::Playlists::load();
        let playlist = store
            .find(&playlist_name)
            .expect("liked songs local import should create a playlist");

        assert_eq!(playlist.songs.len(), 1);
        assert_eq!(playlist.songs[0].video_id, "vid-liked");
        assert_eq!(playlist.songs[0].title, "Liked Song");
        assert_eq!(cp.dest_id.as_deref(), Some(playlist.id.as_str()));
        assert_eq!(report.written, 1);
        assert_eq!(progress.len(), 1);
        assert!(cp.tracks[0].written);
    }

    #[tokio::test]
    async fn file_export_rejects_sources_that_cannot_be_exported_without_clients() {
        let mut ctx = JobCtx {
            ytm: None,
            spotify: None,
            search_config: SearchConfig::default(),
            market: None,
        };
        let spec = JobSpec {
            source: TransferSource::SpotifyLiked,
            dest: TransferDest::File {
                path: std::path::PathBuf::from("out.json"),
                format: FileFormat::Json,
            },
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        };

        let err = export_file(
            "job-export",
            &spec,
            &mut ctx,
            std::path::Path::new("out.json"),
            FileFormat::Json,
            Instant::now(),
        )
        .await
        .unwrap_err();

        assert!(!err.resumable);
        assert!(
            err.error
                .to_string()
                .contains("file export supports YouTube Music / local playlists")
        );
    }
}
