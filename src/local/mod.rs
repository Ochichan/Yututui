//! Local Deck domain types.
//!
//! This module stays independent from scanning and storage. Scanners can produce
//! [`LocalTrack`] values, index stores can persist them, and the app/UI can turn
//! them into ordinary [`crate::api::Song`] values for playback.

pub mod find;
pub mod index;
pub mod metadata;
pub mod model;
pub mod query;
pub mod scan;

pub use index::{LocalIndex, LocalIndexLoad, LocalIndexLoadWarning, default_index_path};
pub use model::{
    AudioFormat, FileFingerprint, LocalAlbum, LocalAlbumId, LocalArtist, LocalArtistId, LocalRowId,
    LocalSmartList, LocalTrack, LocalTrackId,
};
pub use scan::{
    LocalScanProgress, LocalScanResult, LocalScanRoot, LocalScanSummary, ScanError, scan_roots,
    scan_roots_with_progress,
};
