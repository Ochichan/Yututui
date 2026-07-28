//! The publish-one-track background task.

use super::*;

/// Copy one downloaded track into the music folder, then ask the server to rescan.
///
/// The scan runs only after the copy succeeded and its outcome never demotes the result: the bytes
/// are already in place, so a server that cannot or will not scan is a slower route to the same
/// track, not a failed publication.
pub(super) async fn run(
    emitter: super::task_set::RuntimeTaskEmitter,
    generation: u64,
    video_id: String,
) {
    let published = tokio::task::spawn_blocking(move || publish_downloaded_track(&video_id))
        .await
        .map_err(|_| crate::app::MusicServerFailure::Unavailable)
        .and_then(|result| result);
    let result = match published {
        Ok(report) => {
            let scan = match crate::open_subsonic::OpenSubsonicPaths::current() {
                Ok(paths) => crate::open_subsonic::request_library_scan(&paths).await,
                Err(_) => crate::open_subsonic::LibraryScanRequest::NoServer,
            };
            Ok(crate::app::TrackPublishOutcome { report, scan })
        }
        Err(failure) => Err(failure),
    };
    emitter
        .emit_terminal(RuntimeEvent::App(Msg::Server(
            crate::app::ServerEvent::Settings(crate::app::MusicServerEvent::TrackPublished {
                generation,
                result,
            }),
        )))
        .await;
}

/// Resolve one downloaded track and copy it into the configured music folder.
///
/// Blocking on purpose: this is filesystem work, and the ledger it updates is committed with the
/// same atomic-write discipline as the rest of the store.
fn publish_downloaded_track(
    video_id: &str,
) -> Result<crate::app::TrackPublishReport, crate::app::MusicServerFailure> {
    use crate::app::{MusicServerFailure, TrackPublishReport};
    use crate::open_subsonic::publish::{PublishReport, PublishRequest, publish_track};

    let paths = crate::open_subsonic::OpenSubsonicPaths::current()
        .map_err(|_| MusicServerFailure::Unavailable)?;
    let store = crate::downloads::DownloadStore::load();
    let song = store
        .tracks()
        .iter()
        .find(|song| song.video_id == video_id)
        .ok_or(MusicServerFailure::Unavailable)?;
    let source = song
        .local_path
        .clone()
        .ok_or(MusicServerFailure::Unavailable)?;
    let request = PublishRequest {
        video_id: song.video_id.clone(),
        title: song.title.clone(),
        artist: Some(song.artist.clone()),
        album_artist: song.album_artist.clone(),
        album: song.album.clone(),
        track_number: song.track_number,
        source,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    match publish_track(&paths, &request, now) {
        Ok(PublishReport::Published { .. }) => Ok(TrackPublishReport::Published),
        Ok(PublishReport::AlreadyPublished { .. }) => Ok(TrackPublishReport::AlreadyPublished),
        Ok(PublishReport::Conflict { .. }) => Ok(TrackPublishReport::Conflict),
        Err(_) => Err(MusicServerFailure::Unavailable),
    }
}
