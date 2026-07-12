//! Track downloads via `yt-dlp` + `ffmpeg`: profile-selected audio with embedded metadata
//! and cover art, saved directly under `<download dir>/<title> [<id>].<ext>`. The YouTube id is
//! embedded in the filename so a rescan (or a fresh launch) can recover the track's online identity
//! — see [`crate::api::Song::local_file`] / [`crate::api::Song::parse_embedded_id`].
//!
//! The actor receives [`DownloadCmd::Start`] and spawns one task per track, gated by a
//! [`Semaphore`] so only a configured number run at once (priority #1: bounded work).
//! Progress is parsed from yt-dlp's `--progress-template` lines and streamed back as
//! [`DownloadEvent::Progress`]; the final saved path comes from `--print after_move:filepath`.

use std::collections::{HashMap, VecDeque};
use std::future::pending;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as SyncMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::mpsc::{Sender, error::TrySendError};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::api::Song;
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::util::process_guard::ChildTreeGuard;
use crate::util::{backpressure, sanitize};

mod album_art;
mod import_inbox;
mod metadata;
mod ownership;
mod terminal_outbox;
#[cfg(test)]
use import_inbox::import_inbox_dirs;
use import_inbox::{
    ensure_retained_import_capacity, find_retryable_audio, prepare_import_inbox_dirs,
    reclaim_import_file, retained_import_inventory, validate_downloaded_path,
    validate_real_directory_chain,
};
use metadata::{FinalizedDownload, finalize_download_metadata};
use ownership::{
    AdmittedDownloadCmd, DownloadAdmissionId, DownloadOwner, DownloadRegistration,
    OutstandingDownloads,
};
use terminal_outbox::{TerminalOutbox, TerminalSupervisor};

const OUTPUT_TEMPLATE: &str = "%(title)s [%(id)s].%(ext)s";
const YTDLP_STDOUT_MAX: usize = 64 * 1024;
const DOWNLOAD_ADMISSION_BACKLOG: usize = 1024;
const DOWNLOAD_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const PROGRESS_JOIN_GRACE: Duration = Duration::from_secs(2);

pub(crate) fn delete_download_files(
    paths: Vec<PathBuf>,
    download_dir: &Path,
) -> (Vec<PathBuf>, Vec<(PathBuf, std::io::Error)>) {
    let mut deleted = Vec::new();
    let mut failures = Vec::new();
    for path in paths {
        match remove_download_file_if_safe(&path, download_dir) {
            Ok(()) => deleted.push(path),
            Err(error) => failures.push((path, error)),
        }
    }
    (deleted, failures)
}

fn remove_download_file_if_safe(path: &Path, download_dir: &Path) -> std::io::Result<()> {
    crate::persist::ensure_persistence_writes_allowed()?;
    let root = download_dir.canonicalize()?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to delete symlink",
        ));
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to delete non-regular file",
        ));
    }
    let target = path.canonicalize()?;
    if !target.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to delete outside download directory",
        ));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "download file changed before delete",
        ));
    }
    crate::util::safe_fs::remove_private_file_durable(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadAudioProfile {
    CompatibleM4a,
    PreferNativeM4a,
    KeepNative,
}

impl DownloadAudioProfile {
    fn from_env() -> Self {
        match std::env::var("YTM_DOWNLOAD_AUDIO_PROFILE")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("prefer-native-m4a" | "m4a-native") => Self::PreferNativeM4a,
            Some("keep-native" | "best-source" | "best-native") => Self::KeepNative,
            Some(other) if !other.is_empty() => {
                tracing::warn!(
                    profile = other,
                    "unknown YTM_DOWNLOAD_AUDIO_PROFILE; using compatible-m4a"
                );
                Self::CompatibleM4a
            }
            _ => Self::CompatibleM4a,
        }
    }

    fn ytdlp_args(self) -> &'static [&'static str] {
        match self {
            Self::CompatibleM4a => &[
                "-f",
                "bestaudio",
                "-x",
                "--audio-format",
                "m4a",
                "--audio-quality",
                "0",
            ],
            Self::PreferNativeM4a => &[
                "-f",
                "ba[ext=m4a]/ba",
                "-x",
                "--audio-format",
                "m4a",
                "--audio-quality",
                "0",
            ],
            Self::KeepNative => &["-f", "bestaudio", "-x", "--audio-format", "best"],
        }
    }
}

pub enum DownloadCmd {
    Start(Box<Song>),
    StartForImport(Box<ImportDownloadRequest>),
    SetDir(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ImportDownloadRequest {
    pub session_id: String,
    pub row_id: String,
    pub source_order: u32,
    pub(crate) claim: crate::transfer::artifact_identity::ImportDownloadClaim,
    pub song: Song,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImportDownloadContext {
    pub session_id: String,
    pub row_id: String,
    pub source_order: u32,
    pub video_id: String,
    pub(crate) claim: crate::transfer::artifact_identity::ImportDownloadClaim,
}

impl ImportDownloadContext {
    pub(crate) fn tracking_key(&self) -> String {
        import_download_tracking_key(&self.session_id, self.source_order)
    }
}

impl ImportDownloadRequest {
    fn context(&self) -> ImportDownloadContext {
        ImportDownloadContext {
            session_id: self.session_id.clone(),
            row_id: self.row_id.clone(),
            source_order: self.source_order,
            video_id: self.song.video_id.clone(),
            claim: self.claim.clone(),
        }
    }
}

pub(crate) fn import_download_tracking_key(session_id: &str, source_order: u32) -> String {
    format!("import:{session_id}:{source_order}")
}

pub(crate) fn download_tracking_key(song: &Song) -> String {
    match (&song.import_session_id, song.import_source_order) {
        (Some(session_id), Some(source_order)) => {
            import_download_tracking_key(session_id, source_order)
        }
        _ => song.video_id.clone(),
    }
}

pub fn import_request_for_song(song: &Song) -> anyhow::Result<Option<ImportDownloadRequest>> {
    let (Some(session_id), Some(source_order)) =
        (song.import_session_id.as_ref(), song.import_source_order)
    else {
        if song.import_session_id.is_some() || song.import_source_order.is_some() {
            bail!("import download metadata is incomplete");
        }
        return Ok(None);
    };
    let session_id = session_id.clone();
    let claim =
        crate::transfer::session::claim_import_download(&session_id, source_order, &song.video_id)?;
    let row_id = claim.row_id.clone();
    Ok(Some(ImportDownloadRequest {
        session_id,
        row_id,
        source_order,
        claim,
        song: song.clone(),
    }))
}

#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Progress {
        video_id: String,
        percent: f64,
    },
    ImportProgress {
        context: ImportDownloadContext,
        percent: f64,
    },
    Done {
        video_id: String,
        path: String,
    },
    ImportDone {
        context: ImportDownloadContext,
        path: String,
    },
    Error {
        video_id: String,
        error: String,
    },
    ImportError {
        context: ImportDownloadContext,
        error: String,
    },
}

type EventSink = Arc<dyn Fn(DownloadEvent) -> DeliveryResult + Send + Sync>;

pub struct DownloadHandle {
    tx: Sender<AdmittedDownloadCmd>,
    pending: Arc<SyncMutex<PendingDownloadCommands>>,
    outstanding: Arc<OutstandingDownloads>,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct PendingDownloadCommands {
    queue: VecDeque<AdmittedDownloadCmd>,
    drainer_owned: bool,
    drainer_running: bool,
    closed: bool,
}

enum DownloadAdmissionError {
    Full(AdmittedDownloadCmd),
    Closed(AdmittedDownloadCmd),
}

#[derive(Debug)]
pub struct DownloadStartError {
    pub video_id: String,
    pub tracking_key: String,
    pub import_context: Option<Box<ImportDownloadContext>>,
}

#[derive(Debug)]
pub enum DownloadSetDirError {
    Full(PathBuf),
    Closed(PathBuf),
}

impl DownloadSetDirError {
    pub fn dir(&self) -> &Path {
        match self {
            DownloadSetDirError::Full(dir) | DownloadSetDirError::Closed(dir) => dir,
        }
    }
}

impl std::fmt::Display for DownloadSetDirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadSetDirError::Full(_) => {
                write!(f, "download queue is full; directory was not updated")
            }
            DownloadSetDirError::Closed(_) => {
                write!(
                    f,
                    "download worker is not running; directory was not updated"
                )
            }
        }
    }
}

impl std::error::Error for DownloadSetDirError {}

impl DownloadHandle {
    pub fn start(&self, song: Song) -> std::result::Result<(), DownloadStartError> {
        let video_id = song.video_id.clone();
        if self
            .admit_tracked(
                DownloadOwner::Ordinary(video_id.clone()),
                DownloadCmd::Start(Box::new(song)),
            )
            .is_ok()
        {
            Ok(())
        } else {
            Err(DownloadStartError {
                tracking_key: video_id.clone(),
                video_id,
                import_context: None,
            })
        }
    }

    pub fn start_for_import(
        &self,
        request: ImportDownloadRequest,
    ) -> std::result::Result<(), DownloadStartError> {
        let video_id = request.song.video_id.clone();
        let import_context = request.context();
        let owner = DownloadOwner::Import(import_context.clone());
        if self
            .admit_tracked(owner, DownloadCmd::StartForImport(Box::new(request)))
            .is_ok()
        {
            Ok(())
        } else {
            Err(DownloadStartError {
                tracking_key: import_context.tracking_key(),
                video_id,
                import_context: Some(Box::new(import_context)),
            })
        }
    }

    pub fn set_dir(&self, dir: PathBuf) -> std::result::Result<(), DownloadSetDirError> {
        match self.admit(AdmittedDownloadCmd::untracked(DownloadCmd::SetDir(dir))) {
            Ok(()) => Ok(()),
            Err(DownloadAdmissionError::Full(cmd)) => match cmd.command {
                DownloadCmd::SetDir(dir) => Err(DownloadSetDirError::Full(dir)),
                _ => unreachable!(),
            },
            Err(DownloadAdmissionError::Closed(cmd)) => match cmd.command {
                DownloadCmd::SetDir(dir) => Err(DownloadSetDirError::Closed(dir)),
                _ => unreachable!(),
            },
        }
    }

    /// Stop accepting work, cancel active downloads, and wait until the actor has joined every
    /// task. The caller supplies the outer timeout so quit remains globally bounded.
    pub async fn shutdown(&self) -> bool {
        self.outstanding.begin_intentional_shutdown();
        {
            let mut pending = lock_pending_download_commands(&self.pending);
            pending.closed = true;
            pending.queue.clear();
        }
        let _ = self.cancel.send(true);
        if *self.done.borrow() {
            return true;
        }
        let mut done = self.done.clone();
        loop {
            match done.changed().await {
                Ok(()) if *done.borrow() => return true,
                Ok(()) => {}
                Err(_) => return *done.borrow(),
            }
        }
    }

    fn admit(&self, cmd: AdmittedDownloadCmd) -> std::result::Result<(), DownloadAdmissionError> {
        let mut pending = lock_pending_download_commands(&self.pending);
        self.admit_locked(cmd, &mut pending)
    }

    fn admit_tracked(
        &self,
        owner: DownloadOwner,
        command: DownloadCmd,
    ) -> std::result::Result<(), DownloadAdmissionError> {
        // Serialize registration with the supervisor's final snapshot. A rejected command must
        // never be mistaken for accepted work if the actor exits during admission.
        let mut pending = lock_pending_download_commands(&self.pending);
        let admission_id = match self.outstanding.register_for_admission(owner) {
            DownloadRegistration::Registered(admission_id) => admission_id,
            DownloadRegistration::DuplicateImport => return Ok(()),
            DownloadRegistration::Full => {
                return Err(DownloadAdmissionError::Full(AdmittedDownloadCmd {
                    admission_id: None,
                    command,
                }));
            }
        };
        let result = self.admit_locked(
            AdmittedDownloadCmd::tracked(admission_id, command),
            &mut pending,
        );
        if result.is_err() {
            self.outstanding.retire(admission_id);
        }
        result
    }

    fn admit_locked(
        &self,
        cmd: AdmittedDownloadCmd,
        pending: &mut PendingDownloadCommands,
    ) -> std::result::Result<(), DownloadAdmissionError> {
        if pending.closed || self.tx.is_closed() {
            pending.closed = true;
            return Err(DownloadAdmissionError::Closed(cmd));
        }

        if pending.drainer_running || !pending.queue.is_empty() {
            return stage_download_command(cmd, pending);
        }

        match self.tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(TrySendError::Closed(cmd)) => {
                pending.closed = true;
                Err(DownloadAdmissionError::Closed(cmd))
            }
            Err(TrySendError::Full(cmd)) => {
                stage_download_command(cmd, pending)?;
                pending.drainer_running = true;
                if spawn_download_command_drainer(self.tx.clone(), Arc::clone(&self.pending)) {
                    Ok(())
                } else {
                    pending.drainer_running = false;
                    let cmd = pending
                        .queue
                        .pop_back()
                        .expect("new download command was staged");
                    Err(DownloadAdmissionError::Full(cmd))
                }
            }
        }
    }
}

impl Drop for DownloadHandle {
    fn drop(&mut self) {
        self.outstanding.begin_intentional_shutdown();
        {
            let mut pending = lock_pending_download_commands(&self.pending);
            pending.closed = true;
            pending.queue.clear();
        }
        let _ = self.cancel.send(true);
    }
}

fn stage_download_command(
    cmd: AdmittedDownloadCmd,
    pending: &mut PendingDownloadCommands,
) -> std::result::Result<(), DownloadAdmissionError> {
    let AdmittedDownloadCmd {
        admission_id,
        command,
    } = cmd;
    match command {
        DownloadCmd::SetDir(new_dir) => {
            debug_assert!(admission_id.is_none());
            if let Some(AdmittedDownloadCmd {
                admission_id: None,
                command: DownloadCmd::SetDir(current),
            }) = pending.queue.back_mut()
            {
                *current = new_dir;
                return Ok(());
            }
            let cmd = AdmittedDownloadCmd {
                admission_id,
                command: DownloadCmd::SetDir(new_dir),
            };
            if download_backlog_len(pending) >= DOWNLOAD_ADMISSION_BACKLOG {
                return Err(DownloadAdmissionError::Full(cmd));
            }
            pending.queue.push_back(cmd);
        }
        command => {
            let cmd = AdmittedDownloadCmd {
                admission_id,
                command,
            };
            if download_backlog_len(pending) >= DOWNLOAD_ADMISSION_BACKLOG {
                return Err(DownloadAdmissionError::Full(cmd));
            }
            pending.queue.push_back(cmd);
        }
    }
    Ok(())
}

fn download_backlog_len(pending: &PendingDownloadCommands) -> usize {
    pending.queue.len() + usize::from(pending.drainer_owned)
}

fn lock_pending_download_commands(
    pending: &SyncMutex<PendingDownloadCommands>,
) -> std::sync::MutexGuard<'_, PendingDownloadCommands> {
    pending.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("download command backlog mutex poisoned; recovering");
        poisoned.into_inner()
    })
}

fn spawn_download_command_drainer(
    tx: Sender<AdmittedDownloadCmd>,
    pending: Arc<SyncMutex<PendingDownloadCommands>>,
) -> bool {
    std::thread::Builder::new()
        .name("ytt-download-admission".to_owned())
        .spawn(move || {
            loop {
                let cmd = {
                    let mut pending = lock_pending_download_commands(&pending);
                    match pending.queue.pop_front() {
                        Some(cmd) => {
                            pending.drainer_owned = true;
                            cmd
                        }
                        None => {
                            pending.drainer_running = false;
                            return;
                        }
                    }
                };
                let sent = tx.blocking_send(cmd).is_ok();
                let mut pending = lock_pending_download_commands(&pending);
                pending.drainer_owned = false;
                if !sent {
                    pending.queue.clear();
                    pending.drainer_running = false;
                    pending.closed = true;
                    return;
                }
            }
        })
        .is_ok()
}

/// Spawn the download actor. Results return as [`DownloadEvent`]s.
pub fn spawn<F>(
    emit: F,
    dir: PathBuf,
    cookies: Option<PathBuf>,
    max_concurrent: usize,
) -> DownloadHandle
where
    F: Fn(DownloadEvent) -> DeliveryResult + Send + Sync + 'static,
{
    match retained_import_inventory(&dir) {
        Ok(inventory) if inventory.files > 0 => tracing::warn!(
            files = inventory.files,
            bytes = inventory.bytes,
            over_limit = inventory.over_limit,
            "retained import artifact work is present"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "retained import artifact inventory is unavailable"),
    }
    #[cfg(not(test))]
    if !crate::persist::persistence_access().is_read_only() {
        match crate::transfer::artifact_move::reconcile_all_pending() {
            Ok(report) => {
                if report.recovered > 0 {
                    tracing::info!(
                        recovered = report.recovered,
                        "recovered pending import artifact moves"
                    );
                }
                for warning in report.warnings {
                    tracing::warn!(warning, "import artifact move remains pending");
                }
            }
            Err(error) => {
                tracing::error!(error = %error, "could not inventory pending import artifact moves");
            }
        }
    }
    spawn_with_program(
        emit,
        dir,
        cookies,
        max_concurrent,
        crate::tools::ytdlp_program(),
    )
}

fn spawn_with_program<F>(
    emit: F,
    dir: PathBuf,
    cookies: Option<PathBuf>,
    max_concurrent: usize,
    program: String,
) -> DownloadHandle
where
    F: Fn(DownloadEvent) -> DeliveryResult + Send + Sync + 'static,
{
    let (tx, rx) =
        backpressure::bounded_channel::<AdmittedDownloadCmd>(backpressure::DOWNLOAD_QUEUE);
    let sem = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let emit: EventSink = Arc::new(emit);
    let pending = Arc::new(SyncMutex::new(PendingDownloadCommands::default()));
    let outstanding = Arc::new(OutstandingDownloads::default());
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);
    let terminal_outbox = Arc::new(TerminalOutbox::default());
    let actor = tokio::spawn(run_download_actor(DownloadActor {
        rx,
        sem,
        emit: Arc::clone(&emit),
        dir,
        cookies,
        program,
        cancel: cancel_rx.clone(),
        outstanding: Arc::clone(&outstanding),
        terminal_outbox: Arc::clone(&terminal_outbox),
    }));
    tokio::spawn(terminal_outbox::supervise(TerminalSupervisor {
        actor,
        pending: Arc::clone(&pending),
        outstanding: Arc::clone(&outstanding),
        done_tx,
        outbox: terminal_outbox,
        emit,
        cancel: cancel_rx,
    }));
    DownloadHandle {
        tx,
        pending,
        outstanding,
        cancel: cancel_tx,
        done: done_rx,
    }
}

#[derive(Clone)]
struct DownloadCancellation(Option<watch::Receiver<bool>>);

impl DownloadCancellation {
    #[cfg(test)]
    fn never() -> Self {
        Self(None)
    }

    fn watched(rx: watch::Receiver<bool>) -> Self {
        Self(Some(rx))
    }

    fn is_cancelled(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|rx| *rx.borrow() || rx.has_changed().is_err())
    }

    async fn cancelled(&self) {
        let Some(mut rx) = self.0.clone() else {
            pending::<()>().await;
            return;
        };
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

struct DownloadActor {
    rx: tokio::sync::mpsc::Receiver<AdmittedDownloadCmd>,
    sem: Arc<Semaphore>,
    emit: EventSink,
    dir: PathBuf,
    cookies: Option<PathBuf>,
    program: String,
    cancel: watch::Receiver<bool>,
    outstanding: Arc<OutstandingDownloads>,
    terminal_outbox: Arc<TerminalOutbox>,
}

async fn run_download_actor(actor: DownloadActor) {
    let DownloadActor {
        mut rx,
        sem,
        emit,
        mut dir,
        cookies,
        program,
        mut cancel,
        outstanding,
        terminal_outbox,
    } = actor;
    let mut tasks = JoinSet::new();
    let mut task_downloads = HashMap::new();
    loop {
        tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break;
                }
            }
            joined = tasks.join_next_with_id(), if !tasks.is_empty() => {
                if let Some(joined) = joined {
                    match joined {
                        Ok((id, ())) => {
                            task_downloads.remove(&id);
                        }
                        Err(error) => {
                            let download: Option<(DownloadAdmissionId, DownloadOwner)> =
                                task_downloads.remove(&error.id());
                            tracing::warn!(%error, ?download, "download task stopped unexpectedly");
                            if let Some((admission_id, owner)) = download {
                                let event = owner.unexpected_error(
                                    "download task stopped unexpectedly".to_owned(),
                                );
                                if outstanding.mark_terminal_pending(admission_id, event) {
                                    terminal_outbox.notify();
                                }
                            }
                        }
                    }
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                let AdmittedDownloadCmd {
                    admission_id,
                    command,
                } = cmd;
                match command {
                    DownloadCmd::SetDir(new_dir) => {
                        debug_assert!(admission_id.is_none());
                        // Once per change (not per download): make the resolved/expanded
                        // destination visible so an env/config override is inspectable.
                        tracing::info!(dir = %new_dir.display(), "download directory set");
                        dir = new_dir;
                    }
                    DownloadCmd::Start(song) => {
                        let admission_id =
                            admission_id.expect("accepted download start must be tracked");
                        let video_id = song.video_id.clone();
                        let emit = Arc::clone(&emit);
                        let dir = dir.clone();
                        let cookies = cookies.clone();
                        let program = program.clone();
                        let sem = Arc::clone(&sem);
                        let task_outstanding = Arc::clone(&outstanding);
                        let task_terminal = Arc::clone(&terminal_outbox);
                        let task_cancel = DownloadCancellation::watched(cancel.clone());
                        let task = tasks.spawn(async move {
                            let permit = tokio::select! {
                                _ = task_cancel.cancelled() => return,
                                permit = sem.acquire_owned() => match permit {
                                    Ok(permit) => permit,
                                    Err(_) => return,
                                },
                            };
                            let result = run_download_job(
                                &program,
                                &song,
                                &dir,
                                cookies.as_deref(),
                                &emit,
                                &task_cancel,
                            )
                            .await;
                            drop(permit);
                            let event = match result {
                                Ok(event) => event,
                                Err(_) if task_cancel.is_cancelled() => return,
                                Err(error) => {
                                    let error = sanitize::sanitize_error_text(format!("{error:#}"));
                                    tracing::warn!(error = %error, video_id = %song.video_id, "download failed");
                                    DownloadEvent::Error {
                                        video_id: song.video_id.clone(),
                                        error,
                                    }
                                }
                            };
                            if task_outstanding.mark_terminal_pending(admission_id, event) {
                                task_terminal.notify();
                            }
                        });
                        task_downloads.insert(
                            task.id(),
                            (admission_id, DownloadOwner::Ordinary(video_id)),
                        );
                    }
                    DownloadCmd::StartForImport(request) => {
                        let admission_id = admission_id
                            .expect("accepted import download start must be tracked");
                        let owner = DownloadOwner::Import(request.context());
                        let emit = Arc::clone(&emit);
                        let dir = dir.clone();
                        let cookies = cookies.clone();
                        let program = program.clone();
                        let sem = Arc::clone(&sem);
                        let task_outstanding = Arc::clone(&outstanding);
                        let task_terminal = Arc::clone(&terminal_outbox);
                        let task_cancel = DownloadCancellation::watched(cancel.clone());
                        let task = tasks.spawn(async move {
                            let permit = tokio::select! {
                                _ = task_cancel.cancelled() => return,
                                permit = sem.acquire_owned() => match permit {
                                    Ok(permit) => permit,
                                    Err(_) => return,
                                },
                            };
                            let result = run_import_download_job(
                                &program,
                                &request,
                                &dir,
                                cookies.as_deref(),
                                &emit,
                                &task_cancel,
                            )
                            .await;
                            drop(permit);
                            let event = match result {
                                Ok(event) => event,
                                Err(_) if task_cancel.is_cancelled() => return,
                                Err(error) => {
                                    let error = sanitize::sanitize_error_text(format!("{error:#}"));
                                    tracing::warn!(
                                        error = %error,
                                        session_id = %request.session_id,
                                        row_id = %request.row_id,
                                        source_order = request.source_order,
                                        video_id = %request.song.video_id,
                                        "import download failed"
                                    );
                                    if let Err(record_error) =
                                        crate::transfer::session::record_import_download_error(
                                            &request.claim,
                                            &error,
                                        )
                                    {
                                        tracing::warn!(
                                            error = %record_error,
                                            session_id = %request.session_id,
                                            row_id = %request.row_id,
                                            source_order = request.source_order,
                                            "could not record stable import download failure"
                                        );
                                    }
                                    DownloadEvent::ImportError {
                                        context: request.context(),
                                        error,
                                    }
                                }
                            };
                            if task_outstanding.mark_terminal_pending(admission_id, event) {
                                task_terminal.notify();
                            }
                        });
                        task_downloads.insert(task.id(), (admission_id, owner));
                    }
                }
            }
        }
    }

    rx.close();
    let joined = async {
        while let Some(result) = tasks.join_next_with_id().await {
            match result {
                Ok((id, ())) => {
                    task_downloads.remove(&id);
                }
                Err(error) => {
                    task_downloads.remove(&error.id());
                    tracing::warn!(%error, "download task stopped unexpectedly during shutdown");
                }
            }
        }
    };
    if tokio::time::timeout(DOWNLOAD_SHUTDOWN_GRACE, joined)
        .await
        .is_err()
    {
        tracing::warn!("download shutdown grace expired; aborting remaining tasks");
        tasks.abort_all();
        while let Some(result) = tasks.join_next_with_id().await {
            match result {
                Ok((id, ())) => {
                    task_downloads.remove(&id);
                }
                Err(error) => {
                    task_downloads.remove(&error.id());
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
async fn run_download_with_program(
    program: &str,
    song: &Song,
    dir: &Path,
    cookies: Option<&Path>,
    emit: &EventSink,
) -> Result<()> {
    let event = run_download_job(
        program,
        song,
        dir,
        cookies,
        emit,
        &DownloadCancellation::never(),
    )
    .await?;
    if emit_terminal(emit, event, &DownloadCancellation::never()).await {
        Ok(())
    } else {
        bail!("download completion owner closed")
    }
}

async fn run_download_job(
    program: &str,
    song: &Song,
    dir: &Path,
    cookies: Option<&Path>,
    emit: &EventSink,
    cancel: &DownloadCancellation,
) -> Result<DownloadEvent> {
    let finalized = run_download_to_path(
        program,
        song,
        dir,
        emit,
        cancel,
        DownloadRunOptions {
            cookies,
            import_root: None,
            import_context: None,
            metadata_required: false,
        },
    )
    .await?;
    let path = finalized.artifact_path;
    let path = path.to_string_lossy().into_owned();
    tracing::info!(path = %path, video_id = %song.video_id, "download done");
    Ok(DownloadEvent::Done {
        video_id: song.video_id.clone(),
        path,
    })
}

#[cfg(all(test, unix))]
async fn run_import_download_with_program(
    program: &str,
    request: &ImportDownloadRequest,
    dir: &Path,
    cookies: Option<&Path>,
    emit: &EventSink,
) -> Result<()> {
    let event = run_import_download_job(
        program,
        request,
        dir,
        cookies,
        emit,
        &DownloadCancellation::never(),
    )
    .await?;
    if emit_terminal(emit, event, &DownloadCancellation::never()).await {
        Ok(())
    } else {
        bail!("import download completion owner closed")
    }
}

async fn run_import_download_job(
    program: &str,
    request: &ImportDownloadRequest,
    dir: &Path,
    cookies: Option<&Path>,
    emit: &EventSink,
    cancel: &DownloadCancellation,
) -> Result<DownloadEvent> {
    let import_context = request.context();
    let inbox_root = dir.to_path_buf();
    let inbox_session_id = request.session_id.clone();
    let dirs = crate::util::blocking::spawn_io(move || {
        prepare_import_inbox_dirs(&inbox_root, &inbox_session_id)
    })
    .await
    .context("join import inbox preparation")??;

    let recovery_claim = request.claim.clone();
    if let Some(complete) = crate::util::blocking::spawn_io(move || {
        crate::transfer::artifact_move::reconcile_claim(&recovery_claim)
    })
    .await
    .context("join import artifact recovery")??
    {
        return Ok(import_download_done_event(request, complete));
    }

    let retryable = find_retryable_audio(&dirs.incoming, &request.song.video_id)?;
    if retryable.is_none() {
        ensure_retained_import_capacity(dir)?;
    }
    let incoming = match retryable {
        Some(path) => {
            finalize_download_metadata(
                &request.song,
                &path,
                &dirs.incoming,
                Some(dir),
                Some(&import_context),
                true,
            )
            .await?
        }
        None => run_download_to_path(
            program,
            &request.song,
            &dirs.incoming,
            emit,
            cancel,
            DownloadRunOptions {
                cookies,
                import_root: Some(dir),
                import_context: Some(&import_context),
                metadata_required: true,
            },
        )
        .await
        .with_context(|| {
            format!(
                "download import row {} ({})",
                request.row_id, request.source_order
            )
        })?,
    };
    let FinalizedDownload {
        artifact_path,
        publish_name,
        retained_raw,
    } = incoming;
    let complete = dirs.complete.join(&publish_name);
    let move_request = crate::transfer::artifact_move::ArtifactMoveRequest::import_download(
        request.session_id.clone(),
        request.row_id.clone(),
        request.source_order,
        request.claim.clone(),
        artifact_path,
        complete,
        dirs.complete,
    );
    let complete = crate::util::blocking::spawn_io(move || {
        crate::transfer::artifact_move::commit(move_request)
    })
    .await
    .context("join import artifact commit")??;
    if let Some(retained) = retained_raw {
        let root = dir.to_path_buf();
        match crate::util::blocking::spawn_io(move || {
            reclaim_import_file(&root, &retained.path, retained.object_id)
        })
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(
                %error,
                "committed import retained its exact raw download for explicit recovery"
            ),
            Err(error) => tracing::warn!(
                %error,
                "raw import cleanup worker failed after artifact commit"
            ),
        }
    }
    Ok(import_download_done_event(request, complete))
}

fn import_download_done_event(request: &ImportDownloadRequest, complete: PathBuf) -> DownloadEvent {
    let path = complete.to_string_lossy().into_owned();
    tracing::info!(
        path = %path,
        session_id = %request.session_id,
        row_id = %request.row_id,
        source_order = request.source_order,
        video_id = %request.song.video_id,
        "import download done"
    );
    DownloadEvent::ImportDone {
        context: request.context(),
        path,
    }
}

async fn emit_terminal(
    emit: &EventSink,
    event: DownloadEvent,
    cancel: &DownloadCancellation,
) -> bool {
    loop {
        match emit(event.clone()) {
            Ok(DeliveryReceipt::Enqueued | DeliveryReceipt::Deferred) => return true,
            Ok(DeliveryReceipt::Coalesced { .. }) => {
                tracing::warn!("download terminal unexpectedly used coalesced delivery");
                return true;
            }
            Err(DeliveryError::Closed) => return false,
            Err(error) => {
                tracing::trace!(%error, "download terminal delivery saturated; retrying");
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return false,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
}

enum DownloadProcessOutcome {
    Finished(Result<(String, std::process::ExitStatus)>),
    TimedOut,
    Cancelled,
}

struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn stop_progress_reader(
    mut progress: tokio::task::JoinHandle<VecDeque<String>>,
) -> VecDeque<String> {
    match tokio::time::timeout(PROGRESS_JOIN_GRACE, &mut progress).await {
        Ok(Ok(lines)) => lines,
        Ok(Err(error)) => {
            tracing::warn!(%error, "download progress reader stopped unexpectedly");
            VecDeque::new()
        }
        Err(_) => {
            progress.abort();
            let _ = progress.await;
            VecDeque::new()
        }
    }
}

async fn join_progress_reader(
    mut progress: tokio::task::JoinHandle<VecDeque<String>>,
) -> Result<VecDeque<String>> {
    match tokio::time::timeout(PROGRESS_JOIN_GRACE, &mut progress).await {
        Ok(Ok(lines)) => Ok(lines),
        Ok(Err(error)) => Err(anyhow!("download progress reader failed: {error}")),
        Err(_) => {
            progress.abort();
            let _ = progress.await;
            bail!("yt-dlp stderr remained open after process exit")
        }
    }
}

struct DownloadRunOptions<'a> {
    cookies: Option<&'a Path>,
    import_root: Option<&'a Path>,
    import_context: Option<&'a ImportDownloadContext>,
    metadata_required: bool,
}

async fn run_download_to_path(
    program: &str,
    song: &Song,
    dir: &Path,
    emit: &EventSink,
    cancel: &DownloadCancellation,
    options: DownloadRunOptions<'_>,
) -> Result<FinalizedDownload> {
    let DownloadRunOptions {
        cookies,
        import_root,
        import_context,
        metadata_required,
    } = options;
    // Re-check at the actor boundary (not just in the UI reducer): a non-video YouTube ref
    // (channel/playlist) would otherwise have `playback_target()` fall back to a channel URL,
    // handing yt-dlp something that isn't a downloadable track.
    if let Some(reason) = song.unplayable_youtube_ref_reason() {
        bail!("not a downloadable track: {reason}");
    }
    let playback_target = song
        .playback_target_checked()
        .with_context(|| format!("invalid download target for {}", song.video_id))?;
    let playback_target =
        crate::api::validate_playable_url_destination(song.source, &playback_target)
            .await
            .with_context(|| format!("unsafe download target for {}", song.video_id))?;
    if let Some(root) = import_root {
        validate_real_directory_chain(root, dir)?;
    } else {
        std::fs::create_dir_all(dir).with_context(|| format!("create download dir {dir:?}"))?;
    }

    let mut cmd = crate::tools::ytdlp_command_for(program);
    cmd.arg(playback_target)
        .args(DownloadAudioProfile::from_env().ytdlp_args())
        .args([
            "--embed-metadata",
            "--embed-thumbnail",
            "--no-playlist",
            "--newline",
        ])
        .arg("-P")
        .arg(dir)
        .args(["-o", OUTPUT_TEMPLATE])
        .args(["--progress-template", "download:%(progress._percent_str)s"])
        .args(["--no-simulate", "--print", "after_move:filepath"]);
    crate::tools::append_ytdlp_cookie_args(&mut cmd, cookies);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(root) = import_root {
        validate_real_directory_chain(root, dir)?;
    }
    let mut child = cmd.spawn().context("spawn yt-dlp (is it installed?)")?;
    let mut process_group =
        ChildTreeGuard::for_tokio(&child, crate::util::process::ProcessProfile::YtDlp);
    // Piped just above, so these are effectively infallible — but a returned error beats a panic
    // that would unwind through the spawned task and leave the download silently dead.
    let stdout = child.stdout.take().context("yt-dlp stdout was not piped")?;
    let stderr = child.stderr.take().context("yt-dlp stderr was not piped")?;

    // Stream progress from stderr.
    let vid = song.video_id.clone();
    let import_context = import_context.cloned();
    let progress_import_context = import_context.clone();
    let emit_progress = emit.clone();
    let progress = tokio::spawn(async move {
        use crate::util::io::{BoundedLine, read_bounded_line};
        // A progress line is tiny; cap each so a newline-less stderr blob can't grow a String
        // without bound. On an over-cap line, keep draining in bounded chunks (never break —
        // that would leave yt-dlp blocked writing to a full stderr pipe and stall the
        // download); progress is best-effort, so a dropped fragment just isn't parsed.
        const STDERR_LINE_MAX: usize = 64 * 1024;
        const STDERR_DIAGNOSTIC_LINES_MAX: usize = 8;
        let mut reader = BufReader::new(stderr);
        let mut line: Vec<u8> = Vec::new();
        let mut last_percent: Option<u8> = None;
        let mut diagnostics: VecDeque<String> = VecDeque::new();
        loop {
            line.clear();
            match read_bounded_line(&mut reader, &mut line, STDERR_LINE_MAX).await {
                Ok(BoundedLine::Line) => {}
                Ok(BoundedLine::TooLarge) => continue, // drain the oversized line, then resume
                Ok(BoundedLine::Eof) | Err(_) => break,
            }
            if let Some(pct) = parse_percent(&String::from_utf8_lossy(&line)) {
                let rounded = pct.round().clamp(0.0, 100.0) as u8;
                if last_percent == Some(rounded) {
                    continue;
                }
                last_percent = Some(rounded);
                let event = match progress_import_context.as_ref() {
                    Some(context) => DownloadEvent::ImportProgress {
                        context: context.clone(),
                        percent: f64::from(rounded),
                    },
                    None => DownloadEvent::Progress {
                        video_id: vid.clone(),
                        percent: f64::from(rounded),
                    },
                };
                let _ = emit_progress(event);
            } else {
                let trimmed = String::from_utf8_lossy(&line).trim().to_owned();
                if !trimmed.is_empty() {
                    diagnostics.push_back(trimmed.chars().take(200).collect::<String>());
                    if diagnostics.len() > STDERR_DIAGNOSTIC_LINES_MAX {
                        diagnostics.pop_front();
                    }
                }
            }
        }
        diagnostics
    });
    // `JoinHandle` normally detaches on drop. Tie this reader to the owning download so the
    // actor's forced-abort fallback cannot leave a stderr task alive after terminal shutdown.
    let _progress_abort = AbortTaskOnDrop(progress.abort_handle());

    // The final path is printed to stdout (small); read it to EOF, then reap the child — all
    // under an overall backstop timeout so a wedged yt-dlp can't hold its concurrency
    // permit (the `Semaphore`) forever. A real audio download completes well within this; the
    // cap only catches a genuinely stuck process, which is killed and reported as an error.
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    let outcome = {
        let finish = async {
            let mut out = String::new();
            BufReader::new(stdout)
                .take((YTDLP_STDOUT_MAX + 1) as u64)
                .read_to_string(&mut out)
                .await?;
            if out.len() > YTDLP_STDOUT_MAX {
                bail!("yt-dlp output too large");
            }
            let status = child.wait().await?;
            anyhow::Ok((out, status))
        };
        tokio::pin!(finish);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => DownloadProcessOutcome::Cancelled,
            result = &mut finish => DownloadProcessOutcome::Finished(result),
            _ = tokio::time::sleep(DOWNLOAD_TIMEOUT) => DownloadProcessOutcome::TimedOut,
        }
    };
    let (out, status, stderr_lines) = match outcome {
        DownloadProcessOutcome::Finished(Ok((out, status))) => {
            let stderr_lines = match join_progress_reader(progress).await {
                Ok(lines) => lines,
                Err(error) => {
                    process_group.terminate();
                    return Err(error);
                }
            };
            process_group.terminate();
            (out, status, stderr_lines)
        }
        DownloadProcessOutcome::Finished(Err(error)) => {
            process_group.terminate();
            crate::util::process::kill_and_wait_tokio(
                &mut child,
                crate::util::process::ProcessProfile::YtDlp,
            )
            .await;
            let _ = stop_progress_reader(progress).await;
            return Err(error);
        }
        DownloadProcessOutcome::TimedOut => {
            process_group.terminate();
            crate::util::process::kill_and_wait_tokio(
                &mut child,
                crate::util::process::ProcessProfile::YtDlp,
            )
            .await;
            let _ = stop_progress_reader(progress).await;
            bail!("yt-dlp download timed out");
        }
        DownloadProcessOutcome::Cancelled => {
            process_group.terminate();
            crate::util::process::kill_and_wait_tokio(
                &mut child,
                crate::util::process::ProcessProfile::YtDlp,
            )
            .await;
            let _ = stop_progress_reader(progress).await;
            bail!("yt-dlp download cancelled");
        }
    };

    if !status.success() {
        // Newline-joined so `ytdlp_failure_detail` picks the LAST line (yt-dlp prints the
        // decisive `ERROR:` after its warnings) while classification scans all of them.
        let stderr = stderr_lines.into_iter().collect::<Vec<_>>().join("\n");
        bail!(
            "yt-dlp exited with {status}{}",
            crate::tools::ytdlp_failure_detail(stderr.as_bytes())
        );
    }
    // A zero exit + a printed line is NOT proof the file is really there: a yt-dlp
    // version/option change, a stray stdout line, or an external delete can all leave a bogus
    // path. Validate before reporting `Done` so a bad result surfaces as a normal download
    // error (existing toast) instead of a phantom "saved" track pointing at nothing.
    let path_str = out.lines().last().unwrap_or_default().trim();
    if path_str.is_empty() {
        bail!("yt-dlp reported no output filepath");
    }
    let path = PathBuf::from(path_str);
    let meta = validate_downloaded_path(&path, dir, import_root)?;
    if !meta.is_file() {
        bail!("downloaded path is not a regular file: {}", path.display());
    }
    if meta.len() == 0 {
        bail!("downloaded file is empty: {}", path.display());
    }
    // Best-effort id check: the filename embeds `[id]`. A mismatch is logged, not fatal —
    // yt-dlp's resolved id can legitimately differ for some refs.
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && let Some((_, embedded_id)) = crate::api::Song::parse_embedded_id(stem)
        && embedded_id != song.video_id
    {
        tracing::warn!(embedded_id, requested = %song.video_id, "downloaded file id differs from requested");
    }
    finalize_download_metadata(
        song,
        &path,
        dir,
        import_root,
        import_context.as_ref(),
        metadata_required,
    )
    .await
}

/// Parse a percentage from a `download:<pct>%` progress-template line.
fn parse_percent(line: &str) -> Option<f64> {
    let rest = line.strip_prefix("download:")?;
    rest.trim().trim_end_matches('%').parse::<f64>().ok()
}

#[cfg(test)]
#[path = "download/import_admission_tests.rs"]
mod import_admission_tests;
#[cfg(test)]
#[path = "download/tests.rs"]
mod tests;
