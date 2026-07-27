//! Asking a music server to rescan its library after a track was published into its folder.

use super::ServerError;
use super::client::OpenSubsonicClient;
use super::profile::OpenSubsonicPaths;
use super::transaction::load_store_set_read_only;

/// What asking the server to rescan its library actually achieved.
///
/// Every variant except `Started` is advisory. A publish has already written its bytes into the
/// music folder by the time a scan is requested, so a server that cannot or will not scan is a
/// slower route to the same result — never a failed publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryScanRequest {
    /// The server accepted the request.
    Started,
    /// No music server is configured, so there is nothing to ask.
    NoServer,
    /// The server does not implement `startScan`, or a proxy hides it.
    Unsupported,
    /// This account may not trigger a scan. Navidrome restricts scanning to admins.
    NotPermitted,
    /// The server could not be reached, or the outcome could not be established.
    Unavailable,
}

/// Ask the configured server to rescan its library.
///
/// Read-only with respect to local state: this never touches the store set, so it is safe to call
/// while holding nothing but a reader lease.
pub async fn request_library_scan(paths: &OpenSubsonicPaths) -> LibraryScanRequest {
    let Ok(Some(store_set)) = load_store_set_read_only(paths) else {
        return LibraryScanRequest::NoServer;
    };
    let Ok(client) = OpenSubsonicClient::connect(&store_set.profile).await else {
        return LibraryScanRequest::Unavailable;
    };
    match client
        .start_scan(store_set.private_state.credential())
        .await
    {
        Ok(()) => LibraryScanRequest::Started,
        Err(ServerError::UnsupportedFeature) => LibraryScanRequest::Unsupported,
        // Only an authorisation refusal means "this account may not scan". A credential the
        // server would not accept is a different problem with a different fix, and telling the
        // user their account lacks the right would send them to the wrong place.
        Err(ServerError::PermissionDenied) => LibraryScanRequest::NotPermitted,
        Err(_) => LibraryScanRequest::Unavailable,
    }
}
