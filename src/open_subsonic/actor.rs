//! Long-lived credential owner plus setup/status lifecycle facade.

mod playlist_catalog;
mod playlist_ownership;
mod playlists;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use age::secrecy::ExposeSecret as _;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use zeroize::Zeroize;

use super::bridge_event::{OpenSubsonicBridgeSink, OpenSubsonicScrobbleKind};
use super::bridge_runtime::BridgeRuntime;
pub use super::bridge_runtime::OutboundScrobbleResolution;
use super::capabilities::{ServerCapabilities, ServerFeature};
use super::catalog::OpenSubsonicCatalog;
use super::client::{BinaryPayload, OpenSubsonicClient, ServerError};
use super::model::{
    AccountScopeId, AlbumId, ArtistId, BackendId, CoverArtId, OpenSubsonicItemRef,
    ServerLibraryDetail, ServerLibraryPage, ServerLibrarySection, ServerPlaylistId, ServerSong,
};
use super::origin::ConfiguredPrivateOrigin;
use super::playlist_create_recovery::{
    PlaylistCreateAttention, playlist_create_attention_from_state,
};
use super::private_store::{CredentialKind, OpenSubsonicPrivateState, ServerCredential};
use super::profile::{OpenSubsonicPaths, OpenSubsonicProfile, StoreError};
use super::proxy::{
    OpenSubsonicProxyGuard, OpenSubsonicProxyHandle, ProxyUpstreamError, StreamRequest,
    StreamSource, StreamSourceFuture, UpstreamStream,
};
use super::transaction::{
    OpenSubsonicStoreSet, StoreRevisions, commit_store_set, load_store_set,
    load_store_set_read_only, remove_store_set, reset_store_set,
};
use super::{NativeHistoryHealth, OpenSubsonicBridgeState};
use crate::personal_state::OpenSubsonicRatingWinner;

use playlist_catalog::{
    PlaylistCatalogSession, finalize_detail_playlist_access, finalize_page_playlist_access,
};
use playlists::PlaylistPreviewCache;
pub(crate) use playlists::remote_fingerprint as playlist_snapshot_fingerprint;
pub use playlists::{PlaylistMergePreview, PlaylistPreviewMode, PlaylistPreviewTarget};

#[derive(Default)]
struct ActorPlaylistState {
    previews: PlaylistPreviewCache,
    catalog_session: PlaylistCatalogSession,
}

const ACTOR_CAPACITY: usize = 32;
const BRIDGE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const NATIVE_HISTORY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const HISTORY_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    Store(StoreError),
    Server(ServerError),
    ActorUnavailable,
    ProxyUnavailable,
    InvalidSetup,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Store(error) => return error.fmt(formatter),
            Self::Server(error) => return error.fmt(formatter),
            Self::ActorUnavailable => "music server service is unavailable",
            Self::ProxyUnavailable => "music server playback proxy is unavailable",
            Self::InvalidSetup => "music server setup is invalid",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ServiceError {}

impl From<StoreError> for ServiceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ServerError> for ServiceError {
    fn from(error: ServerError) -> Self {
        Self::Server(error)
    }
}

/// Secret-bearing setup input. Deliberately lacks `Debug` and `Clone`.
pub struct SetupInput {
    display_name: String,
    origin: String,
    allow_lan_http: bool,
    custom_ca_pem: Option<Vec<u8>>,
    credential: Option<ServerCredential>,
    identity_intent: SetupIdentityIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupIdentityIntent {
    Create,
    UpdateSameServerAndAccount,
    ReplaceServerOrAccount,
}

impl SetupInput {
    pub fn new(
        display_name: impl Into<String>,
        origin: impl Into<String>,
        allow_lan_http: bool,
        custom_ca_pem: Option<Vec<u8>>,
        credential: ServerCredential,
        identity_intent: SetupIdentityIntent,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            origin: origin.into(),
            allow_lan_http,
            custom_ca_pem,
            credential: Some(credential),
            identity_intent,
        }
    }
}

impl Drop for SetupInput {
    fn drop(&mut self) {
        self.origin.zeroize();
        if let Some(pem) = &mut self.custom_ca_pem {
            pem.zeroize();
        }
    }
}

/// Tested candidate with no durable side effects. Deliberately lacks `Debug`.
pub struct PreparedSetup {
    expected: StoreRevisions,
    store_set: Option<OpenSubsonicStoreSet>,
    capabilities: ServerCapabilities,
}

impl PreparedSetup {
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSubsonicStatusKind {
    Off,
    UpToDate,
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSubsonicStatus {
    pub kind: OpenSubsonicStatusKind,
    pub display_name: Option<String>,
    pub backend_id: Option<BackendId>,
    pub account_scope_id: Option<AccountScopeId>,
    pub credential_kind: Option<CredentialKind>,
    pub uses_lan_http: bool,
    /// Whether a user-provided certificate authority is configured.
    ///
    /// This deliberately exposes only the redacted presence bit, never certificate bytes,
    /// fingerprints, paths, or the configured origin.
    pub uses_custom_ca: bool,
    /// Experimental exact Navidrome history is enabled. No username/password is exposed.
    pub native_history_enabled: bool,
    /// Actual redacted native-history capability/health; standard play-count history is separate.
    pub native_history_health: NativeHistoryHealth,
    /// Bounded count of opaque outbound reports requiring an explicit delivery decision.
    pub outbound_scrobbles_needing_attention: usize,
    /// Replay-unsafe playlist creates requiring an explicit keep/abandon decision.
    pub playlist_creates_needing_attention: usize,
    /// Redacted local identities and recovery states for replay-unsafe playlist creates.
    pub playlist_create_attention: Vec<PlaylistCreateAttention>,
    /// Linked playlists deleted on the server and awaiting an explicit keep/restore decision.
    pub playlist_links_needing_decision: usize,
    /// Linked playlist writes that exhausted bounded verification and require reconnection.
    pub playlist_projections_needing_attention: usize,
    /// Linked local playlists containing tracks outside this exact server/account.
    pub playlist_contents_needing_attention: usize,
}

impl OpenSubsonicStatus {
    fn off() -> Self {
        Self {
            kind: OpenSubsonicStatusKind::Off,
            display_name: None,
            backend_id: None,
            account_scope_id: None,
            credential_kind: None,
            uses_lan_http: false,
            uses_custom_ca: false,
            native_history_enabled: false,
            native_history_health: NativeHistoryHealth::Off,
            outbound_scrobbles_needing_attention: 0,
            playlist_creates_needing_attention: 0,
            playlist_create_attention: Vec::new(),
            playlist_links_needing_decision: 0,
            playlist_projections_needing_attention: 0,
            playlist_contents_needing_attention: 0,
        }
    }

    fn needs_attention() -> Self {
        Self {
            kind: OpenSubsonicStatusKind::NeedsAttention,
            display_name: None,
            backend_id: None,
            account_scope_id: None,
            credential_kind: None,
            uses_lan_http: false,
            uses_custom_ca: false,
            native_history_enabled: false,
            native_history_health: NativeHistoryHealth::Off,
            outbound_scrobbles_needing_attention: 0,
            playlist_creates_needing_attention: 0,
            playlist_create_attention: Vec::new(),
            playlist_links_needing_decision: 0,
            playlist_projections_needing_attention: 0,
            playlist_contents_needing_attention: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSubsonicProfileSummary {
    pub display_name: String,
    pub backend_id: BackendId,
    pub account_scope_id: AccountScopeId,
    pub credential_kind: CredentialKind,
    pub uses_lan_http: bool,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerLibraryRequest {
    pub section: ServerLibrarySection,
    pub offset: u32,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLibraryDetailRequest {
    Album(AlbumId),
    Artist(ArtistId),
    Playlist(ServerPlaylistId),
}

pub async fn test_and_prepare_setup(
    paths: &OpenSubsonicPaths,
    mut input: SetupInput,
) -> Result<PreparedSetup, ServiceError> {
    let current = load_store_set(paths)?;
    let expected = current
        .as_ref()
        .map_or(StoreRevisions::MISSING, OpenSubsonicStoreSet::revisions);
    let (backend_id, account_scope_id) = match (current.as_ref(), input.identity_intent) {
        (None, SetupIdentityIntent::Create) => (
            BackendId::random().map_err(|_| ServiceError::InvalidSetup)?,
            AccountScopeId::random().map_err(|_| ServiceError::InvalidSetup)?,
        ),
        (Some(state), SetupIdentityIntent::UpdateSameServerAndAccount) => (
            state.profile.backend_id().clone(),
            state.profile.account_scope_id().clone(),
        ),
        (Some(_), SetupIdentityIntent::ReplaceServerOrAccount) => (
            BackendId::random().map_err(|_| ServiceError::InvalidSetup)?,
            AccountScopeId::random().map_err(|_| ServiceError::InvalidSetup)?,
        ),
        (None, SetupIdentityIntent::UpdateSameServerAndAccount)
        | (None, SetupIdentityIntent::ReplaceServerOrAccount)
        | (Some(_), SetupIdentityIntent::Create) => return Err(ServiceError::InvalidSetup),
    };
    let display_name = std::mem::take(&mut input.display_name);
    let raw_origin = std::mem::take(&mut input.origin);
    let custom_ca_pem = input.custom_ca_pem.take();
    let credential = input.credential.take().ok_or(ServiceError::InvalidSetup)?;
    let origin = ConfiguredPrivateOrigin::new(&raw_origin, input.allow_lan_http)
        .map_err(|_| ServiceError::InvalidSetup)?;
    let profile = OpenSubsonicProfile::with_ids(
        0,
        backend_id.clone(),
        account_scope_id.clone(),
        &display_name,
        origin,
        custom_ca_pem,
    )?;
    let mut private_state =
        OpenSubsonicPrivateState::new(backend_id.clone(), account_scope_id.clone(), credential);
    if input.identity_intent == SetupIdentityIntent::UpdateSameServerAndAccount {
        if private_state.credential_kind() == CredentialKind::Password {
            require_same_account_owner(
                current
                    .as_ref()
                    .ok_or(ServiceError::InvalidSetup)?
                    .private_state
                    .credential(),
                private_state.credential(),
            )?;
        }
        private_state.preserve_native_history_from(
            &current
                .as_ref()
                .ok_or(ServiceError::InvalidSetup)?
                .private_state,
        )?;
    }
    let mut bridge_state =
        if input.identity_intent == SetupIdentityIntent::UpdateSameServerAndAccount {
            current
                .as_ref()
                .map(|state| state.bridge_state.clone())
                .ok_or(ServiceError::InvalidSetup)?
        } else {
            OpenSubsonicBridgeState::new(backend_id, account_scope_id)
        };
    // Candidates are identity-coherent at revision zero; commit assigns the next shared store
    // revision after its optimistic check.
    bridge_state.set_revision(0);
    let mut store_set = OpenSubsonicStoreSet::new(profile, private_state, bridge_state)?;
    let client = OpenSubsonicClient::connect(&store_set.profile).await?;
    let capabilities =
        ServerCapabilities::probe(&client, store_set.private_state.credential()).await?;
    if store_set.private_state.credential_kind() == CredentialKind::ApiKey {
        if capabilities.supports(ServerFeature::ApiKeyAuthentication) {
            let username = client
                .api_key_username(store_set.private_state.credential())
                .await?;
            store_set
                .private_state
                .bind_api_key_username(username)
                .map_err(ServiceError::Store)?;
            if input.identity_intent == SetupIdentityIntent::UpdateSameServerAndAccount {
                require_same_account_owner(
                    current
                        .as_ref()
                        .ok_or(ServiceError::InvalidSetup)?
                        .private_state
                        .credential(),
                    store_set.private_state.credential(),
                )?;
            }
        } else if input.identity_intent == SetupIdentityIntent::UpdateSameServerAndAccount {
            // An unbound API key cannot prove that a replacement credential still belongs to the
            // same account. Preserve neither the account scope nor its linked-playlist authority.
            // The caller must choose ReplaceServerOrAccount instead.
            return Err(ServiceError::InvalidSetup);
        }
    }
    if input.identity_intent == SetupIdentityIntent::UpdateSameServerAndAccount {
        store_set
            .bridge_state
            .requeue_playlist_projections_needing_attention();
    }
    Ok(PreparedSetup {
        expected,
        store_set: Some(store_set),
        capabilities,
    })
}

fn require_same_account_owner(
    previous: &ServerCredential,
    candidate: &ServerCredential,
) -> Result<(), ServiceError> {
    let previous = previous
        .username()
        .ok_or(ServiceError::InvalidSetup)?
        .expose_secret();
    let candidate = candidate
        .username()
        .ok_or(ServiceError::InvalidSetup)?
        .expose_secret();
    if previous != candidate {
        return Err(ServiceError::InvalidSetup);
    }
    Ok(())
}

pub fn commit_setup(
    paths: &OpenSubsonicPaths,
    mut prepared: PreparedSetup,
) -> Result<OpenSubsonicStatus, ServiceError> {
    let mut store_set = prepared
        .store_set
        .take()
        .ok_or(ServiceError::InvalidSetup)?;
    // A durable profile/credential change is an immediate trust-boundary change. The owner may
    // keep the old guard while the replacement is only being tested, but once commit starts its
    // catalog handle and every previously minted route must already be unusable. In particular,
    // the store transaction can return an error after its commit marker is durable, so revoking
    // after a successful return would leave the old credential owner alive on that ambiguous
    // path.
    clear_current_runtime();
    commit_store_set(paths, prepared.expected, &mut store_set)?;
    Ok(status_from_store_set(&store_set))
}

pub fn read_status(paths: &OpenSubsonicPaths) -> Result<OpenSubsonicStatus, ServiceError> {
    match load_store_set_read_only(paths) {
        Ok(store_set) => Ok(store_set
            .as_ref()
            .map_or_else(OpenSubsonicStatus::off, status_from_store_set)),
        Err(StoreError::InvalidState | StoreError::PayloadTooLarge) => {
            Ok(OpenSubsonicStatus::needs_attention())
        }
        Err(error) => Err(error.into()),
    }
}

/// Verify the saved credential and exact-origin policy without changing durable state.
pub async fn test_connection(
    paths: &OpenSubsonicPaths,
) -> Result<OpenSubsonicStatus, ServiceError> {
    let Some(store_set) = load_store_set(paths)? else {
        return Ok(OpenSubsonicStatus::off());
    };
    probe_store_set(&store_set).await
}

/// Read-only connection probe for one-shot status commands.
///
/// Unlike [`test_connection`], this never creates the store directory, takes the mutation lock,
/// or rolls a pending transaction forward.
pub async fn test_connection_read_only(
    paths: &OpenSubsonicPaths,
) -> Result<OpenSubsonicStatus, ServiceError> {
    let Some(store_set) = load_store_set_read_only(paths)? else {
        return Ok(OpenSubsonicStatus::off());
    };
    probe_store_set(&store_set).await
}

async fn probe_store_set(
    store_set: &OpenSubsonicStoreSet,
) -> Result<OpenSubsonicStatus, ServiceError> {
    let client = OpenSubsonicClient::connect(&store_set.profile).await?;
    ServerCapabilities::probe(&client, store_set.private_state.credential()).await?;
    Ok(status_from_store_set(store_set))
}

pub fn remove_profile(paths: &OpenSubsonicPaths) -> Result<OpenSubsonicStatus, ServiceError> {
    clear_current_runtime();
    match load_store_set(paths) {
        Ok(Some(store_set)) => remove_store_set(paths, store_set.revisions())?,
        Ok(None) => {}
        Err(StoreError::InvalidState | StoreError::PayloadTooLarge) => reset_store_set(paths)?,
        Err(error) => return Err(error.into()),
    }
    Ok(OpenSubsonicStatus::off())
}

/// Disable the experimental native-history credential as one coherent private/bridge commit.
///
/// The caller must retire any live credential owner before invoking this mutation, then load a
/// fresh actor from the returned durable state. Standard OpenSubsonic credentials are retained.
pub fn disable_native_history(
    paths: &OpenSubsonicPaths,
) -> Result<OpenSubsonicStatus, ServiceError> {
    let Some(mut store_set) = load_store_set(paths)? else {
        return Err(ServiceError::InvalidSetup);
    };
    let expected = store_set.revisions();
    store_set.private_state.disable_native_history();
    store_set
        .bridge_state
        .set_native_history_health(NativeHistoryHealth::Off);
    commit_store_set(paths, expected, &mut store_set)?;
    Ok(status_from_store_set(&store_set))
}

fn status_from_store_set(store_set: &OpenSubsonicStoreSet) -> OpenSubsonicStatus {
    let native_history_enabled = store_set.private_state.native_history_enabled();
    let native_history_health = if native_history_enabled {
        match store_set.bridge_state.native_history_health() {
            NativeHistoryHealth::Off => NativeHistoryHealth::Probing,
            health => health,
        }
    } else {
        NativeHistoryHealth::Off
    };
    let outbound_scrobbles_needing_attention = store_set
        .bridge_state
        .outbound_scrobble_attention_ids()
        .len();
    let playlist_create_attention = playlist_create_attention_from_state(&store_set.bridge_state);
    let playlist_creates_needing_attention = playlist_create_attention.len();
    let playlist_links_needing_decision = store_set.bridge_state.playlist_links_needing_decision();
    let playlist_projections_needing_attention = store_set
        .bridge_state
        .playlist_projections_needing_attention();
    let playlist_contents_needing_attention =
        store_set.bridge_state.playlist_contents_needing_attention();
    OpenSubsonicStatus {
        kind: if outbound_scrobbles_needing_attention == 0
            && playlist_creates_needing_attention == 0
            && playlist_links_needing_decision == 0
            && playlist_projections_needing_attention == 0
            && playlist_contents_needing_attention == 0
        {
            OpenSubsonicStatusKind::UpToDate
        } else {
            OpenSubsonicStatusKind::NeedsAttention
        },
        display_name: Some(store_set.profile.display_name().to_owned()),
        backend_id: Some(store_set.profile.backend_id().clone()),
        account_scope_id: Some(store_set.profile.account_scope_id().clone()),
        credential_kind: Some(store_set.private_state.credential_kind()),
        uses_lan_http: store_set.profile.uses_lan_http(),
        uses_custom_ca: store_set.profile.custom_ca_fingerprint().is_some(),
        native_history_enabled,
        native_history_health,
        outbound_scrobbles_needing_attention,
        playlist_creates_needing_attention,
        playlist_create_attention,
        playlist_links_needing_decision,
        playlist_projections_needing_attention,
        playlist_contents_needing_attention,
    }
}

#[derive(Clone)]
pub struct OpenSubsonicHandle {
    tx: mpsc::Sender<ActorCommand>,
}

impl std::fmt::Debug for OpenSubsonicHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpenSubsonicHandle(<redacted>)")
    }
}

impl OpenSubsonicHandle {
    pub async fn search(
        &self,
        query: impl Into<String>,
        limit: u32,
    ) -> Result<Vec<ServerSong>, ServerError> {
        self.request(|reply| ActorCommand::Search {
            query: query.into(),
            limit,
            reply,
        })
        .await
    }

    pub async fn library_page(
        &self,
        request: ServerLibraryRequest,
    ) -> Result<ServerLibraryPage, ServerError> {
        self.request(|reply| ActorCommand::LibraryPage { request, reply })
            .await
    }

    pub async fn library_detail(
        &self,
        request: ServerLibraryDetailRequest,
    ) -> Result<ServerLibraryDetail, ServerError> {
        self.request(|reply| ActorCommand::LibraryDetail { request, reply })
            .await
    }

    pub async fn cover_art(
        &self,
        item: OpenSubsonicItemRef,
        id: CoverArtId,
    ) -> Result<Option<BinaryPayload>, ServerError> {
        self.request(|reply| ActorCommand::CoverArt { item, id, reply })
            .await
    }

    pub async fn profile_summary(&self) -> Result<OpenSubsonicProfileSummary, ServerError> {
        self.request(|reply| ActorCommand::ProfileSummary { reply })
            .await
    }

    /// Queue the current personal-state winners for canonical server projection.
    ///
    /// This never contains a credential and does not wait for the network. The credential owner
    /// persists each projection before attempting it and retries partial writes.
    pub fn reconcile_ratings(
        &self,
        winners: Vec<OpenSubsonicRatingWinner>,
    ) -> Result<OpenSubsonicRatingReceipt, ServerError> {
        let (reply, receipt) = oneshot::channel();
        self.try_send(ActorCommand::ReconcileRatings { winners, reply })?;
        Ok(receipt)
    }

    /// Confirm that the personal-state owner durably committed one bridge import.
    pub fn acknowledge_bridge_import(
        &self,
        operation_id: impl Into<String>,
    ) -> Result<(), ServerError> {
        self.try_send(ActorCommand::AcknowledgeBridgeImport {
            operation_id: operation_id.into(),
        })
    }

    /// Queue an exact OpenSubsonic playback action without exposing upstream authentication.
    ///
    /// The returned receipt resolves only after the credential owner has committed the report to
    /// its bridge store. Channel admission alone is not a durability acknowledgement.
    pub fn queue_scrobble(
        &self,
        event_id: String,
        kind: OpenSubsonicScrobbleKind,
        track: crate::scrobble::ScrobbleTrack,
    ) -> Result<OpenSubsonicScrobbleReceipt, ServerError> {
        let (reply, receipt) = oneshot::channel();
        self.try_send(ActorCommand::QueueScrobble {
            event_id,
            kind,
            track,
            reply,
        })?;
        Ok(receipt)
    }

    /// Confirm that the exact source journal durably entered its non-submitting acknowledgement
    /// state.
    ///
    /// This second receipt closes the cross-store crash window: the bridge retains its replay
    /// receipt until this command itself has crossed the bridge-store durability boundary.
    pub fn acknowledge_scrobble_source(
        &self,
        event_id: String,
        track: crate::scrobble::ScrobbleTrack,
    ) -> Result<OpenSubsonicScrobbleReceipt, ServerError> {
        let (reply, receipt) = oneshot::channel();
        self.try_send(ActorCommand::AcknowledgeScrobbleSource {
            event_id,
            track,
            reply,
        })?;
        Ok(receipt)
    }

    /// Return only opaque event IDs for reports requiring an explicit delivery decision.
    pub async fn scrobble_attention_ids(&self) -> Result<Vec<String>, ServerError> {
        self.request(|reply| ActorCommand::ScrobbleAttentionIds { reply })
            .await
    }

    /// Explicitly retry or mark one ambiguously delivered report as sent.
    ///
    /// The command accepts only its opaque event ID; track metadata, server addresses, and
    /// credentials never cross this API.
    pub fn resolve_scrobble(
        &self,
        event_id: String,
        resolution: OutboundScrobbleResolution,
    ) -> Result<OpenSubsonicScrobbleReceipt, ServerError> {
        let (reply, receipt) = oneshot::channel();
        self.try_send(ActorCommand::ResolveScrobble {
            event_id,
            resolution,
            reply,
        })?;
        Ok(receipt)
    }

    async fn open_stream_response(
        &self,
        item: OpenSubsonicItemRef,
        request: StreamRequest,
    ) -> Result<UpstreamStream, ServerError> {
        self.request(|reply| ActorCommand::OpenStream {
            item,
            request,
            reply,
        })
        .await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, ServerError>>) -> ActorCommand,
    ) -> Result<T, ServerError> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(command(reply))
            .await
            .map_err(|_| ServerError::Offline)?;
        response.await.map_err(|_| ServerError::Offline)?
    }

    fn try_send(&self, command: ActorCommand) -> Result<(), ServerError> {
        self.tx
            .try_send(command)
            .map_err(|_| ServerError::TemporarilyUnavailable)
    }
}

impl StreamSource for OpenSubsonicHandle {
    fn open_stream(&self, item: OpenSubsonicItemRef, request: StreamRequest) -> StreamSourceFuture {
        let handle = self.clone();
        Box::pin(async move {
            handle
                .open_stream_response(item, request)
                .await
                .map_err(proxy_error)
        })
    }
}

pub struct OpenSubsonicRuntime {
    generation: u64,
    handle: OpenSubsonicHandle,
    proxy_handle: OpenSubsonicProxyHandle,
    bridge_activation: Arc<BridgeActivation>,
    proxy_guard: Option<OpenSubsonicProxyGuard>,
    actor_task: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct BridgeActivation {
    active: AtomicBool,
    changed: Notify,
}

impl BridgeActivation {
    fn activate(&self) -> bool {
        if self.active.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.changed.notify_waiters();
        true
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

impl OpenSubsonicRuntime {
    pub fn handle(&self) -> OpenSubsonicHandle {
        self.handle.clone()
    }

    pub fn proxy_handle(&self) -> OpenSubsonicProxyHandle {
        self.proxy_handle.clone()
    }

    pub fn route_provider(&self) -> crate::playback_target::PlaybackRouteProviderHandle {
        self.proxy_handle.route_provider()
    }

    /// Publish this fully constructed runtime to read-only catalog consumers.
    ///
    /// Loading and activation are deliberately separate: multiple setup generations may finish
    /// out of order, and only the owner lane may decide which result is current. A discarded
    /// candidate therefore never replaces or revokes the active profile.
    pub(crate) fn activate(&self) {
        if !self.bridge_activation.activate() {
            return;
        }
        install_current(
            self.generation,
            self.handle.clone(),
            self.proxy_handle.clone(),
            self.actor_task
                .as_ref()
                .expect("an active runtime owns its actor task")
                .abort_handle(),
        );
    }

    pub async fn shutdown(mut self) {
        clear_current_generation(self.generation);
        if let Some(guard) = self.proxy_guard.take() {
            guard.shutdown().await;
        }
        if let Some(task) = self.actor_task.take() {
            task.abort();
        }
    }
}

impl Drop for OpenSubsonicRuntime {
    fn drop(&mut self) {
        clear_current_generation(self.generation);
        self.proxy_handle.revoke_all();
        if let Some(task) = self.actor_task.take() {
            task.abort();
        }
    }
}

pub async fn load_actor(
    paths: &OpenSubsonicPaths,
) -> Result<Option<OpenSubsonicRuntime>, ServiceError> {
    load_actor_with_bridge_sink(paths, None).await
}

/// Load the primary credential owner and deliver durable server observations to its state owner.
pub async fn load_actor_with_bridge_sink(
    paths: &OpenSubsonicPaths,
    sink: Option<OpenSubsonicBridgeSink>,
) -> Result<Option<OpenSubsonicRuntime>, ServiceError> {
    let Some(store_set) = load_store_set(paths)? else {
        return Ok(None);
    };
    start_actor(store_set, BridgeRuntime::writable(paths.clone(), sink))
        .await
        .map(Some)
}

/// Start a credential owner from a coherent snapshot without creating storage, locking for
/// mutation, or rolling a transaction forward. Read-only secondary processes use this path.
pub async fn load_actor_read_only(
    paths: &OpenSubsonicPaths,
) -> Result<Option<OpenSubsonicRuntime>, ServiceError> {
    let Some(store_set) = load_store_set_read_only(paths)? else {
        return Ok(None);
    };
    start_actor(store_set, BridgeRuntime::read_only())
        .await
        .map(Some)
}

async fn start_actor(
    store_set: OpenSubsonicStoreSet,
    bridge: BridgeRuntime,
) -> Result<OpenSubsonicRuntime, ServiceError> {
    let client = OpenSubsonicClient::connect(&store_set.profile).await?;
    let capabilities =
        ServerCapabilities::probe(&client, store_set.private_state.credential()).await?;
    let summary = OpenSubsonicProfileSummary {
        display_name: store_set.profile.display_name().to_owned(),
        backend_id: store_set.profile.backend_id().clone(),
        account_scope_id: store_set.profile.account_scope_id().clone(),
        credential_kind: store_set.private_state.credential_kind(),
        uses_lan_http: store_set.profile.uses_lan_http(),
        capabilities,
    };
    let (tx, rx) = mpsc::channel(ACTOR_CAPACITY);
    let handle = OpenSubsonicHandle { tx };
    let bridge_activation = Arc::new(BridgeActivation::default());
    let actor_task = tokio::spawn(run_actor(
        rx,
        store_set,
        client,
        summary,
        bridge,
        bridge_activation.clone(),
    ));
    let (proxy_handle, proxy_guard) = match super::proxy::start(Arc::new(handle.clone())).await {
        Ok(proxy) => proxy,
        Err(_) => {
            actor_task.abort();
            return Err(ServiceError::ProxyUnavailable);
        }
    };
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(OpenSubsonicRuntime {
        generation,
        handle,
        proxy_handle,
        bridge_activation,
        proxy_guard: Some(proxy_guard),
        actor_task: Some(actor_task),
    })
}

pub fn current_handle() -> Option<OpenSubsonicHandle> {
    current()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|entry| entry.handle.clone())
}

pub fn current_proxy_handle() -> Option<OpenSubsonicProxyHandle> {
    current()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|entry| entry.proxy.clone())
}

async fn run_actor(
    mut rx: mpsc::Receiver<ActorCommand>,
    mut store_set: OpenSubsonicStoreSet,
    client: OpenSubsonicClient,
    summary: OpenSubsonicProfileSummary,
    bridge: BridgeRuntime,
    bridge_activation: Arc<BridgeActivation>,
) {
    let now = tokio::time::Instant::now();
    let mut retry = tokio::time::interval_at(
        now + std::time::Duration::from_secs(2),
        BRIDGE_RETRY_INTERVAL,
    );
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut native_history = tokio::time::interval_at(
        now + std::time::Duration::from_secs(5),
        NATIVE_HISTORY_INTERVAL,
    );
    native_history.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut history_jobs = JoinSet::new();
    let mut bridge_active = false;
    let mut last_native_history_error = None;
    let mut native_history_was_truncated = false;
    let mut playlists = ActorPlaylistState::default();

    loop {
        if let Err(error) = activate_bridge_if_needed(
            &bridge,
            &bridge_activation,
            &mut bridge_active,
            &mut store_set,
        ) {
            tracing::warn!(
                reason = %error,
                "music server candidate could not rebase its durable bridge snapshot"
            );
            break;
        }
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                // Activation and the first owner command can become ready on the same scheduler
                // turn. Rebase in this branch too so that command cannot mutate a stale revision.
                if let Err(error) = activate_bridge_if_needed(
                    &bridge,
                    &bridge_activation,
                    &mut bridge_active,
                    &mut store_set,
                ) {
                    tracing::warn!(
                        reason = %error,
                        "music server candidate could not rebase its durable bridge snapshot"
                    );
                    break;
                }
                handle_actor_command(
                    command,
                    &mut store_set,
                    &client,
                    &summary,
                    &bridge,
                    bridge.is_writable() && bridge_active,
                    &mut playlists,
                )
                .await;
            }
            _ = bridge_activation.changed.notified(), if !bridge_active => {}
            _ = retry.tick(), if bridge.is_writable() && bridge_active => {
                if let Err(error) = bridge.retry_network(&mut store_set, &client).await {
                    tracing::warn!(reason = %error, "music server bridge will retry");
                }
            }
            _ = native_history.tick(),
                if bridge.is_writable() && bridge_active && history_jobs.is_empty() =>
            {
                if let Some(worker) = bridge.history_worker() {
                    history_jobs.spawn(worker.fetch());
                }
            }
            completed = history_jobs.join_next(), if !history_jobs.is_empty() => {
                let mut retry_soon = false;
                match completed {
                    Some(Ok(Ok(result))) => match bridge.apply_history_refresh(
                        &mut store_set,
                        result,
                    ) {
                        Ok(outcome) => {
                            let health = native_history_health_after(
                                store_set.private_state.native_history_enabled(),
                                outcome.native_error,
                            );
                            if let Err(error) =
                                bridge.set_native_history_health(&mut store_set, health)
                            {
                                retry_soon = true;
                                tracing::warn!(
                                    reason = %error,
                                    "detailed music server history status could not be persisted"
                                );
                            }
                            if let Some(error) = outcome.native_error {
                                retry_soon |= native_history_error_retries_soon(error);
                                if last_native_history_error != Some(error) {
                                    tracing::warn!(
                                        reason = %error,
                                        "detailed music server history is unavailable; standard history remains active"
                                    );
                                }
                            } else if last_native_history_error.take().is_some() {
                                tracing::info!(
                                    "detailed music server history is available again"
                                );
                            }
                            last_native_history_error = outcome.native_error;
                            if let Some(error) = outcome.standard_error {
                                retry_soon = true;
                                tracing::warn!(
                                    reason = %error,
                                    "music server play-count history is temporarily unavailable"
                                );
                            }
                            if outcome.native_stale {
                                retry_soon = true;
                                tracing::debug!(
                                    "stale detailed-history result was discarded and will retry"
                                );
                            }
                            if outcome.native_truncated {
                                retry_soon = true;
                                if !native_history_was_truncated {
                                    tracing::warn!(
                                        "detailed music server history reached its bounded scan limit; continuing from its durable cursor"
                                    );
                                }
                            }
                            native_history_was_truncated = outcome.native_truncated;
                        }
                        Err(error) => {
                            retry_soon = true;
                            let health = if store_set.private_state.native_history_enabled() {
                                NativeHistoryHealth::Probing
                            } else {
                                NativeHistoryHealth::Off
                            };
                            if let Err(status_error) =
                                bridge.set_native_history_health(&mut store_set, health)
                            {
                                tracing::warn!(
                                    reason = %status_error,
                                    "detailed music server history status could not be persisted"
                                );
                            }
                            tracing::warn!(
                                reason = %error,
                                "music server history could not be persisted"
                            );
                        }
                    },
                    Some(Ok(Err(error))) => {
                        retry_soon = true;
                        if store_set.private_state.native_history_enabled()
                            && let Err(status_error) = bridge.set_native_history_health(
                                &mut store_set,
                                NativeHistoryHealth::Probing,
                            )
                        {
                            tracing::warn!(
                                reason = %status_error,
                                "detailed music server history status could not be persisted"
                            );
                        }
                        tracing::warn!(
                            reason = %error,
                            "music server history worker is temporarily unavailable"
                        );
                    }
                    Some(Err(error)) => {
                        retry_soon = true;
                        if store_set.private_state.native_history_enabled()
                            && let Err(status_error) = bridge.set_native_history_health(
                                &mut store_set,
                                NativeHistoryHealth::Probing,
                            )
                        {
                            tracing::warn!(
                                reason = %status_error,
                                "detailed music server history status could not be persisted"
                            );
                        }
                        tracing::warn!(
                            cancelled = error.is_cancelled(),
                            "music server history worker stopped unexpectedly"
                        );
                    }
                    None => {}
                }
                if retry_soon {
                    native_history.reset_after(HISTORY_RETRY_INTERVAL);
                }
            }
        }
    }
}

fn activate_bridge_if_needed(
    bridge: &BridgeRuntime,
    activation: &BridgeActivation,
    bridge_active: &mut bool,
    store_set: &mut OpenSubsonicStoreSet,
) -> Result<(), ServiceError> {
    if !*bridge_active && activation.is_active() {
        bridge.refresh_snapshot_for_activation(store_set)?;
        *bridge_active = true;
        bridge.emit_pending(store_set);
    }
    Ok(())
}

fn native_history_error_retries_soon(error: super::native_history::NativeHistoryError) -> bool {
    matches!(
        error,
        super::native_history::NativeHistoryError::Offline
            | super::native_history::NativeHistoryError::TemporarilyUnavailable
    )
}

fn native_history_health_after(
    enabled: bool,
    error: Option<super::native_history::NativeHistoryError>,
) -> NativeHistoryHealth {
    use super::native_history::NativeHistoryError;

    if !enabled {
        return NativeHistoryHealth::Off;
    }
    match error {
        None => NativeHistoryHealth::Detailed,
        Some(
            NativeHistoryError::InvalidCredential
            | NativeHistoryError::AuthenticationRequired
            | NativeHistoryError::PermissionDenied,
        ) => NativeHistoryHealth::UpdatePassword,
        Some(NativeHistoryError::UnsupportedFeature) => NativeHistoryHealth::PlayCountsOnly,
        Some(_) => NativeHistoryHealth::Probing,
    }
}

async fn handle_actor_command(
    command: ActorCommand,
    store_set: &mut OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
    summary: &OpenSubsonicProfileSummary,
    bridge: &BridgeRuntime,
    playlist_mutations_allowed: bool,
    playlists: &mut ActorPlaylistState,
) {
    match command {
        ActorCommand::Search {
            query,
            limit,
            mut reply,
        } => {
            let catalog = catalog(store_set, client);
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = catalog.search(&query, limit) => result,
            };
            if let Ok(songs) = &result
                && let Err(error) = bridge.observe_songs(store_set, songs)
            {
                tracing::warn!(reason = %error, "music server observations were not persisted");
            }
            let _ = reply.send(result);
        }
        ActorCommand::LibraryPage { request, mut reply } => {
            let catalog = catalog(store_set, client);
            let page_plan = match playlists
                .catalog_session
                .start_page(request, &store_set.bridge_state)
            {
                Ok(plan) => plan,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let (catalog_offset, catalog_limit) = page_plan.remote_window();
            let mut result = tokio::select! {
                _ = reply.closed() => return,
                result = catalog.library_page(
                    request.section,
                    catalog_offset,
                    catalog_limit,
                ) => result,
            };
            if let Ok(page) = &result
                && let Err(error) = bridge.observe_page(store_set, page)
            {
                tracing::warn!(reason = %error, "music server observations were not persisted");
            }
            if let Ok(page) = &mut result {
                finalize_page_playlist_access(
                    page,
                    store_set.private_state.credential(),
                    &store_set.bridge_state,
                );
            }
            let finish_result = match &mut result {
                Ok(page) => playlists.catalog_session.finish_page(
                    page,
                    &store_set.bridge_state,
                    request,
                    &page_plan,
                ),
                Err(_) => Ok(()),
            };
            if let Err(error) = finish_result {
                result = Err(error);
            }
            let _ = reply.send(result);
        }
        ActorCommand::LibraryDetail { request, mut reply } => {
            let catalog = catalog(store_set, client);
            let operation = async {
                match request {
                    ServerLibraryDetailRequest::Album(id) => catalog.album_detail(&id).await,
                    ServerLibraryDetailRequest::Artist(id) => catalog.artist_detail(&id).await,
                    ServerLibraryDetailRequest::Playlist(id) => catalog.playlist_detail(&id).await,
                }
            };
            let mut result = tokio::select! {
                _ = reply.closed() => return,
                result = operation => result,
            };
            if let Ok(detail) = &result
                && let Err(error) = bridge.observe_detail(store_set, detail)
            {
                tracing::warn!(reason = %error, "music server observations were not persisted");
            }
            if let Ok(detail) = &mut result {
                finalize_detail_playlist_access(
                    detail,
                    store_set.private_state.credential(),
                    &store_set.bridge_state,
                );
            }
            let _ = reply.send(result);
        }
        ActorCommand::CoverArt {
            item,
            id,
            mut reply,
        } => {
            let catalog = catalog(store_set, client);
            let operation = async {
                match client.validate_item_scope(&item) {
                    Ok(()) => catalog.cover_art(&id).await,
                    Err(error) => Err(error),
                }
            };
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = operation => result,
            };
            let _ = reply.send(result);
        }
        ActorCommand::ProfileSummary { reply } => {
            let _ = reply.send(Ok(summary.clone()));
        }
        ActorCommand::OpenStream {
            item,
            request,
            mut reply,
        } => {
            let operation = async {
                let origin = client.proxy_origin()?;
                let response = client
                    .open_stream(store_set.private_state.credential(), &item, request)
                    .await?;
                Ok(UpstreamStream::new(response, origin))
            };
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = operation => result,
            };
            let _ = reply.send(result);
        }
        ActorCommand::ReconcileRatings { winners, reply } => {
            let result = bridge.reconcile_ratings(store_set, winners);
            let failed = result.err();
            let _ = reply.send(failed.map_or(Ok(()), Err));
            if let Some(error) = failed {
                tracing::warn!(reason = %error, "music server ratings will retry");
            }
        }
        ActorCommand::Playlist(command) => {
            let Some(command) = playlist_ownership::authorize(command, playlist_mutations_allowed)
            else {
                return;
            };
            playlists::handle_command(command, &mut playlists.previews, store_set, client, bridge)
                .await;
        }
        ActorCommand::AcknowledgeBridgeImport { operation_id } => {
            if let Err(error) = bridge.acknowledge_import(store_set, &operation_id) {
                tracing::warn!(reason = %error, "music server import acknowledgement will retry");
            }
        }
        ActorCommand::QueueScrobble {
            event_id,
            kind,
            track,
            reply,
        } => {
            let result = bridge.queue_scrobble(store_set, &event_id, kind, track);
            let failed = result.err();
            let _ = reply.send(failed.map_or(Ok(()), Err));
            if let Some(error) = failed {
                tracing::warn!(reason = %error, "music server playback report will retry");
            }
        }
        ActorCommand::AcknowledgeScrobbleSource {
            event_id,
            track,
            reply,
        } => {
            let result = bridge.acknowledge_scrobble_source(store_set, &event_id, track);
            let failed = result.err();
            let _ = reply.send(failed.map_or(Ok(()), Err));
            if let Some(error) = failed {
                tracing::warn!(
                    reason = %error,
                    "music server source acknowledgement will retry"
                );
            }
        }
        ActorCommand::ScrobbleAttentionIds { reply } => {
            let _ = reply.send(Ok(bridge.outbound_scrobble_attention_ids(store_set)));
        }
        ActorCommand::ResolveScrobble {
            event_id,
            resolution,
            reply,
        } => {
            let result = bridge.resolve_outbound_scrobble(store_set, &event_id, resolution);
            let failed = result.err();
            let _ = reply.send(failed.map_or(Ok(()), Err));
            if let Some(error) = failed {
                tracing::warn!(
                    reason = %error,
                    "music server playback report decision was not persisted"
                );
            }
        }
    }
}

fn catalog<'a>(
    store_set: &'a OpenSubsonicStoreSet,
    client: &'a OpenSubsonicClient,
) -> OpenSubsonicCatalog<'a> {
    OpenSubsonicCatalog::new(
        client,
        store_set.private_state.credential(),
        store_set.profile.backend_id(),
        store_set.profile.account_scope_id(),
    )
}

enum ActorCommand {
    Search {
        query: String,
        limit: u32,
        reply: oneshot::Sender<Result<Vec<ServerSong>, ServerError>>,
    },
    LibraryPage {
        request: ServerLibraryRequest,
        reply: oneshot::Sender<Result<ServerLibraryPage, ServerError>>,
    },
    LibraryDetail {
        request: ServerLibraryDetailRequest,
        reply: oneshot::Sender<Result<ServerLibraryDetail, ServerError>>,
    },
    CoverArt {
        item: OpenSubsonicItemRef,
        id: CoverArtId,
        reply: oneshot::Sender<Result<Option<BinaryPayload>, ServerError>>,
    },
    ProfileSummary {
        reply: oneshot::Sender<Result<OpenSubsonicProfileSummary, ServerError>>,
    },
    OpenStream {
        item: OpenSubsonicItemRef,
        request: StreamRequest,
        reply: oneshot::Sender<Result<UpstreamStream, ServerError>>,
    },
    ReconcileRatings {
        winners: Vec<OpenSubsonicRatingWinner>,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    Playlist(playlists::PlaylistActorCommand),
    AcknowledgeBridgeImport {
        operation_id: String,
    },
    QueueScrobble {
        event_id: String,
        kind: OpenSubsonicScrobbleKind,
        track: crate::scrobble::ScrobbleTrack,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    AcknowledgeScrobbleSource {
        event_id: String,
        track: crate::scrobble::ScrobbleTrack,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    ScrobbleAttentionIds {
        reply: oneshot::Sender<Result<Vec<String>, ServerError>>,
    },
    ResolveScrobble {
        event_id: String,
        resolution: OutboundScrobbleResolution,
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
}

/// Correlated proof that an outbound playback report crossed the bridge-store fsync boundary.
pub type OpenSubsonicScrobbleReceipt = oneshot::Receiver<Result<(), ServiceError>>;

/// Correlated proof that a rating projection crossed the bridge-store fsync boundary.
pub type OpenSubsonicRatingReceipt = oneshot::Receiver<Result<(), ServiceError>>;

/// Correlated proof that linked playlist projection crossed the bridge-store fsync boundary.
pub type OpenSubsonicPlaylistReceipt = oneshot::Receiver<Result<(), ServiceError>>;

fn proxy_error(error: ServerError) -> ProxyUpstreamError {
    let reason = match error {
        ServerError::AuthenticationRequired => "upstream_authentication_required",
        ServerError::PermissionDenied => "upstream_permission_denied",
        ServerError::WrongAccountScope => "upstream_scope_mismatch",
        ServerError::OriginRejected | ServerError::CertificateFailed => "upstream_origin_rejected",
        ServerError::NotFound => "upstream_not_found",
        ServerError::RateLimited(_) => "upstream_rate_limited",
        ServerError::UnsupportedFeature => "upstream_unsupported",
        ServerError::InvalidResponse | ServerError::ResponseTooLarge => "upstream_invalid_response",
        ServerError::Offline | ServerError::TemporarilyUnavailable => "upstream_unavailable",
    };
    ProxyUpstreamError::new(reason)
}

struct CurrentEntry {
    generation: u64,
    handle: OpenSubsonicHandle,
    proxy: OpenSubsonicProxyHandle,
    actor_abort: tokio::task::AbortHandle,
}

impl CurrentEntry {
    fn revoke(self) {
        self.proxy.revoke_all();
        self.actor_abort.abort();
    }
}

static CURRENT: OnceLock<RwLock<Option<CurrentEntry>>> = OnceLock::new();
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn current() -> &'static RwLock<Option<CurrentEntry>> {
    CURRENT.get_or_init(|| RwLock::new(None))
}

fn install_current(
    generation: u64,
    handle: OpenSubsonicHandle,
    proxy: OpenSubsonicProxyHandle,
    actor_abort: tokio::task::AbortHandle,
) {
    let mut current = current()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(previous) = current.replace(CurrentEntry {
        generation,
        handle,
        proxy,
        actor_abort,
    }) {
        previous.revoke();
    }
}

fn clear_current_runtime() {
    let mut current = current()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(previous) = current.take() {
        previous.revoke();
    }
}

fn clear_current_generation(generation: u64) {
    let mut current = current()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current
        .as_ref()
        .is_some_and(|entry| entry.generation == generation)
        && let Some(entry) = current.take()
    {
        entry.revoke();
    }
}

#[cfg(test)]
mod tests;
