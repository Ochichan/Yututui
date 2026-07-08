//! Local Deck domain types.
//!
//! This module stays independent from scanning and storage. Scanners can produce
//! [`LocalTrack`] values, index stores can persist them, and the app/UI can turn
//! them into ordinary [`crate::api::Song`] values for playback.

pub mod index;
pub mod metadata;
pub mod model;
pub mod query;
pub mod scan;

pub use index::{LocalIndex, default_index_path};
pub use model::{
    AudioFormat, FileFingerprint, LocalAlbum, LocalAlbumId, LocalArtist, LocalArtistId, LocalTrack,
    LocalTrackId,
};
pub use scan::{LocalScanResult, LocalScanRoot, LocalScanSummary, ScanError, scan_roots};
