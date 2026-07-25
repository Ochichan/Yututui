//! OpenSubsonic/Navidrome catalog, storage, and network integration.

pub mod actor;
pub mod auth;
pub mod bridge_store;
pub mod capabilities;
pub mod catalog;
pub mod client;
pub mod model;
pub mod origin;
pub mod private_store;
pub mod profile;
pub mod proxy;
pub mod transaction;
mod wire;

pub use actor::{
    OpenSubsonicHandle, OpenSubsonicProfileSummary, OpenSubsonicRuntime, OpenSubsonicStatus,
    OpenSubsonicStatusKind, PreparedSetup, ServerLibraryDetailRequest, ServerLibraryRequest,
    ServiceError, SetupIdentityIntent, SetupInput, commit_setup, current_handle,
    current_proxy_handle, load_actor, load_actor_read_only, read_status, remove_profile,
    test_and_prepare_setup, test_connection, test_connection_read_only,
};
pub use bridge_store::OpenSubsonicBridgeState;
pub use capabilities::{ServerCapabilities, ServerFeature};
pub use catalog::{MAX_PAGE_SIZE, OpenSubsonicCatalog};
pub use client::{BinaryPayload, OpenSubsonicClient, ServerError, ServerInfo};
pub use model::{
    AccountScopeId, AlbumId, ArtistId, BackendId, CoverArtId, ItemId, LibraryWarning, ModelError,
    OpenSubsonicItemRef, Page, ServerAlbum, ServerArtist, ServerLibraryDetail, ServerLibraryPage,
    ServerLibraryRow, ServerLibrarySection, ServerPlaylist, ServerPlaylistId,
    ServerPlaylistSummary, ServerSong,
};
pub use origin::{ConfiguredPrivateOrigin, OriginError};
pub use private_store::{CredentialKind, OpenSubsonicPrivateState, ServerCredential};
pub use profile::{OpenSubsonicPaths, OpenSubsonicProfile, StoreError};
pub use transaction::{
    OpenSubsonicStoreSet, StoreRevisions, commit_store_set, load_store_set, recover_store_set,
    remove_store_set, reset_store_set,
};
