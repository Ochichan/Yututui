//! Blocking disk work for the radio recorder.
//!
//! Save requests cross a durable acceptance boundary before they enter Tokio's blocking pool.
//! [`durable`] owns that journal, crash recovery, and the serialized final-name commit. Reducers
//! still decide *what* to keep; this module only owns file lifetime and result reporting.

mod durable;

use std::path::PathBuf;
use std::time::Instant;

pub use durable::{AcceptedSave, RecoveryReport};

/// A unit of disk work handed to the runtime's tracked blocking-task set.
pub enum RecorderJob {
    /// Wait for every exact mpv reply in one recorder transition. The runtime reports the
    /// correlated outcome through [`RecorderEvent::TransitionResolved`].
    AwaitTransition {
        transition_id: u64,
        close: Option<super::barrier::CommandBarrier>,
        open: Option<super::barrier::CommandBarrier>,
    },
    /// Copy a kept temp recording into the recordings folder and tag it.
    Save {
        /// Correlates the result back to the [`super::RecordedTrack`].
        id: u64,
        temp: PathBuf,
        /// Stable cache-owned root used to discover accepted intents across destination changes.
        temp_dir: PathBuf,
        final_dir: PathBuf,
        /// Sanitized base name (no extension).
        filename: String,
        ext: &'static str,
        title: Option<String>,
        artist: Option<String>,
        station: Option<String>,
        /// Correlated acknowledgement of the final stream-record property command. `None` is
        /// used by startup recovery and manual saves whose close was already acknowledged.
        close_barrier: Option<super::barrier::CommandBarrier>,
        /// Automatic Everything-mode work is subject to the durable spool ceilings. An explicit
        /// Decide-mode Save remains available so the user can drain a kept local source.
        automatic: bool,
        /// Orderly shutdown may publish the single already-existing blocked source as an
        /// automatic-origin journal without counting it as another live-recording admission.
        bypass_limits: bool,
    },
    /// Delete a temp recording (too short, discarded, or evicted from history).
    Discard {
        temp: PathBuf,
        close_barrier: Option<super::barrier::CommandBarrier>,
    },
    /// Re-scan the durable spool after a blocked owner's terminal event reported no capacity.
    /// Only this fresh correlated result may consume a later worker's positive capacity hint.
    ProbeCapacity {
        owner_id: u64,
        temp_dir: PathBuf,
        final_dir: PathBuf,
    },
}

/// Result of a [`RecorderJob::Save`]. `Discard` reports nothing back.
#[derive(Debug)]
pub enum RecorderEvent {
    TransitionResolved {
        transition_id: u64,
        close: Option<Result<(), String>>,
        open: Option<Result<(), String>>,
    },
    /// The runtime synchronously published and synced the Save journal before attempting worker
    /// admission. From this point startup recovery owns the source even if shutdown wins.
    SaveAccepted { id: u64 },
    Saved {
        id: u64,
        final_path: PathBuf,
        /// The destination is visible, but the journal/source remain owned by recovery until a
        /// later retry proves the directory entry and removes the journal durably.
        recovery_owned: bool,
        /// The complete destination is visible, but syncing its directory entry failed. This is
        /// a success (retrying would create a duplicate) with an observable durability warning.
        durability_warning: Option<String>,
        /// True only after rescanning the durable registry proves another automatic Save can fit.
        capacity_available: bool,
    },
    /// A post-acceptance attempt failed. The UI must not offer a duplicate retry: the durable
    /// journal and original source remain owned by startup recovery even if this event is lost.
    SaveDeferred { id: u64, error: String },
    /// Another worker/recovery process settled the exact durable intent first. The optimistic
    /// saved state remains valid and retry must stay disabled, but this stale worker has no path.
    AlreadySettled { id: u64, capacity_available: bool },
    /// The request was rejected before its durable acceptance boundary (for example, because
    /// the source was already missing). No recovery intent exists, so a visible retry is safe.
    SaveFailed {
        id: u64,
        error: String,
        /// Automatic pre-acceptance failures pause recording and retain their source; manual
        /// Decide-mode failures remain directly retryable without affecting recorder admission.
        automatic: bool,
    },
    /// Automatic saving was rejected before publication because the bounded durable spool is at
    /// its item or byte ceiling. The source remains a normal kept recording.
    CapacityBlocked {
        id: u64,
        pending_count: usize,
        pending_bytes: u64,
    },
    CapacityProbed {
        owner_id: u64,
        capacity_available: bool,
    },
}

/// Persist a Save intent before it is admitted to the blocking pool.
///
/// Once this returns `Ok`, a process cutoff can delay the copy but cannot silently turn the
/// explicit user action back into an ordinary startup-wiped temp recording.
pub fn accept_save(job: RecorderJob) -> Result<AcceptedSave, RecorderEvent> {
    durable::accept(job)
}

/// Resume one already accepted Save intent on the current blocking thread.
pub fn run_accepted(save: AcceptedSave) -> RecorderEvent {
    durable::run_accepted(save)
}

/// Recover accepted Saves and then clean ordinary incomplete/Decide temp recordings.
///
/// This is blocking filesystem work. Callers must run it through the tracked runtime task set.
pub fn recover_pending(temp_dir: &std::path::Path, final_dir: &std::path::Path) -> RecoveryReport {
    durable::recover_pending(temp_dir, final_dir)
}

/// Execute a job on the current (blocking) thread. Returns an event only for `Save`.
///
/// Runtime dispatch uses [`accept_save`] before spawning and then [`run_accepted`]. This combined
/// entry point remains useful for focused tests and non-runtime callers.
pub fn run(job: RecorderJob) -> Option<RecorderEvent> {
    match job {
        RecorderJob::AwaitTransition {
            transition_id,
            close,
            open,
        } => {
            let deadline = Instant::now() + super::barrier::COMMAND_ACK_TIMEOUT;
            Some(RecorderEvent::TransitionResolved {
                transition_id,
                close: close.map(|barrier| barrier.wait_until(deadline)),
                open: open.map(|barrier| barrier.wait_until(deadline)),
            })
        }
        save @ RecorderJob::Save { .. } => Some(match accept_save(save) {
            Ok(accepted) => run_accepted(accepted),
            Err(event) => event,
        }),
        RecorderJob::Discard {
            temp,
            close_barrier,
        } => {
            if close_barrier.is_none_or(|barrier| barrier.wait().is_ok()) {
                let _ = std::fs::remove_file(temp);
            }
            None
        }
        RecorderJob::ProbeCapacity {
            owner_id,
            temp_dir,
            final_dir,
        } => Some(RecorderEvent::CapacityProbed {
            owner_id,
            capacity_available: durable::capacity_available(&temp_dir, &final_dir),
        }),
    }
}

pub(super) fn tag_file_handle(
    file: &mut std::fs::File,
    title: Option<&str>,
    artist: Option<&str>,
    album: Option<&str>,
) -> Result<(), String> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, Tag, TagExt};

    use std::io::{Seek as _, SeekFrom};

    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut tagged = Probe::new(&mut *file)
        .guess_file_type()
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())?;

    let tag_type = tagged.primary_tag_type();
    if tagged.primary_tag().is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged
        .primary_tag_mut()
        .ok_or_else(|| "no writable tag".to_owned())?;

    if let Some(t) = title {
        tag.set_title(t.to_owned());
    }
    if let Some(a) = artist {
        tag.set_artist(a.to_owned());
    }
    if let Some(al) = album {
        tag.set_album(al.to_owned());
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    tag.save_to(&mut *file, WriteOptions::default())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
