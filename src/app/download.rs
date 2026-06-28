//! Download reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Mark a download as starting and emit the effect to run it.
    pub(in crate::app) fn start_download(&mut self, song: Song) -> Vec<Cmd> {
        if song.is_local() {
            self.status = format!("{}: {}", t!("Already local", "이미 로컬에 있음"), song.title);
            self.dirty = true;
            return Vec::new();
        }
        self.downloads
            .insert(song.video_id.clone(), DownloadState::Running(0));
        self.download_sources
            .insert(song.video_id.clone(), song.clone());
        self.status = format!("{}: {} — {}", t!("Downloading", "다운로드 중"), song.title, song.artist);
        self.dirty = true;
        vec![Cmd::Download(song)]
    }

    pub(in crate::app) fn add_downloaded_track(&mut self, song: Song) {
        self.downloaded_tracks
            .retain(|s| s.video_id != song.video_id);
        self.downloaded_tracks.insert(0, song);
        self.downloaded_tracks.truncate(DOWNLOADED_TRACKS_MAX);
    }
}
