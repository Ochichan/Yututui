//! Portable listening records projected from the synchronized personal-state ledger.

mod model;
mod playback;
mod projection;

pub use model::{
    BookmarkId, BookmarkRecord, DjPreset, DjPresetId, ListeningOperation, PassportNote,
    PassportVisit, ResumeCandidate, ResumeClear, ResumePoint, ResumeProvenance,
};
pub use playback::{
    ListeningLoadReason, ListeningPlaybackState, PendingListeningSeek, PendingSeekOutcome,
    PlaybackMemoryAction, StationVisitTarget, format_listening_projection, portable_track,
    radio_station_target, resolve_portable_track,
};
pub use projection::{ListeningProjection, ResumeState};

#[cfg(test)]
mod tests;
