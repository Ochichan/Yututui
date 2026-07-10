//! Source naming and bounded fetch logic.

use anyhow::{anyhow, bail};

use super::*;

#[cfg(test)]
pub(super) fn default_dest_name(dest: &TransferDest, source_name: &str) -> String {
    default_dest_name_for_media(dest, source_name, ImportMediaKind::Track)
}

pub(super) fn default_dest_name_for_media(
    dest: &TransferDest,
    source_name: &str,
    media_kind: ImportMediaKind,
) -> String {
    let default_source_name = || match media_kind {
        ImportMediaKind::Track => source_name.to_owned(),
        ImportMediaKind::MusicVideo => format!("{source_name} (Music Videos)"),
    };
    match dest {
        TransferDest::YtmNewPlaylist { name }
        | TransferDest::SpotifyNewPlaylist { name }
        | TransferDest::LocalPlaylist { name } => name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(default_source_name),
        TransferDest::YtmExistingPlaylist { name } => name.clone(),
        TransferDest::YtmLikes => "Liked Music".to_owned(),
        TransferDest::SpotifyMirrorPlaylist { id } => format!("spotify:{id}"),
        TransferDest::File { path, .. } => path.display().to_string(),
    }
}

pub(super) async fn fetch_source(
    job_id: &str,
    spec: &JobSpec,
    ctx: &mut JobCtx,
) -> Result<(String, Vec<TrackEntry>, u32, bool), JobError> {
    let mut beat = progress_beat(job_id, Stage::Fetching);
    let (name, inputs, skipped_local, provider_truncated): (String, Vec<TrackInput>, u32, bool) =
        match &spec.source {
            TransferSource::SpotifyPlaylist { id } => {
                let spotify = ctx.spotify()?;
                let meta = spotify.playlist_meta(id).await.map_err(spotify_job_error)?;
                let mut on_page = |done: u32, total: u32| beat(done, total, meta.name.clone());
                let (tracks, skipped) = spotify
                    .playlist_tracks_for_transfer(id, TRACK_CAP, &mut on_page)
                    .await
                    .map_err(spotify_job_error)?;
                (
                    meta.name,
                    tracks.iter().map(TrackInput::from_spotify).collect(),
                    skipped,
                    usize::try_from(meta.total).unwrap_or(usize::MAX) > TRACK_CAP,
                )
            }
            TransferSource::SpotifyLiked => {
                let spotify = ctx.spotify()?;
                let mut on_page =
                    |done: u32, total: u32| beat(done, total, "Liked Songs".to_owned());
                let mut tracks = spotify
                    .liked_tracks_for_transfer(TRACK_CAP, &mut on_page)
                    .await
                    .map_err(spotify_job_error)?;
                tracks.reverse();
                (
                    "Spotify Liked Songs".to_owned(),
                    tracks.iter().map(TrackInput::from_spotify).collect(),
                    0,
                    tracks.len() >= TRACK_CAP,
                )
            }
            TransferSource::YtmPlaylist { id } => {
                let ytm = ctx.ytm()?;
                let name = ytm
                    .library_playlists()
                    .await
                    .map_err(JobError::fatal)?
                    .into_iter()
                    .find(|(playlist_id, _, _)| playlist_id == id)
                    .map(|(_, title, _)| title)
                    .unwrap_or_else(|| format!("ytm:{id}"));
                let songs = ytm.playlist_tracks_full(id).await.map_err(ytm_job_error)?;
                (
                    name,
                    songs.iter().map(TrackInput::from_song).collect(),
                    0,
                    songs.len() > TRACK_CAP,
                )
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
                    playlist.songs.len() > TRACK_CAP,
                )
            }
            TransferSource::File { path } => {
                let (name, inputs) = read_file_inputs(path).map_err(JobError::fatal)?;
                let truncated = inputs.len() > TRACK_CAP;
                (name, inputs, 0, truncated)
            }
        };
    let mut inputs = inputs;
    let source_truncated = provider_truncated || inputs.len() > TRACK_CAP;
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
    Ok((name, entries, skipped_local, source_truncated))
}

pub(super) fn read_file_inputs(
    path: &std::path::Path,
) -> anyhow::Result<(String, Vec<TrackInput>)> {
    let ext = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "json" => {
            let file = super::super::json::read_playlist(path)?;
            let inputs = file.to_track_inputs();
            Ok((file.name, inputs))
        }
        "csv" => {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("Imported playlist")
                .to_owned();
            Ok((name, super::super::csv::read_tracks(path)?))
        }
        _ => bail!("unsupported file type `{ext}` — use .json (yututui) or .csv (Exportify)"),
    }
}
