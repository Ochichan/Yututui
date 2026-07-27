//! OpenSubsonic/Navidrome catalog, storage, and network integration.

pub mod actor;
pub mod auth;
mod bridge_event;
mod bridge_runtime;
#[cfg(test)]
mod bridge_runtime_tests;
pub mod bridge_store;
pub mod capabilities;
pub mod catalog;
pub mod client;
pub mod history;
pub mod linked_playlists;
pub mod model;
pub mod native_history;
pub mod origin;
mod outbound_recovery;
mod playlist_create_recovery;
pub mod private_store;
pub mod profile;
pub mod proxy;
pub mod publish;
pub mod rating;
pub mod scan;
pub use scan::{LibraryScanRequest, request_library_scan};
pub mod transaction;
mod wire;

pub use actor::{
    OpenSubsonicHandle, OpenSubsonicPlaylistReceipt, OpenSubsonicProfileSummary,
    OpenSubsonicRatingReceipt, OpenSubsonicRuntime, OpenSubsonicScrobbleReceipt,
    OpenSubsonicStatus, OpenSubsonicStatusKind, OutboundScrobbleResolution, PlaylistMergePreview,
    PlaylistPreviewMode, PlaylistPreviewTarget, PreparedSetup, ServerLibraryDetailRequest,
    ServerLibraryRequest, ServiceError, SetupIdentityIntent, SetupInput, commit_setup,
    current_handle, current_proxy_handle, disable_native_history, load_actor, load_actor_read_only,
    load_actor_with_bridge_sink, read_status, remove_profile, test_and_prepare_setup,
    test_connection, test_connection_read_only,
};
pub use bridge_event::{
    OpenSubsonicBridgeImport, OpenSubsonicBridgeSink, OpenSubsonicScrobbleKind,
};
pub use bridge_store::{
    NativeHistoryHealth, OpenSubsonicBridgeState, PendingPlaylistImportPurpose,
};
pub use capabilities::{ServerCapabilities, ServerFeature};
pub use catalog::{MAX_PAGE_SIZE, OpenSubsonicCatalog};
pub use client::{BinaryPayload, OpenSubsonicClient, ServerError, ServerInfo};
pub use history::{
    AggregateHistoryShadow, AggregatePlan, AggregatePlay, MAX_AGGREGATE_DELTA, parse_rfc3339_unix,
    plan_aggregate_history,
};
pub use linked_playlists::{
    IndexedLinkedEntry, IndexedRemoteOccurrence, InitialMergeOccurrence, InitialMergePlan,
    InitialMergePreview, LinkedPlaylistEntry, LinkedPlaylistError, MAX_LINKED_PLAYLIST_ENTRIES,
    OccurrenceMatch, PlaylistSequence, RemoteDelta, RemotePlaylistUpdatePlan, plan_initial_merge,
    plan_remote_delta, plan_remote_update,
};
pub use model::{
    AccountScopeId, AlbumId, ArtistId, BackendId, CoverArtId, ItemId, LibraryWarning, ModelError,
    OpenSubsonicItemRef, Page, ServerAlbum, ServerArtist, ServerLibraryDetail, ServerLibraryPage,
    ServerLibraryRow, ServerLibrarySection, ServerPlaylist, ServerPlaylistAccess, ServerPlaylistId,
    ServerPlaylistLinkHealth, ServerPlaylistLinkSummary, ServerPlaylistSummary, ServerSong,
};
pub use native_history::{
    MAX_NATIVE_HISTORY_PAGES, MAX_NATIVE_HISTORY_ROWS, NATIVE_HISTORY_PAGE_SIZE,
    NativeHistoryCredential, NativeHistoryError, NativeHistorySession, NativeScrobblePage,
    NativeScrobblePageRequest, NativeScrobbleRow, NativeScrobbleScan, NavidromeNativeClient,
};
pub use origin::{ConfiguredPrivateOrigin, OriginError};
pub use outbound_recovery::{list_scrobble_attention_ids, resolve_scrobble_attention};
pub use playlist_create_recovery::{
    PlaylistCreateAttention, PlaylistCreateRecoveryState, abandon_playlist_create_attention,
    list_playlist_create_attention,
};
pub use private_store::{CredentialKind, OpenSubsonicPrivateState, ServerCredential};
pub use profile::{OpenSubsonicPaths, OpenSubsonicProfile, StoreError};
pub use rating::{RawServerRating, canonical_server_rating, map_server_rating};
pub use transaction::{
    OpenSubsonicStoreSet, StoreRevisions, commit_store_set, load_store_set, recover_store_set,
    remove_store_set, reset_store_set,
};

/// Bounded owner-lane handoff. Durable submissions exceeding this transient window remain in the
/// scrobble JSONL source journal and replay after restart; ephemeral now-playing reports yield first.
pub(crate) const OWNER_PLAYBACK_REPORT_QUEUE_MAX: usize = 1024;
