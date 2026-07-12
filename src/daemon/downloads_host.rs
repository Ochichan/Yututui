//! Daemon owner-lane host for the download actor and retained `downloads` projection.

use std::path::PathBuf;

use crate::api::Song;
use crate::download::{DownloadEvent, DownloadHandle};
use crate::remote::proto::{
    DownloadStateModel, DownloadStatusModel, RemoteCommand, RemoteResponse,
};
use crate::remote::publish::Publisher;
use crate::remote::server::RemoteEvent;

use super::engine::{DaemonEngine, RequesterKey};
use super::events::{DaemonEvent, DaemonEventSender, emit_daemon_event};

pub(super) struct DownloadsHost {
    handle: DownloadHandle,
    projection: DownloadsProjection,
    /// The spawn-time download root — the containment boundary for `delete_file`.
    dir: PathBuf,
}

impl DownloadsHost {
    pub(super) fn spawn(
        event_tx: DaemonEventSender,
        dir: PathBuf,
        cookies: Option<PathBuf>,
        max_concurrent: usize,
    ) -> Self {
        let handle = crate::download::spawn(
            move |event| emit_daemon_event(&event_tx, DaemonEvent::Download(event)),
            dir.clone(),
            cookies,
            max_concurrent,
        );
        Self {
            handle,
            projection: DownloadsProjection::default(),
            dir,
        }
    }

    pub(super) fn models(&self) -> Vec<DownloadStatusModel> {
        self.projection.models()
    }

    /// Intercept owner-only download commands and actor events before engine dispatch.
    /// `None` means the event was fully handled, including its correlated reply.
    pub(super) fn intercept(
        &mut self,
        event: DaemonEvent,
        engine: &DaemonEngine,
        publisher: &mut Publisher,
    ) -> Option<DaemonEvent> {
        match event {
            DaemonEvent::Download(event) => {
                self.on_event(event, publisher);
                None
            }
            DaemonEvent::Remote(RemoteEvent::Command(command, reply)) => {
                match self.command(engine, None, command) {
                    Ok((response, publish)) => {
                        let _ = reply.send(response);
                        if publish {
                            publisher.publish_downloads(self.projection.models());
                        }
                        None
                    }
                    Err(command) => Some(DaemonEvent::Remote(RemoteEvent::Command(command, reply))),
                }
            }
            DaemonEvent::Remote(RemoteEvent::SessionCommand {
                command,
                origin,
                reply,
            }) => {
                let requester =
                    RequesterKey::new(origin.session_id(), origin.page_id().map(str::to_owned));
                match self.command(engine, Some(&requester), command) {
                    Ok((response, publish)) => {
                        let _ = reply.send(response);
                        if publish {
                            publisher.publish_downloads(self.projection.models());
                        }
                        None
                    }
                    Err(command) => Some(DaemonEvent::Remote(RemoteEvent::SessionCommand {
                        command,
                        origin,
                        reply,
                    })),
                }
            }
            event => Some(event),
        }
    }

    fn command(
        &mut self,
        engine: &DaemonEngine,
        requester: Option<&RequesterKey>,
        command: RemoteCommand,
    ) -> Result<(RemoteResponse, bool), RemoteCommand> {
        match command {
            RemoteCommand::Download { video_id, title } => {
                let response = engine.resolve_gui_track(requester, &video_id).map_or_else(
                    || RemoteResponse::err("unknown_track"),
                    |song| self.start(song, title),
                );
                let publish = response.ok;
                Ok((response, publish))
            }
            RemoteCommand::DeleteDownload {
                video_id,
                delete_file,
            } => {
                // Publish only when the projection actually changed: a failed file
                // deletion keeps the entry (and the retry path) alive, so the wire
                // must not claim it is gone.
                let response = self.remove(&video_id, delete_file);
                let publish = response.ok;
                Ok((response, publish))
            }
            command => Err(command),
        }
    }

    fn start(&mut self, mut song: Song, title: String) -> RemoteResponse {
        if song.title.trim().is_empty() || song.title == song.video_id {
            song.title.clone_from(&title);
        }
        let video_id = song.video_id.clone();
        if self.handle.start(song).is_err() {
            // DownloadStartError intentionally collapses duplicate/full/closed admission;
            // expose one stable owner-lane reason rather than guessing which occurred.
            return RemoteResponse::err("busy");
        }
        self.projection.start(video_id, title);
        RemoteResponse::ok("download_started".to_owned())
    }

    fn on_event(&mut self, event: DownloadEvent, publisher: &mut Publisher) {
        let publish = match event {
            DownloadEvent::Progress { video_id, percent } => {
                self.projection.progress(&video_id, percent)
            }
            DownloadEvent::Done { video_id, path } => self.projection.done(&video_id, path),
            DownloadEvent::Error { video_id, error } => self.projection.failed(&video_id, error),
            DownloadEvent::ImportProgress { .. }
            | DownloadEvent::ImportDone { .. }
            | DownloadEvent::ImportError { .. } => {
                tracing::debug!("ignored transfer-owned download event in daemon downloads host");
                false
            }
        };
        if publish {
            publisher.publish_downloads(self.projection.models());
        }
    }

    fn remove(&mut self, video_id: &str, delete_file: bool) -> RemoteResponse {
        match self.projection.remove(video_id, delete_file, &self.dir) {
            Ok(()) => RemoteResponse::ok("download_removed".to_owned()),
            Err(reason) => RemoteResponse::err(reason),
        }
    }
}

/// Projection cap, matching the repo's 999 convention for user-facing stores.
const DOWNLOADS_PROJECTION_MAX: usize = 999;

#[derive(Debug, Default)]
struct DownloadsProjection {
    entries: Vec<DownloadEntry>,
}

#[derive(Debug)]
struct DownloadEntry {
    video_id: String,
    title: String,
    state: DownloadStateModel,
    pct: f64,
    error: Option<String>,
    path: Option<String>,
    last_published_pct: f64,
}

impl DownloadsProjection {
    fn start(&mut self, video_id: String, title: String) {
        if let Some(entry) = self.entry_mut(&video_id) {
            entry.title = title;
            entry.state = DownloadStateModel::Running;
            entry.pct = 0.0;
            entry.error = None;
            entry.path = None;
            entry.last_published_pct = 0.0;
            return;
        }
        // Bounded like every analogous store (library caps at 999): evict the oldest
        // terminal entry rather than growing for the daemon's whole lifetime.
        if self.entries.len() >= DOWNLOADS_PROJECTION_MAX
            && let Some(evict) = self
                .entries
                .iter()
                .position(|entry| entry.state != DownloadStateModel::Running)
        {
            self.entries.remove(evict);
        }
        self.entries.push(DownloadEntry {
            video_id,
            title,
            state: DownloadStateModel::Running,
            pct: 0.0,
            error: None,
            path: None,
            last_published_pct: 0.0,
        });
    }

    /// Record every progress value, but publish only after at least five percentage
    /// points beyond the last pushed value. Start/done/error always publish separately.
    fn progress(&mut self, video_id: &str, percent: f64) -> bool {
        let Some(entry) = self.entry_mut(video_id) else {
            return false;
        };
        if entry.state != DownloadStateModel::Running {
            return false;
        }
        entry.pct = percent.clamp(0.0, 100.0);
        if entry.pct - entry.last_published_pct < 5.0 {
            return false;
        }
        entry.last_published_pct = entry.pct;
        true
    }

    fn done(&mut self, video_id: &str, path: String) -> bool {
        let Some(entry) = self.entry_mut(video_id) else {
            return false;
        };
        entry.state = DownloadStateModel::Done;
        entry.pct = 100.0;
        entry.error = None;
        entry.path = Some(path);
        entry.last_published_pct = 100.0;
        true
    }

    fn failed(&mut self, video_id: &str, error: String) -> bool {
        let Some(entry) = self.entry_mut(video_id) else {
            return false;
        };
        entry.state = DownloadStateModel::Failed;
        entry.error = Some(error);
        true
    }

    fn remove(
        &mut self,
        video_id: &str,
        delete_file: bool,
        download_dir: &std::path::Path,
    ) -> Result<(), &'static str> {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.video_id == video_id)
        else {
            return Err("unknown_track");
        };
        // The actor has no cancel: dropping a Running entry would orphan the finishing
        // file (its Done event finds nothing) and dead-end the id behind the actor's
        // duplicate-admission guard. The GUI retries after completion/failure.
        if self.entries[index].state == DownloadStateModel::Running {
            return Err("download_running");
        }
        // Delete BEFORE forgetting the entry, through the same hardened deleter the App
        // owner uses (persistence-write gate, symlink/non-regular refusal, containment
        // under the download root) — a failure keeps the entry so the wire state stays
        // truthful and the delete stays retryable.
        if delete_file && let Some(path) = self.entries[index].path.as_deref() {
            match crate::download::remove_download_file_if_safe(
                std::path::Path::new(path),
                download_dir,
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("delete_failed"),
            }
        }
        self.entries.remove(index);
        Ok(())
    }

    fn entry_mut(&mut self, video_id: &str) -> Option<&mut DownloadEntry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.video_id == video_id)
    }

    fn models(&self) -> Vec<DownloadStatusModel> {
        self.entries
            .iter()
            .map(|entry| DownloadStatusModel {
                video_id: entry.video_id.clone(),
                title: entry.title.clone(),
                state: entry.state,
                pct: entry.pct,
                error: entry.error.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_progress_done_preserves_order_and_bounds_progress_pushes() {
        let mut projection = DownloadsProjection::default();
        projection.start("a".to_owned(), "First".to_owned());
        projection.start("b".to_owned(), "Second".to_owned());
        assert!(!projection.progress("a", 4.0));
        assert!(projection.progress("a", 5.0));
        assert!(!projection.progress("a", 9.0));
        assert!(projection.progress("a", 10.0));
        assert!(projection.done("a", "/tmp/first.m4a".to_owned()));

        let models = projection.models();
        assert_eq!(models[0].video_id, "a");
        assert_eq!(models[0].state, DownloadStateModel::Done);
        assert_eq!(models[0].pct, 100.0);
        assert_eq!(models[1].video_id, "b");
    }

    #[test]
    fn redownload_resets_existing_entry_in_place() {
        let mut projection = DownloadsProjection::default();
        projection.start("a".to_owned(), "Old".to_owned());
        projection.start("b".to_owned(), "Second".to_owned());
        projection.failed("a", "network".to_owned());
        projection.start("a".to_owned(), "New".to_owned());

        let models = projection.models();
        assert_eq!(models[0].video_id, "a");
        assert_eq!(models[0].title, "New");
        assert_eq!(models[0].state, DownloadStateModel::Running);
        assert_eq!(models[0].pct, 0.0);
        assert_eq!(models[0].error, None);
        assert_eq!(models[1].video_id, "b");
    }

    #[test]
    fn delete_removes_exact_recorded_file_and_unknown_is_stable() {
        let dir = std::env::temp_dir().join(format!(
            "ytt-download-host-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("track.m4a");
        std::fs::write(&path, b"audio").unwrap();

        let mut projection = DownloadsProjection::default();
        projection.start("a".to_owned(), "Track".to_owned());
        // Running downloads refuse deletion: the actor has no cancel, so dropping the
        // entry would orphan the finishing file and dead-end the id.
        assert_eq!(projection.remove("a", true, &dir), Err("download_running"));
        projection.done("a", path.to_string_lossy().into_owned());
        assert_eq!(projection.remove("a", true, &dir), Ok(()));
        assert!(!path.exists());
        assert_eq!(projection.remove("a", false, &dir), Err("unknown_track"));

        // A path outside the download root is refused by the hardened deleter and the
        // entry survives, keeping the wire state truthful and the delete retryable.
        let outside_dir = dir.join("outside-root");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("keep.m4a");
        std::fs::write(&outside, b"audio").unwrap();
        projection.start("b".to_owned(), "Other".to_owned());
        projection.done("b", outside.to_string_lossy().into_owned());
        let unrelated_root = dir.join("unrelated-root");
        std::fs::create_dir_all(&unrelated_root).unwrap();
        assert_eq!(
            projection.remove("b", true, &unrelated_root),
            Err("delete_failed")
        );
        assert!(outside.exists(), "containment refusal must not delete");
        assert!(
            projection.models().iter().any(|m| m.video_id == "b"),
            "failed deletion keeps the entry"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn error_sets_failed_state_and_message() {
        let mut projection = DownloadsProjection::default();
        projection.start("a".to_owned(), "Track".to_owned());
        assert!(projection.progress("a", 25.0));
        assert!(projection.failed("a", "network".to_owned()));

        let model = &projection.models()[0];
        assert_eq!(model.state, DownloadStateModel::Failed);
        assert_eq!(model.pct, 25.0);
        assert_eq!(model.error.as_deref(), Some("network"));
    }
}
