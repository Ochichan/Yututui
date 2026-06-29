//! Download reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Mark a download as starting and emit the effect to run it.
    pub(in crate::app) fn start_download(&mut self, song: Song) -> Vec<Cmd> {
        if song.is_local() {
            self.status.text = format!("{}: {}", t!("Already local", "이미 로컬에 있음"), song.title);
            self.dirty = true;
            return Vec::new();
        }
        self.downloads.active
            .insert(song.video_id.clone(), DownloadState::Running(0));
        self.downloads.sources
            .insert(song.video_id.clone(), song.clone());
        self.status.text = format!("{}: {} — {}", t!("Downloading", "다운로드 중"), song.title, song.artist);
        self.dirty = true;
        vec![Cmd::Download(song)]
    }

    pub(in crate::app) fn add_downloaded_track(&mut self, song: Song) {
        // Remember the enriched record (id + real metadata) BEFORE the move below — `insert`
        // takes `song` by value, so recording after it would not compile.
        self.download_store.record(&song);
        self.library_ui.downloaded
            .retain(|s| s.video_id != song.video_id);
        self.library_ui.downloaded.insert(0, song);
        self.library_ui.downloaded.truncate(DOWNLOADED_TRACKS_MAX);
    }

    /// Turn a bare disk scan into the Downloads-tab list, restoring each track's YouTube
    /// identity where possible: first from the persisted manifest (by `video_id` — brings back
    /// the real artist/duration too), then, for anything still id-less (e.g. files downloaded
    /// before the id-in-filename scheme), a best-effort normalized-title match against remote
    /// favorites/history entries. Files with no recoverable origin stay local-only (correct).
    pub fn enrich_downloads(&self, scanned: Vec<Song>) -> Vec<Song> {
        self.download_store
            .enrich(scanned)
            .into_iter()
            .map(|song| match self.legacy_youtube_id_for(&song) {
                Some(id) => song.with_yt_id(id),
                None => song,
            })
            .collect()
    }

    /// Best-effort recovery of a YouTube id for an id-less local `song` by matching its
    /// normalized title against a *remote* favorite/history entry (one that actually has a
    /// YouTube origin). Returns `None` when the song already has an id or nothing matches.
    fn legacy_youtube_id_for(&self, song: &Song) -> Option<String> {
        if song.youtube_id().is_some() {
            return None;
        }
        let key = song.title.trim().to_lowercase();
        self.library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .find(|e| e.youtube_id().is_some() && e.title.trim().to_lowercase() == key)
            .and_then(|e| e.youtube_id().map(str::to_owned))
    }
}
