use std::collections::HashSet;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;
use crate::util::sanitize::sanitize_error_text;

use super::{RecorderEvent, RecorderJob};
use crate::recorder::barrier::CommandBarrier;
use crate::recorder::ext_is_taggable;
use crate::recorder::ownership;

mod journal;
mod pinned_io;

use journal::*;
use pinned_io::*;

const WORK_DIR_NAME: &str = ".ytt-recorder-work";
const JOURNAL_DIR_NAME: &str = "pending";
const SAVE_LOCK_NAME: &str = "save.lock";
const JOURNAL_SCHEMA: u8 = 1;
const MAX_JOURNAL_BYTES: usize = 64 * 1024;
const MAX_JOURNAL_RECORDS: usize = 8;
const MAX_SNAPSHOT_GROWTH_BYTES: usize = 1024;
const MAX_NAME_ATTEMPTS: u32 = 100_000;
const LOCK_WAIT: Duration = Duration::from_secs(5);
pub(crate) const MAX_PENDING_SAVES: usize = 128;
pub(crate) const MAX_PENDING_SOURCE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SaveIntent {
    schema: u8,
    token: String,
    id: u64,
    source: PathBuf,
    final_dir: PathBuf,
    filename: String,
    ext: String,
    title: Option<String>,
    artist: Option<String>,
    station: Option<String>,
    /// Kernel identities turn every mutable pathname below the trusted roots into a fail-closed
    /// lookup. Missing fields identify a legacy journal which is readable for inventory, but may
    /// not copy, publish, or remove bytes because its original generations cannot be proven.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_parent_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    final_parent_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work_parent_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    save_lock_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_parent_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    journal_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_identity: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destination_identity: Option<safe_fs::FileObjectId>,
    /// The destination was durably observed complete. Retained source/stage/journal generations
    /// remain counted by the spool, but startup recovery must not report the same save again.
    #[serde(default)]
    settled: bool,
    /// At most one externally-raced destination can advance the durable name. Further retries
    /// reuse that exact fallback and defer, so a hostile collision cannot consume the journal.
    #[serde(default)]
    collision_reselected: bool,
    destination: Option<PathBuf>,
    /// Present only after the complete staged bytes have been synced and before installation.
    /// `default` keeps schema-1 journals written by older versions readable. A legacy journal
    /// never treats an existing destination as committed because it has no identity proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit_identity: Option<CommitIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CommitIdentity {
    len: u64,
    sha256: String,
}

/// Durable token returned only after the intent file and its parent directory are synced.
pub struct AcceptedSave {
    journal_path: PathBuf,
    intent: SaveIntent,
    temp_root: PathBuf,
    close_barrier: Option<CommandBarrier>,
}

impl AcceptedSave {
    pub(crate) fn id(&self) -> u64 {
        self.intent.id
    }
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub recovered: usize,
    pub pending: usize,
    pub discarded_temps: usize,
    pub warnings: Vec<String>,
    pub pending_bytes: u64,
    /// Registry/lease/inventory uncertainty means no bounded-spool admission decision is safe.
    pub admission_uncertain: bool,
}

impl RecoveryReport {
    pub fn capacity_blocked(&self) -> bool {
        self.admission_uncertain
            || self.pending >= MAX_PENDING_SAVES
            || self.pending_bytes >= MAX_PENDING_SOURCE_BYTES
    }
}

#[derive(Debug)]
pub(super) enum InstallOutcome {
    Durable,
    CommittedButSyncFailed(io::Error),
}

struct PendingStage {
    #[cfg(test)]
    path: PathBuf,
    generation: safe_fs::OwnedGeneration,
}

enum AcceptanceFailure {
    RetrySafe(io::Error),
    Ambiguous(io::Error),
    Capacity { count: usize, bytes: u64 },
}

impl From<io::Error> for AcceptanceFailure {
    fn from(error: io::Error) -> Self {
        Self::RetrySafe(error)
    }
}

impl PendingStage {
    fn new(path: PathBuf, generation: safe_fs::OwnedGeneration) -> Self {
        let _ = &path;
        Self {
            #[cfg(test)]
            path,
            generation,
        }
    }
}

pub(super) fn accept(job: RecorderJob) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer_and_limits(
        job,
        create_journal,
        safe_fs::sync_parent_dir,
        safe_fs::ensure_dir_durable,
        |file| file.sync_all(),
        MAX_PENDING_SAVES,
        MAX_PENDING_SOURCE_BYTES,
    )
}

#[cfg(test)]
fn accept_with_writer<W>(job: RecorderJob, write: W) -> Result<AcceptedSave, RecorderEvent>
where
    W: FnOnce(&Path, &mut SaveIntent) -> io::Result<()>,
{
    accept_with_writer_and_limits(
        job,
        write,
        safe_fs::sync_parent_dir,
        safe_fs::ensure_dir_durable,
        |file| file.sync_all(),
        MAX_PENDING_SAVES,
        MAX_PENDING_SOURCE_BYTES,
    )
}

fn accept_with_writer_and_limits<W, S, D, F>(
    job: RecorderJob,
    write: W,
    resync_parent: S,
    ensure_final_dir: D,
    sync_source: F,
    max_count: usize,
    max_bytes: u64,
) -> Result<AcceptedSave, RecorderEvent>
where
    W: FnOnce(&Path, &mut SaveIntent) -> io::Result<()>,
    S: FnOnce(&Path) -> io::Result<()>,
    D: FnOnce(&Path) -> io::Result<()>,
    F: FnOnce(&std::fs::File) -> io::Result<()>,
{
    let RecorderJob::Save {
        id,
        temp,
        temp_dir,
        final_dir,
        filename,
        ext,
        title,
        artist,
        station,
        close_barrier,
        automatic,
        bypass_limits,
    } = job
    else {
        unreachable!("accept_save only accepts RecorderJob::Save")
    };

    let result = (|| -> Result<AcceptedSave, AcceptanceFailure> {
        // Reject an obviously impossible Save before creating a durable intent. Once accepted,
        // every later error is deferred and the journal remains the single retry owner.
        let source_parent_path = temp.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording source has no parent directory",
            )
        })?;
        safe_fs::ensure_private_dir(source_parent_path)?;
        let source_parent = pin_private_absolute_dir(source_parent_path, None)?;
        let source_name = temp.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording source has no basename",
            )
        })?;
        let source = source_parent.open_child_readonly(source_name)?;
        let source_metadata = source.file()?.metadata()?;
        if source_metadata.len() == 0 && close_barrier.is_none() {
            return Err(
                io::Error::new(io::ErrorKind::UnexpectedEof, "recording source is empty").into(),
            );
        }
        if temp.parent() != Some(temp_dir.as_path())
            && ownership::namespace_for_source(&temp_dir, &temp).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording source is outside its stable temp directory",
            )
            .into());
        }
        // A durable journal is useful only if its mpv-created source inode, bytes, and owner
        // directory entry survive the same cutoff. This remains pre-acceptance: a sync failure
        // returns truthful SaveFailed and never publishes recovery ownership.
        sync_source(source.file()?)?;
        let _publication = ownership::acquire_publication_lock(&temp_dir)?;
        if automatic && !bypass_limits {
            let usage = spool_usage(&temp_dir, &final_dir)?;
            let source_bytes = source_metadata.len();
            let projected_bytes = usage.bytes.checked_add(source_bytes).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::StorageFull,
                    "recording spool byte count overflow",
                )
            })?;
            if usage.uncertain
                || usage.count >= max_count
                || usage.bytes >= max_bytes
                || projected_bytes > max_bytes
            {
                return Err(AcceptanceFailure::Capacity {
                    count: usage.count.saturating_add(1),
                    bytes: projected_bytes,
                });
            }
        }
        ensure_final_dir(&final_dir)?;
        safe_fs::ensure_private_dir_durable(&work_dir(&final_dir))?;
        let journal_dir = registry_journal_dir(&temp_dir);
        safe_fs::ensure_private_dir_durable(&journal_dir)?;
        let final_parent = pin_absolute_dir(&final_dir, None)?;
        let work_parent = pin_private_absolute_dir(&work_dir(&final_dir), None)?;
        let journal_parent = pin_private_absolute_dir(&journal_dir, None)?;
        let (_, save_lock_identity) = work_parent
            .try_lock_child(std::ffi::OsStr::new(SAVE_LOCK_NAME), None, true)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "recording save lock is owned by another process during acceptance",
                )
            })?;
        let token = random_token()?;
        let journal_path = journal_dir.join(format!("{token}.json"));
        let mut intent = SaveIntent {
            schema: JOURNAL_SCHEMA,
            token,
            id,
            source: temp,
            final_dir,
            filename,
            ext: ext.to_owned(),
            title,
            artist,
            station,
            source_parent_identity: Some(source_parent.identity()),
            source_identity: Some(source.identity()),
            final_parent_identity: Some(final_parent.identity()),
            work_parent_identity: Some(work_parent.identity()),
            save_lock_identity: Some(save_lock_identity),
            journal_parent_identity: Some(journal_parent.identity()),
            journal_identity: None,
            stage_identity: None,
            destination_identity: None,
            settled: false,
            collision_reselected: false,
            destination: None,
            commit_identity: None,
        };
        validate_intent(&intent)?;
        validate_journal_budget(&intent)?;
        if let Err(error) = write(&journal_path, &mut intent) {
            match load_journal(&journal_path) {
                Ok(visible) if same_request(&visible, &intent) => {
                    // Atomic rename already published the exact intent; only the directory sync
                    // failed. It is no longer retry-safe. Retry the durability boundary, then
                    // admit the worker even if the filesystem still cannot confirm it.
                    if let Err(sync_error) = resync_parent(&journal_path) {
                        tracing::warn!(%error, %sync_error, journal = %journal_path.display(), "accepted recording journal is visible but directory sync remains uncertain");
                        return Err(AcceptanceFailure::Ambiguous(io::Error::new(
                            sync_error.kind(),
                            format!(
                                "recording intent is visible but its durability could not be confirmed: {sync_error}"
                            ),
                        )));
                    }
                }
                Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => {
                    return Err(AcceptanceFailure::RetrySafe(error));
                }
                Err(read_error) if read_error.kind() == io::ErrorKind::InvalidData => {
                    match remove_uninitialized_journal(&journal_path, &intent) {
                        Ok(()) => return Err(AcceptanceFailure::RetrySafe(error)),
                        Err(cleanup_error) => {
                            return Err(AcceptanceFailure::Ambiguous(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                format!(
                                    "recording intent write failed ({error}) and its invalid exact journal could not be retired: {cleanup_error}"
                                ),
                            )));
                        }
                    }
                }
                Ok(_) | Err(_) => {
                    return Err(AcceptanceFailure::Ambiguous(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "recording intent publication was ambiguous after write failure: {error}"
                        ),
                    )));
                }
            }
        }
        Ok(AcceptedSave {
            journal_path,
            intent,
            temp_root: temp_dir,
            close_barrier,
        })
    })();

    result.map_err(|failure| {
        let (ambiguous, error) = match failure {
            AcceptanceFailure::RetrySafe(error) => (false, error),
            AcceptanceFailure::Ambiguous(error) => (true, error),
            AcceptanceFailure::Capacity { count, bytes } => {
                return RecorderEvent::CapacityBlocked {
                    id,
                    pending_count: count,
                    pending_bytes: bytes,
                };
            }
        };
        let message = sanitize_error_text(error.to_string());
        if ambiguous {
            RecorderEvent::SaveDeferred { id, error: message }
        } else {
            RecorderEvent::SaveFailed {
                id,
                error: message,
                automatic,
            }
        }
    })
}

pub(super) fn run_accepted(mut save: AcceptedSave) -> RecorderEvent {
    let id = save.intent.id;
    match execute(&mut save) {
        Ok(ExecuteOutcome::Committed(final_path, warning)) => {
            let recovery_owned = warning.is_some();
            RecorderEvent::Saved {
                id,
                final_path,
                recovery_owned,
                durability_warning: warning,
                capacity_available: spool_has_capacity(&save),
            }
        }
        Ok(ExecuteOutcome::AlreadySettled) => RecorderEvent::AlreadySettled {
            id,
            capacity_available: spool_has_capacity(&save),
        },
        Err(error) => RecorderEvent::SaveDeferred {
            id,
            error: sanitize_error_text(format!(
                "{error}; the accepted Save remains queued for startup recovery"
            )),
        },
    }
}

enum ExecuteOutcome {
    Committed(PathBuf, Option<String>),
    AlreadySettled,
}

fn execute(save: &mut AcceptedSave) -> io::Result<ExecuteOutcome> {
    execute_with_cleanup_hook(save, |_| Ok(()))
}

fn execute_with_cleanup_hook<F>(
    save: &mut AcceptedSave,
    mut before_cleanup: F,
) -> io::Result<ExecuteOutcome>
where
    F: FnMut(&AcceptedSave) -> io::Result<()>,
{
    if let Some(barrier) = &save.close_barrier {
        barrier.wait().map_err(io::Error::other)?;
    }
    ensure_identity_contract(&save.intent)?;
    let _lock = acquire_save_lock(&save.intent)?;
    match load_journal(&save.journal_path) {
        Ok(latest) if same_request(&latest, &save.intent) => save.intent = latest,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "accepted recording journal changed immutable request fields",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ExecuteOutcome::AlreadySettled);
        }
        Err(error) => return Err(error),
    }
    ensure_identity_contract(&save.intent)?;
    if save.intent.settled {
        reclaim_settled_generations(save)?;
        return Ok(ExecuteOutcome::AlreadySettled);
    }
    let final_parent = pin_absolute_dir(&save.intent.final_dir, save.intent.final_parent_identity)?;
    let work_parent = pin_private_absolute_dir(
        &work_dir(&save.intent.final_dir),
        save.intent.work_parent_identity,
    )?;

    loop {
        let final_path = match valid_destination(&save.intent) {
            Some(path) => path,
            None => select_destination(save)?,
        };

        if let Some(expected) = save.intent.commit_identity.clone() {
            match destination_state_pinned(&save.intent, &final_parent, &final_path, &expected)? {
                DestinationState::Matches => {
                    save.intent.settled = true;
                    let journal_result = write_journal(&save.journal_path, &save.intent);
                    let outcome = finalize_known_commit(&final_parent);
                    let outcome = match (outcome, journal_result) {
                        (InstallOutcome::Durable, Ok(())) => InstallOutcome::Durable,
                        (InstallOutcome::CommittedButSyncFailed(error), _) | (_, Err(error)) => {
                            InstallOutcome::CommittedButSyncFailed(error)
                        }
                    };
                    before_cleanup(save)?;
                    return settle_commit(save, final_path, outcome)
                        .map(|(path, warning)| ExecuteOutcome::Committed(path, warning));
                }
                DestinationState::Foreign => {
                    // The path was created or replaced by another process. Never overwrite it and
                    // never interpret mere existence as proof that this intent committed. Keep
                    // the identity so a durable stage from the interrupted attempt can be reused.
                    reselect_destination(save, expected)?;
                    continue;
                }
                DestinationState::Missing => {}
            }
        } else if pinned_name_occupied(&final_parent, &final_path)? {
            // Schema-1 journals written before commit identities are still recoverable, but an
            // existing path is untrusted. Keep the source and install under a fresh name.
            clear_destination(save)?;
            continue;
        }

        let stage = prepare_or_reuse_stage(save, &work_parent)?;
        let identity = file_identity_handle(stage.generation.file()?)?;
        save.intent.commit_identity = Some(identity.clone());
        write_journal(&save.journal_path, &save.intent)?;

        let destination_name = final_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "recording destination has no basename",
            )
        })?;
        match stage
            .generation
            .promote_noreplace(&final_parent, destination_name)
        {
            Ok(destination) => {
                let current_final_parent =
                    pin_absolute_dir(&save.intent.final_dir, save.intent.final_parent_identity)?;
                let destination = current_final_parent
                    .open_existing_child(destination_name, destination.identity())?;
                let observed = file_identity_handle(destination.file()?)?;
                if observed != identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "published recording bytes differ from the exact staged generation",
                    ));
                }
                save.intent.destination_identity = Some(destination.identity());
                save.intent.settled = true;
                let outcome = match write_journal(&save.journal_path, &save.intent) {
                    Ok(()) => InstallOutcome::Durable,
                    Err(error) => InstallOutcome::CommittedButSyncFailed(error),
                };
                before_cleanup(save)?;
                return settle_commit(save, final_path, outcome)
                    .map(|(path, warning)| ExecuteOutcome::Committed(path, warning));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                // A non-cooperating writer won after selection. The no-replace primitive leaves
                // its file untouched; choose another name while this intent retains retry ownership.
                let identity = save
                    .intent
                    .commit_identity
                    .clone()
                    .expect("staged identity was persisted before install");
                reselect_destination(save, identity)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn settle_commit(
    save: &AcceptedSave,
    final_path: PathBuf,
    outcome: InstallOutcome,
) -> io::Result<(PathBuf, Option<String>)> {
    match outcome {
        InstallOutcome::Durable => match reclaim_settled_generations(save) {
            Ok(()) => Ok((final_path, None)),
            Err(error) => Ok((
                final_path,
                Some(sanitize_error_text(format!(
                    "recording saved, but exact-generation recovery cleanup was preserved: {error}"
                ))),
            )),
        },
        InstallOutcome::CommittedButSyncFailed(error) => Ok((
            final_path,
            Some(sanitize_error_text(format!(
                "recording saved, but commit durability finalization failed: {error}"
            ))),
        )),
    }
}

#[cfg(test)]
fn settle_commit_with<S, J>(
    save: &AcceptedSave,
    final_path: PathBuf,
    outcome: InstallOutcome,
    remove_source: S,
    remove_journal: J,
) -> io::Result<(PathBuf, Option<String>)>
where
    S: FnOnce(&Path) -> io::Result<()>,
    J: FnOnce(&Path) -> io::Result<()>,
{
    match outcome {
        InstallOutcome::Durable => {
            if let Err(error) = remove_source(&save.intent.source) {
                return Ok((
                    final_path,
                    Some(sanitize_error_text(format!(
                        "recording saved, but recovery-owned source cleanup failed: {error}"
                    ))),
                ));
            }
            if let Err(error) = remove_journal(&save.journal_path) {
                let restore = write_journal(&save.journal_path, &save.intent).err();
                return Ok((
                    final_path,
                    Some(sanitize_error_text(format!(
                        "recording saved, but pending-save journal cleanup failed: {error}{}",
                        restore.map_or_else(String::new, |restore| format!(
                            "; journal restoration also failed: {restore}"
                        ))
                    ))),
                ));
            }
            Ok((final_path, None))
        }
        InstallOutcome::CommittedButSyncFailed(error) => Ok((
            final_path,
            Some(sanitize_error_text(format!(
                "recording saved, but commit durability finalization failed: {error}"
            ))),
        )),
    }
}

fn select_destination(save: &mut AcceptedSave) -> io::Result<PathBuf> {
    let pinned = pin_absolute_dir(&save.intent.final_dir, save.intent.final_parent_identity)?;
    let path = unique_recording_path(
        &save.intent.final_dir,
        accepted_journal_dir(save)?,
        &save.intent.filename,
        &save.intent.ext,
        &save.intent.token,
    )?;
    pinned.verify()?;
    save.intent.destination = Some(path.clone());
    save.intent.commit_identity = None;
    save.intent.destination_identity = None;
    save.intent.settled = false;
    write_journal(&save.journal_path, &save.intent)?;
    Ok(path)
}

fn clear_destination(save: &mut AcceptedSave) -> io::Result<()> {
    save.intent.destination = None;
    save.intent.commit_identity = None;
    save.intent.destination_identity = None;
    save.intent.settled = false;
    write_journal(&save.journal_path, &save.intent)
}

fn reselect_destination(save: &mut AcceptedSave, identity: CommitIdentity) -> io::Result<PathBuf> {
    if save.intent.collision_reselected {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "the durable fallback recording name is still occupied; retained the exact save for retry",
        ));
    }
    let pinned = pin_absolute_dir(&save.intent.final_dir, save.intent.final_parent_identity)?;
    let path = unique_recording_path(
        &save.intent.final_dir,
        accepted_journal_dir(save)?,
        &save.intent.filename,
        &save.intent.ext,
        &save.intent.token,
    )?;
    pinned.verify()?;
    save.intent.destination = Some(path.clone());
    save.intent.commit_identity = Some(identity);
    save.intent.destination_identity = None;
    save.intent.settled = false;
    save.intent.collision_reselected = true;
    write_journal(&save.journal_path, &save.intent)?;
    Ok(path)
}

fn accepted_journal_dir(save: &AcceptedSave) -> io::Result<&Path> {
    save.journal_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "accepted recording journal has no parent directory",
        )
    })
}

#[cfg(test)]
fn finalize_visible_install<S, R>(
    stage: &mut PendingStage,
    destination: &Path,
    sync_destination: S,
    cleanup_stage: R,
) -> InstallOutcome
where
    S: FnOnce(&Path) -> io::Result<()>,
    R: FnOnce(&Path) -> io::Result<()>,
{
    // Keep the synced stage name durable until the final-directory entry is durable. Otherwise a
    // crash after unlinking the stage could lose both names on filesystems with delayed metadata.
    if let Err(error) = sync_destination(destination) {
        return InstallOutcome::CommittedButSyncFailed(error);
    }
    match cleanup_stage(&stage.path) {
        Ok(()) => InstallOutcome::Durable,
        Err(error) => InstallOutcome::CommittedButSyncFailed(error),
    }
}

fn finalize_known_commit(final_parent: &safe_fs::PinnedDir) -> InstallOutcome {
    if let Err(error) = final_parent.sync_directory() {
        return InstallOutcome::CommittedButSyncFailed(error);
    }
    InstallOutcome::Durable
}

fn acquire_save_lock(intent: &SaveIntent) -> io::Result<safe_fs::AdvisoryFileLock> {
    let work_parent =
        pin_private_absolute_dir(&work_dir(&intent.final_dir), intent.work_parent_identity)?;
    let expected = intent.save_lock_identity.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "recording save lock identity is missing",
        )
    })?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match work_parent.try_lock_child(
            std::ffi::OsStr::new(SAVE_LOCK_NAME),
            Some(expected),
            false,
        )? {
            Some((lock, _)) => return Ok(lock),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "timed out waiting for the recording save lock",
                ));
            }
        }
    }
}

fn unique_recording_path(
    dir: &Path,
    journals: &Path,
    filename: &str,
    ext: &str,
    current_token: &str,
) -> io::Result<PathBuf> {
    // A journal which selected a name but was cut off before rename still reserves that name.
    // This closes the fast-restart gap between the old process releasing the advisory lock and
    // startup recovery replaying its intent.
    let reserved = reserved_destinations(journals, dir, current_token);
    for n in 1..=MAX_NAME_ATTEMPTS {
        let name = if n == 1 {
            format!("{filename}.{ext}")
        } else {
            format!("{filename} ({n}).{ext}")
        };
        let candidate = dir.join(name);
        if !path_occupied(&candidate)? && !reserved.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "recording filename space exhausted",
    ))
}

fn path_occupied(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn reserved_destinations(
    journals: &Path,
    final_dir: &Path,
    current_token: &str,
) -> HashSet<PathBuf> {
    let Ok(entries) = std::fs::read_dir(journals) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| load_journal(&entry.path()).ok())
        .filter(|intent| intent.token != current_token && intent.final_dir == final_dir)
        .filter_map(|intent| valid_destination(&intent))
        .collect()
}

fn valid_destination(intent: &SaveIntent) -> Option<PathBuf> {
    intent.destination.as_ref().and_then(|path| {
        (path.parent() == Some(intent.final_dir.as_path()) && path.file_name().is_some())
            .then_some(path.clone())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DestinationState {
    Missing,
    Matches,
    Foreign,
}

#[cfg(test)]
fn atomic_install_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    safe_fs::install_file_noreplace(from, to)
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn work_dir(final_dir: &Path) -> PathBuf {
    final_dir.join(WORK_DIR_NAME)
}

fn legacy_journal_dir(final_dir: &Path) -> PathBuf {
    work_dir(final_dir).join(JOURNAL_DIR_NAME)
}

fn registry_journal_dir(temp_dir: &Path) -> PathBuf {
    ownership::pending_dir(temp_dir)
}

fn stage_path(intent: &SaveIntent) -> PathBuf {
    work_dir(&intent.final_dir).join(format!("{}.media.{}", intent.token, intent.ext))
}

pub(super) fn recover_pending(temp_dir: &Path, final_dir: &Path) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let _publication = match ownership::acquire_publication_lock(temp_dir) {
        Ok(lock) => lock,
        Err(error) => {
            report.pending = 1;
            report.admission_uncertain = true;
            report.warnings.push(sanitize_error_text(format!(
                "could not lock pending recordings: {error}"
            )));
            return report;
        }
    };
    let canonical_dir = registry_journal_dir(temp_dir);
    let legacy_cache_dir = ownership::legacy_pending_dir(temp_dir);
    cleanup_atomic_journal_temps(temp_dir, final_dir, &mut report);
    let (live_owners, stale_owners) = match inspect_owner_namespaces(temp_dir) {
        Ok(owners) => owners,
        Err(error) => {
            report.admission_uncertain = true;
            report.pending = 1;
            report.warnings.push(sanitize_error_text(format!(
                "could not inspect recorder owner leases: {error}"
            )));
            return report;
        }
    };
    let journals = match journal_paths(temp_dir, final_dir) {
        Ok(paths) => paths,
        Err(error) => {
            report.pending += 1;
            report.admission_uncertain = true;
            report.warnings.push(sanitize_error_text(format!(
                "could not scan pending recordings: {error}"
            )));
            // An incomplete directory enumeration cannot prove which temp files are ordinary.
            // Abort recovery and cleanup as one conservative unit.
            return report;
        }
    };
    let mut discovered = Vec::with_capacity(journals.len());
    let mut protected_final_roots = HashSet::new();
    for journal_path in journals {
        let intent = match load_journal(&journal_path) {
            Ok(intent) => intent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                report.pending += 1;
                report.warnings.push(sanitize_error_text(format!(
                    "could not read pending recording {}: {error}",
                    journal_path.display()
                )));
                continue;
            }
        };
        let stable_registry = journal_path.parent() == Some(canonical_dir.as_path());
        let legacy_cache_registry = journal_path.parent() == Some(legacy_cache_dir.as_path());
        let cache_source = intent.source.parent() == Some(temp_dir)
            || ownership::namespace_for_source(temp_dir, &intent.source).is_some();
        if ((stable_registry || legacy_cache_registry) && !cache_source)
            || (!stable_registry && !legacy_cache_registry && intent.final_dir != final_dir)
        {
            report.pending += 1;
            report.warnings.push(sanitize_error_text(format!(
                "pending recording has mismatched discovery fields: {}",
                journal_path.display()
            )));
            continue;
        }

        if let Some(root) = protected_final_root(temp_dir, &intent.final_dir) {
            protected_final_roots.insert(root);
        }
        if ownership::namespace_for_source(temp_dir, &intent.source)
            .is_some_and(|namespace| live_owners.contains(&namespace))
        {
            report.pending += 1;
            continue;
        }
        discovered.push((journal_path, intent));
    }

    for (journal_path, intent) in discovered {
        let mut accepted = AcceptedSave {
            journal_path: journal_path.clone(),
            intent,
            temp_root: temp_dir.to_path_buf(),
            close_barrier: None,
        };
        match execute(&mut accepted) {
            Ok(ExecuteOutcome::Committed(_, None)) => report.recovered += 1,
            Ok(ExecuteOutcome::Committed(_, Some(warning))) => {
                report.recovered += 1;
                report.pending += usize::from(journal_path.exists());
                report.warnings.push(warning);
            }
            Ok(ExecuteOutcome::AlreadySettled) => {}
            Err(error) => {
                report.pending += 1;
                report.warnings.push(sanitize_error_text(format!(
                    "pending recording recovery failed: {error}"
                )));
            }
        }
    }

    if let Some(mut preserve) = pending_sources(temp_dir, final_dir, &mut report) {
        for claim in &stale_owners {
            let namespace = claim.path();
            match claim
                .verify()
                .and_then(|()| unjournaled_stale_sources(namespace, &preserve))
            {
                Ok(sources) => {
                    for (source, _) in sources {
                        report.admission_uncertain = true;
                        report.warnings.push(format!(
                            "preserved unjournaled recording for manual recovery at {}; automatic recording remains paused",
                            recovery_path_text(&source)
                        ));
                        preserve.insert(source);
                    }
                }
                Err(error) => {
                    report.admission_uncertain = true;
                    report.warnings.push(format!(
                        "preserved unreadable recorder owner {} for manual recovery: {error}",
                        recovery_path_text(namespace),
                        error = sanitize_error_text(error.to_string())
                    ));
                    preserve.insert(namespace.to_path_buf());
                }
            }
        }
        cleanup_ordinary_temps(
            temp_dir,
            final_dir,
            &preserve,
            &protected_final_roots,
            &mut report,
        );
        cleanup_stale_owners(&stale_owners, &preserve, &mut report);
    }
    drop(stale_owners);
    match spool_usage(temp_dir, final_dir) {
        Ok(usage) => {
            report.pending = usage.count.max(report.pending);
            report.pending_bytes = usage.bytes;
            report.admission_uncertain |= usage.uncertain;
        }
        Err(error) => {
            report.admission_uncertain = true;
            report.warnings.push(sanitize_error_text(format!(
                "could not inventory the recording spool: {error}"
            )));
        }
    }
    report
}

fn inspect_owner_namespaces(
    temp_dir: &Path,
) -> io::Result<(HashSet<PathBuf>, Vec<ownership::StaleOwnerClaim>)> {
    let mut live = HashSet::new();
    let mut stale = Vec::new();
    for namespace in ownership::owner_namespaces(temp_dir)? {
        match ownership::try_claim_stale_owner(&namespace)? {
            Some(claim) => stale.push(claim),
            None => {
                live.insert(namespace);
            }
        }
    }
    Ok((live, stale))
}

fn cleanup_stale_owners(
    stale: &[ownership::StaleOwnerClaim],
    preserve: &HashSet<PathBuf>,
    report: &mut RecoveryReport,
) {
    for claim in stale {
        let namespace = claim.path();
        if preserve.iter().any(|source| source.starts_with(namespace)) {
            continue;
        }
        if claim.verify().is_ok()
            && unjournaled_stale_sources(namespace, &HashSet::new())
                .is_ok_and(|sources| sources.is_empty())
        {
            // The lease-only directory is inert. There is no portable identity-conditional
            // rmdir, so preserve it without blocking recording capacity or claiming payload.
            continue;
        }
        report.admission_uncertain = true;
        let detail = claim.verify().map_or_else(
            |error| format!("; namespace identity also became uncertain: {error}"),
            |()| String::new(),
        );
        report.warnings.push(sanitize_error_text(format!(
            "preserved stale recorder owner {} because recursive pathname cleanup cannot prove every child generation{detail}",
            namespace.display()
        )));
    }
}

fn protected_final_root(temp_dir: &Path, final_dir: &Path) -> Option<PathBuf> {
    let relative = final_dir.strip_prefix(temp_dir).ok()?;
    match relative.components().next() {
        Some(std::path::Component::Normal(name)) => Some(temp_dir.join(name)),
        None => Some(temp_dir.to_path_buf()),
        Some(_) => None,
    }
}

fn journal_paths(temp_dir: &Path, final_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = journal_paths_in(&registry_journal_dir(temp_dir))?;
    paths.extend(journal_paths_in(&ownership::legacy_pending_dir(temp_dir))?);
    paths.extend(journal_paths_in(&legacy_journal_dir(final_dir))?);
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn cleanup_atomic_journal_temps(temp_dir: &Path, final_dir: &Path, report: &mut RecoveryReport) {
    for dir in [
        registry_journal_dir(temp_dir),
        ownership::legacy_pending_dir(temp_dir),
        legacy_journal_dir(final_dir),
    ] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                report.admission_uncertain = true;
                report.warnings.push(sanitize_error_text(format!(
                    "could not inspect recorder journal temporaries in {}: {error}",
                    dir.display()
                )));
                continue;
            }
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    report.admission_uncertain = true;
                    report.warnings.push(sanitize_error_text(format!(
                        "could not inspect a recorder journal temporary: {error}"
                    )));
                    continue;
                }
            };
            let Some(target) = atomic_journal_target(&path) else {
                continue;
            };
            let mut save_lock = None;
            let removable = match load_journal(&target) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Ok(intent) => match try_acquire_save_lock(&intent) {
                    Ok(Some(lock)) => {
                        save_lock = Some(lock);
                        true
                    }
                    Ok(None) => false,
                    Err(error) => {
                        report.admission_uncertain = true;
                        report.warnings.push(sanitize_error_text(format!(
                            "could not fence recorder journal temporary {}: {error}",
                            path.display()
                        )));
                        false
                    }
                },
                Err(error) => {
                    report.admission_uncertain = true;
                    report.warnings.push(sanitize_error_text(format!(
                        "left recorder journal temporary {} because its target is unreadable: {error}",
                        path.display()
                    )));
                    false
                }
            };
            if removable && let Err(error) = safe_fs::remove_private_file_durable(&path) {
                report.admission_uncertain = true;
                report.warnings.push(sanitize_error_text(format!(
                    "could not remove recorder journal temporary {}: {error}",
                    path.display()
                )));
            }
            drop(save_lock);
        }
    }
}

fn atomic_journal_target(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?.strip_prefix('.')?;
    let (target, suffix) = name.rsplit_once(".tmp.")?;
    let token = target.strip_suffix(".json")?;
    let (pid, random) = suffix.split_once('.')?;
    if token.len() != 32
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        || pid.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || random.len() != 16
        || !random.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(path.parent()?.join(target))
}

fn try_acquire_save_lock(intent: &SaveIntent) -> io::Result<Option<safe_fs::AdvisoryFileLock>> {
    let parent =
        pin_private_absolute_dir(&work_dir(&intent.final_dir), intent.work_parent_identity)?;
    let expected = intent.save_lock_identity.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "recording save lock identity is missing",
        )
    })?;
    parent
        .try_lock_child(std::ffi::OsStr::new(SAVE_LOCK_NAME), Some(expected), false)
        .map(|locked| locked.map(|(lock, _)| lock))
}

struct SpoolUsage {
    count: usize,
    bytes: u64,
    uncertain: bool,
}

fn spool_usage(temp_dir: &Path, final_dir: &Path) -> io::Result<SpoolUsage> {
    let mut tokens = HashSet::new();
    let mut sources = HashSet::new();
    let mut objects = Vec::new();
    let mut bytes = 0u64;
    let mut uncertain = false;
    for path in journal_paths(temp_dir, final_dir)? {
        let intent = match load_journal(&path) {
            Ok(intent) => intent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !tokens.insert(intent.token.clone()) {
            continue;
        }
        sources.insert(intent.source.clone());
        match open_intent_source(&intent) {
            Ok(source) => {
                add_inventory_generation(&source, &mut objects, &mut bytes)?;
            }
            Err(_) => uncertain = true,
        }
        if let Some(stage_identity) = intent.stage_identity {
            let stage =
                pin_private_absolute_dir(&work_dir(&intent.final_dir), intent.work_parent_identity)
                    .and_then(|parent| {
                        let path = stage_path(&intent);
                        let name = path.file_name().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "recording stage has no basename",
                            )
                        })?;
                        parent.open_existing_child(name, stage_identity)
                    });
            match stage {
                Ok(stage) => add_inventory_generation(&stage, &mut objects, &mut bytes)?,
                Err(_) => uncertain = true,
            }
        }
    }
    let mut count = tokens.len();
    for namespace in ownership::owner_namespaces(temp_dir)? {
        let Some(claim) = ownership::try_claim_stale_owner(&namespace)? else {
            continue;
        };
        claim.verify()?;
        for (path, len) in unjournaled_stale_sources(claim.path(), &sources)? {
            uncertain = true;
            sources.insert(path);
            count = count.saturating_add(1);
            bytes = bytes.checked_add(len).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::StorageFull,
                    "recording spool byte count overflow",
                )
            })?;
        }
    }
    Ok(SpoolUsage {
        count,
        bytes,
        uncertain,
    })
}

fn open_intent_source(intent: &SaveIntent) -> io::Result<safe_fs::OwnedGeneration> {
    let parent_path = intent.source.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "recording source has no parent")
    })?;
    let basename = intent.source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording source has no basename",
        )
    })?;
    let parent = pin_private_absolute_dir(parent_path, intent.source_parent_identity)?;
    parent.open_existing_child_readonly(
        basename,
        intent.source_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "recording source identity is missing",
            )
        })?,
    )
}

fn add_inventory_generation(
    generation: &safe_fs::OwnedGeneration,
    identities: &mut Vec<safe_fs::FileObjectId>,
    bytes: &mut u64,
) -> io::Result<()> {
    let identity = generation.identity();
    if identities.contains(&identity) {
        return Ok(());
    }
    identities.push(identity);
    *bytes = bytes
        .checked_add(generation.file()?.metadata()?.len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::StorageFull,
                "recording spool byte count overflow",
            )
        })?;
    Ok(())
}

fn spool_has_capacity(save: &AcceptedSave) -> bool {
    capacity_available(&save.temp_root, &save.intent.final_dir)
}

pub(super) fn capacity_available(temp_dir: &Path, final_dir: &Path) -> bool {
    spool_usage(temp_dir, final_dir).is_ok_and(|usage| {
        !usage.uncertain
            && usage.count < MAX_PENDING_SAVES
            && usage.bytes < MAX_PENDING_SOURCE_BYTES
    })
}

fn unjournaled_stale_sources(
    namespace: &Path,
    owned: &HashSet<PathBuf>,
) -> io::Result<Vec<(PathBuf, u64)>> {
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(namespace)? {
        let path = entry?.path();
        if path
            .file_name()
            .is_some_and(|name| name == "lease.lock" || name.to_string_lossy().starts_with('.'))
            || owned.contains(&path)
        {
            continue;
        }
        let metadata = safe_fs::metadata_no_symlink(&path)?;
        if metadata.len() > 0 {
            sources.push((path, metadata.len()));
        }
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(sources)
}

fn recovery_path_text(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .take(1024)
        .collect()
}

fn journal_paths_in(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    collect_journal_paths(entries.map(|entry| entry.map(|entry| entry.path())))
}

fn collect_journal_paths<I>(entries: I) -> io::Result<Vec<PathBuf>>
where
    I: IntoIterator<Item = io::Result<PathBuf>>,
{
    let mut paths = entries
        .into_iter()
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn pending_sources(
    temp_dir: &Path,
    final_dir: &Path,
    report: &mut RecoveryReport,
) -> Option<HashSet<PathBuf>> {
    pending_sources_from_paths(journal_paths(temp_dir, final_dir), report)
}

fn pending_sources_from_paths(
    paths: io::Result<Vec<PathBuf>>,
    report: &mut RecoveryReport,
) -> Option<HashSet<PathBuf>> {
    let mut unreadable = false;
    let paths = match paths {
        Ok(paths) => paths,
        Err(error) => {
            report.admission_uncertain = true;
            report.warnings.push(sanitize_error_text(format!(
                "could not enumerate pending recordings while preserving sources: {error}"
            )));
            return None;
        }
    };
    let sources = paths
        .into_iter()
        .filter_map(|path| match load_journal(&path) {
            Ok(intent) => Some(intent.source),
            Err(error) => {
                unreadable = true;
                report.admission_uncertain = true;
                report.warnings.push(sanitize_error_text(format!(
                    "could not preserve source for pending recording {}: {error}",
                    path.display()
                )));
                None
            }
        })
        .collect();
    (!unreadable).then_some(sources)
}

fn cleanup_ordinary_temps(
    temp_dir: &Path,
    final_dir: &Path,
    preserve: &HashSet<PathBuf>,
    protected_final_roots: &HashSet<PathBuf>,
    report: &mut RecoveryReport,
) {
    // A user can explicitly configure the final recordings directory to the cache temp path.
    // There is then no path-only way to distinguish completed recordings from ordinary temps;
    // preserving visible final audio is the safe side of that invalid cleanup overlap.
    if final_dir == temp_dir || protected_final_roots.contains(temp_dir) {
        return;
    }
    let entries = match std::fs::read_dir(temp_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Err(error) = std::fs::create_dir_all(temp_dir) {
                report.warnings.push(sanitize_error_text(format!(
                    "could not create recording temp directory: {error}"
                )));
            }
            return;
        }
        Err(error) => {
            report.warnings.push(sanitize_error_text(format!(
                "could not scan recording temp directory: {error}"
            )));
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path == temp_dir.join(ownership::LEGACY_REGISTRY_DIR_NAME)
            || preserve.contains(&path)
            || protected_final_roots.contains(&path)
            || (final_dir.starts_with(temp_dir) && final_dir.starts_with(&path))
        {
            continue;
        }
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => report.discarded_temps += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => report.warnings.push(sanitize_error_text(format!(
                "could not remove stale recording temp {}: {error}",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(super) use test_support::*;
