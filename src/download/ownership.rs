use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use super::{DownloadCmd, DownloadEvent, ImportDownloadContext};

pub(super) type DownloadAdmissionId = u64;
pub(super) const DOWNLOAD_OUTSTANDING_MAX: usize = 1_152;

pub(super) struct AdmittedDownloadCmd {
    pub(super) admission_id: Option<DownloadAdmissionId>,
    pub(super) command: DownloadCmd,
}

impl AdmittedDownloadCmd {
    pub(super) fn tracked(admission_id: DownloadAdmissionId, command: DownloadCmd) -> Self {
        Self {
            admission_id: Some(admission_id),
            command,
        }
    }

    pub(super) fn untracked(command: DownloadCmd) -> Self {
        Self {
            admission_id: None,
            command,
        }
    }
}

#[derive(Default)]
struct OutstandingState {
    next_id: DownloadAdmissionId,
    downloads: HashMap<DownloadAdmissionId, OwnedDownload>,
}

enum OwnedDownload {
    Running(DownloadOwner),
    TerminalPending(DownloadEvent),
}

pub(super) enum DownloadRegistration {
    Registered(DownloadAdmissionId),
    DuplicateImport,
    Full,
}

#[derive(Debug, Clone)]
pub(super) enum DownloadOwner {
    Ordinary(String),
    Import(ImportDownloadContext),
}

impl DownloadOwner {
    #[cfg(test)]
    pub(super) fn video_id(&self) -> &str {
        match self {
            Self::Ordinary(video_id) => video_id,
            Self::Import(context) => &context.video_id,
        }
    }

    pub(super) fn unexpected_error(self, error: String) -> DownloadEvent {
        match self {
            Self::Ordinary(video_id) => DownloadEvent::Error { video_id, error },
            Self::Import(context) => {
                if let Err(record_error) =
                    crate::transfer::session::record_import_download_interruption(
                        &context.claim,
                        &error,
                    )
                {
                    tracing::warn!(
                        error = %record_error,
                        claim_id = %context.claim.claim_id,
                        "could not persist interrupted import download"
                    );
                }
                DownloadEvent::ImportError { context, error }
            }
        }
    }

    fn has_same_import_claim(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Import(left), Self::Import(right)) if left.claim == right.claim
        )
    }

    fn matches_event(&self, event: &DownloadEvent) -> bool {
        match (self, event) {
            (
                Self::Ordinary(owner_video_id),
                DownloadEvent::Done { video_id, .. } | DownloadEvent::Error { video_id, .. },
            ) => owner_video_id == video_id,
            (
                Self::Import(owner),
                DownloadEvent::ImportDone { context, .. }
                | DownloadEvent::ImportError { context, .. },
            ) => owner == context,
            _ => false,
        }
    }
}

#[derive(Default)]
pub(super) struct OutstandingDownloads {
    state: Mutex<OutstandingState>,
    intentional_shutdown: AtomicBool,
}

impl OutstandingDownloads {
    pub(super) fn register_for_admission(&self, owner: DownloadOwner) -> DownloadRegistration {
        let mut state = self.lock();
        if matches!(owner, DownloadOwner::Import(_))
            && state.downloads.values().any(|owned| match owned {
                OwnedDownload::Running(existing) => existing.has_same_import_claim(&owner),
                OwnedDownload::TerminalPending(event) => match (&owner, event) {
                    (
                        DownloadOwner::Import(candidate),
                        DownloadEvent::ImportDone { context, .. }
                        | DownloadEvent::ImportError { context, .. },
                    ) => candidate.claim == context.claim,
                    _ => false,
                },
            })
        {
            return DownloadRegistration::DuplicateImport;
        }
        if state.downloads.len() >= DOWNLOAD_OUTSTANDING_MAX {
            return DownloadRegistration::Full;
        }
        let admission_id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("download admission id exhausted");
        let previous = state
            .downloads
            .insert(admission_id, OwnedDownload::Running(owner));
        debug_assert!(previous.is_none());
        DownloadRegistration::Registered(admission_id)
    }

    /// Register before queue admission so even an immediately completed actor command is owned.
    #[cfg(test)]
    pub(super) fn register(&self, owner: DownloadOwner) -> Option<DownloadAdmissionId> {
        let mut state = self.lock();
        if state.downloads.len() >= DOWNLOAD_OUTSTANDING_MAX {
            return None;
        }
        let admission_id = state.next_id;
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("download admission id exhausted");
        let previous = state
            .downloads
            .insert(admission_id, OwnedDownload::Running(owner));
        debug_assert!(previous.is_none());
        Some(admission_id)
    }

    /// Retire only after the corresponding terminal event has been accepted by its owner.
    pub(super) fn retire(&self, admission_id: DownloadAdmissionId) -> bool {
        self.lock().downloads.remove(&admission_id).is_some()
    }

    pub(super) fn mark_terminal_pending(
        &self,
        admission_id: DownloadAdmissionId,
        event: DownloadEvent,
    ) -> bool {
        let mut state = self.lock();
        let Some(owned) = state.downloads.get_mut(&admission_id) else {
            return false;
        };
        let OwnedDownload::Running(owner) = owned else {
            return false;
        };
        if !owner.matches_event(&event) {
            tracing::error!(
                admission_id,
                "download terminal identity did not match its owner"
            );
            return false;
        }
        *owned = OwnedDownload::TerminalPending(event);
        true
    }

    pub(super) fn terminal_event(
        &self,
        admission_id: DownloadAdmissionId,
    ) -> Option<DownloadEvent> {
        match self.lock().downloads.get(&admission_id) {
            Some(OwnedDownload::TerminalPending(event)) => Some(event.clone()),
            _ => None,
        }
    }

    pub(super) fn terminal_pending_ids(&self) -> Vec<DownloadAdmissionId> {
        let mut ids: Vec<_> = self
            .lock()
            .downloads
            .iter()
            .filter_map(|(&admission_id, owned)| {
                matches!(owned, OwnedDownload::TerminalPending(_)).then_some(admission_id)
            })
            .collect();
        ids.sort_unstable();
        ids
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Vec<(DownloadAdmissionId, String)> {
        let mut downloads: Vec<_> = self
            .lock()
            .downloads
            .iter()
            .map(|(&admission_id, owned)| {
                let video_id = match owned {
                    OwnedDownload::Running(owner) => owner.video_id(),
                    OwnedDownload::TerminalPending(event) => match event {
                        DownloadEvent::Done { video_id, .. }
                        | DownloadEvent::Error { video_id, .. } => video_id,
                        DownloadEvent::ImportDone { context, .. }
                        | DownloadEvent::ImportError { context, .. } => &context.video_id,
                        DownloadEvent::Progress { .. } | DownloadEvent::ImportProgress { .. } => {
                            unreachable!("progress is never terminal-owned")
                        }
                    },
                };
                (admission_id, video_id.to_owned())
            })
            .collect();
        downloads.sort_unstable_by_key(|(admission_id, _)| *admission_id);
        downloads
    }

    pub(super) fn running_snapshot(&self) -> Vec<(DownloadAdmissionId, DownloadOwner)> {
        let mut downloads: Vec<_> = self
            .lock()
            .downloads
            .iter()
            .filter_map(|(&admission_id, owned)| match owned {
                OwnedDownload::Running(owner) => Some((admission_id, owner.clone())),
                OwnedDownload::TerminalPending(_) => None,
            })
            .collect();
        downloads.sort_unstable_by_key(|(admission_id, _)| *admission_id);
        downloads
    }

    pub(super) fn begin_intentional_shutdown(&self) {
        self.intentional_shutdown.store(true, Ordering::Release);
    }

    pub(super) fn is_intentional_shutdown(&self) -> bool {
        self.intentional_shutdown.load(Ordering::Acquire)
    }

    pub(super) fn clear(&self) {
        self.lock().downloads.clear();
    }

    fn lock(&self) -> MutexGuard<'_, OutstandingState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("download ownership mutex poisoned; recovering");
            poisoned.into_inner()
        })
    }
}
