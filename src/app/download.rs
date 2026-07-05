//! Download reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;
use std::collections::HashSet;

/// Ceiling on in-flight (dispatched-but-unfinished) downloads. Held comfortably below the
/// bounded command channel (`backpressure::DOWNLOAD_QUEUE` = 128) so a bulk batch drains
/// through `Downloads::pending` instead of overflowing the actor's `try_send`.
const BULK_INFLIGHT_CAP: usize = 96;

impl App {
    /// Mark a single download as starting and emit the effect to run it. Routes through the
    /// same pending/pump path as bulk downloads; a lone song dispatches immediately since the
    /// in-flight cap dwarfs 1, so the visible behavior is unchanged.
    pub(in crate::app) fn start_download(&mut self, song: Song) -> Vec<Cmd> {
        if song.is_local() {
            self.status.text = format!(
                "{}: {}",
                t!("Already local", "이미 로컬에 있음"),
                song.title
            );
            self.dirty = true;
            return Vec::new();
        }
        self.status.text = format!(
            "{}: {} — {}",
            t!("Downloading", "다운로드 중"),
            song.title,
            song.artist
        );
        self.downloads.pending.push_back(song);
        self.dirty = true;
        self.pump_downloads()
    }

    /// Filter a batch down to the songs actually worth fetching: drop local files, drop tracks
    /// already downloaded in a past session (matched by YouTube id against the manifest and the
    /// current on-disk scan), and collapse duplicate ids within the batch. Preserves order.
    pub(in crate::app) fn downloadable_batch(&self, songs: Vec<Song>) -> Vec<Song> {
        let mut seen: HashSet<String> = HashSet::new();
        songs
            .into_iter()
            .filter(|song| {
                // Local files are already on disk; radio stations are live streams, not
                // downloadable tracks — neither belongs in a bulk fetch.
                if song.is_local() || song.is_radio_station() {
                    return false;
                }
                if let Some(yt) = song.youtube_id()
                    && (self.download_store.contains_youtube_id(yt)
                        || self
                            .library_ui
                            .downloaded
                            .iter()
                            .any(|d| d.youtube_id() == Some(yt)))
                {
                    return false; // already downloaded before
                }
                let key = song
                    .youtube_id()
                    .unwrap_or(song.video_id.as_str())
                    .to_string();
                seen.insert(key) // false when this id already appeared in the batch
            })
            .collect()
    }

    /// Compute the real downloadable batch for `songs` and, if any remain, raise the bulk
    /// confirm popup ("Download N songs?"). An empty batch (everything local/already fetched)
    /// just surfaces a toast — no modal.
    pub(in crate::app) fn open_confirm_download(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        let batch = self.downloadable_batch(songs);
        self.dirty = true;
        if batch.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text = t!("Nothing to download", "다운로드할 곡이 없음").to_string();
            return Vec::new();
        }
        self.library_ui.confirm_download = Some(batch);
        Vec::new()
    }

    /// Confirm the bulk-download popup: queue the stored (already-deduped) batch and start
    /// draining it into the actor.
    pub(in crate::app) fn confirm_download_apply(&mut self) -> Vec<Cmd> {
        let Some(batch) = self.library_ui.confirm_download.take() else {
            return Vec::new();
        };
        let n = batch.len();
        self.status.kind = StatusKind::Info;
        self.status.text = format!("{}: {n}", t!("Queued for download", "다운로드 대기"));
        self.downloads.pending.extend(batch);
        self.dirty = true;
        self.pump_downloads()
    }

    /// Hand pending downloads to the actor up to [`BULK_INFLIGHT_CAP`], marking each `Running`
    /// so its indicator shows at once. Returns one `Cmd::Download` per dispatched song. Called
    /// on every enqueue and again as each download finishes, so the queue keeps flowing without
    /// ever exceeding the channel bound.
    pub(in crate::app) fn pump_downloads(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        while self.downloads.dispatched < BULK_INFLIGHT_CAP {
            let Some(song) = self.downloads.pending.pop_front() else {
                break;
            };
            self.downloads
                .active
                .insert(song.video_id.clone(), DownloadState::Running(0));
            self.downloads
                .sources
                .insert(song.video_id.clone(), song.clone());
            self.downloads.dispatched += 1;
            cmds.push(Cmd::Download(song));
        }
        cmds
    }

    pub(in crate::app) fn add_downloaded_track(&mut self, song: Song) {
        // Remember the enriched record (id + real metadata) BEFORE the move below — `insert`
        // takes `song` by value, so recording after it would not compile.
        self.download_store.record(&song);
        self.library_ui.downloaded_rev = self.library_ui.downloaded_rev.wrapping_add(1);
        self.library_ui
            .downloaded
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
