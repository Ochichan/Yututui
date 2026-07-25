//! Long-lived credential owner plus setup/status lifecycle facade.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use zeroize::Zeroize;

use super::OpenSubsonicBridgeState;
use super::capabilities::ServerCapabilities;
use super::catalog::OpenSubsonicCatalog;
use super::client::{BinaryPayload, OpenSubsonicClient, ServerError};
use super::model::{
    AccountScopeId, AlbumId, ArtistId, BackendId, CoverArtId, OpenSubsonicItemRef,
    ServerLibraryDetail, ServerLibraryPage, ServerLibrarySection, ServerPlaylistId, ServerSong,
};
use super::origin::ConfiguredPrivateOrigin;
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

const ACTOR_CAPACITY: usize = 32;

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
    let private_state =
        OpenSubsonicPrivateState::new(backend_id.clone(), account_scope_id.clone(), credential);
    let bridge_state = OpenSubsonicBridgeState::new(backend_id, account_scope_id);
    let store_set = OpenSubsonicStoreSet::new(profile, private_state, bridge_state)?;
    let client = OpenSubsonicClient::connect(&store_set.profile).await?;
    let capabilities =
        ServerCapabilities::probe(&client, store_set.private_state.credential()).await?;
    Ok(PreparedSetup {
        expected,
        store_set: Some(store_set),
        capabilities,
    })
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

fn status_from_store_set(store_set: &OpenSubsonicStoreSet) -> OpenSubsonicStatus {
    OpenSubsonicStatus {
        kind: OpenSubsonicStatusKind::UpToDate,
        display_name: Some(store_set.profile.display_name().to_owned()),
        backend_id: Some(store_set.profile.backend_id().clone()),
        account_scope_id: Some(store_set.profile.account_scope_id().clone()),
        credential_kind: Some(store_set.private_state.credential_kind()),
        uses_lan_http: store_set.profile.uses_lan_http(),
        uses_custom_ca: store_set.profile.custom_ca_fingerprint().is_some(),
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
    proxy_guard: Option<OpenSubsonicProxyGuard>,
    actor_task: Option<JoinHandle<()>>,
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
    let Some(store_set) = load_store_set(paths)? else {
        return Ok(None);
    };
    start_actor(store_set).await.map(Some)
}

/// Start a credential owner from a coherent snapshot without creating storage, locking for
/// mutation, or rolling a transaction forward. Read-only secondary processes use this path.
pub async fn load_actor_read_only(
    paths: &OpenSubsonicPaths,
) -> Result<Option<OpenSubsonicRuntime>, ServiceError> {
    let Some(store_set) = load_store_set_read_only(paths)? else {
        return Ok(None);
    };
    start_actor(store_set).await.map(Some)
}

async fn start_actor(store_set: OpenSubsonicStoreSet) -> Result<OpenSubsonicRuntime, ServiceError> {
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
    let actor_task = tokio::spawn(run_actor(rx, store_set, client, summary));
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
    store_set: OpenSubsonicStoreSet,
    client: OpenSubsonicClient,
    summary: OpenSubsonicProfileSummary,
) {
    while let Some(command) = rx.recv().await {
        let catalog = OpenSubsonicCatalog::new(
            &client,
            store_set.private_state.credential(),
            store_set.profile.backend_id(),
            store_set.profile.account_scope_id(),
        );
        match command {
            ActorCommand::Search {
                query,
                limit,
                mut reply,
            } => {
                let result = tokio::select! {
                    _ = reply.closed() => continue,
                    result = catalog.search(&query, limit) => result,
                };
                let _ = reply.send(result);
            }
            ActorCommand::LibraryPage { request, mut reply } => {
                let result = tokio::select! {
                    _ = reply.closed() => continue,
                    result = catalog.library_page(
                        request.section,
                        request.offset,
                        request.limit,
                    ) => result,
                };
                let _ = reply.send(result);
            }
            ActorCommand::LibraryDetail { request, mut reply } => {
                let operation = async {
                    match request {
                        ServerLibraryDetailRequest::Album(id) => catalog.album_detail(&id).await,
                        ServerLibraryDetailRequest::Artist(id) => catalog.artist_detail(&id).await,
                        ServerLibraryDetailRequest::Playlist(id) => {
                            catalog.playlist_detail(&id).await
                        }
                    }
                };
                let result = tokio::select! {
                    _ = reply.closed() => continue,
                    result = operation => result,
                };
                let _ = reply.send(result);
            }
            ActorCommand::CoverArt {
                item,
                id,
                mut reply,
            } => {
                let operation = async {
                    match client.validate_item_scope(&item) {
                        Ok(()) => catalog.cover_art(&id).await,
                        Err(error) => Err(error),
                    }
                };
                let result = tokio::select! {
                    _ = reply.closed() => continue,
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
                    _ = reply.closed() => continue,
                    result = operation => result,
                };
                let _ = reply.send(result);
            }
        }
    }
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
}

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
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use age::secrecy::SecretString;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn status_never_contains_origin_or_credentials() {
        let status = OpenSubsonicStatus {
            kind: OpenSubsonicStatusKind::UpToDate,
            display_name: Some("Server".to_owned()),
            backend_id: Some(BackendId::new("backend").unwrap()),
            account_scope_id: Some(AccountScopeId::new("account").unwrap()),
            credential_kind: Some(CredentialKind::ApiKey),
            uses_lan_http: false,
            uses_custom_ca: false,
        };
        let rendered = format!("{status:?}");
        assert!(!rendered.contains("https://"));
        assert!(!rendered.contains("sentinel-secret"));
    }

    #[test]
    fn confirmed_remove_resets_corrupt_partial_and_oversized_stores() {
        for (case, bytes) in [
            ("corrupt", b"not-json".to_vec()),
            (
                "oversized",
                vec![b'x'; crate::open_subsonic::profile::MAX_PROFILE_BYTES as usize + 1],
            ),
        ] {
            let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "yututui-open-subsonic-reset-{case}-{}-{id}",
                std::process::id()
            ));
            let paths = OpenSubsonicPaths::for_data_root(root.clone());
            crate::util::safe_fs::write_owner_only_atomic(paths.profile(), &bytes).unwrap();

            assert_eq!(
                read_status(&paths).unwrap().kind,
                OpenSubsonicStatusKind::NeedsAttention
            );
            assert_eq!(
                remove_profile(&paths).unwrap().kind,
                OpenSubsonicStatusKind::Off
            );
            assert_eq!(
                read_status(&paths).unwrap().kind,
                OpenSubsonicStatusKind::Off
            );
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_remove_rejects_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-open-subsonic-reset-link-{}-{id}",
            std::process::id()
        ));
        let paths = OpenSubsonicPaths::for_data_root(root.clone());
        crate::util::safe_fs::ensure_private_dir(paths.root()).unwrap();
        let external = root.join("outside-secret");
        std::fs::write(&external, b"must-stay").unwrap();
        symlink(&external, paths.profile()).unwrap();

        assert!(remove_profile(&paths).is_err());
        assert_eq!(std::fs::read(&external).unwrap(), b"must-stay");
        assert!(paths.profile().is_symlink());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setup_input_is_move_only_and_accepts_secret_credential() {
        let input = SetupInput::new(
            "Server",
            "https://music.example.test/",
            false,
            None,
            ServerCredential::api_key(SecretString::from("secret".to_owned())).unwrap(),
            SetupIdentityIntent::Create,
        );
        assert_eq!(input.identity_intent, SetupIdentityIntent::Create);
    }

    #[tokio::test]
    async fn tested_setup_commits_all_stores_and_remove_is_local_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let replies = [
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            ];
            for body in replies.into_iter().cycle().take(12) {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while request.len() < 16 * 1024 {
                    if stream.read(&mut byte).await.unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-open-subsonic-service-{}-{id}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&root).unwrap();
        let paths = OpenSubsonicPaths::for_data_root(root.clone());
        let input = SetupInput::new(
            "Test server",
            format!("http://127.0.0.1:{port}/"),
            true,
            None,
            ServerCredential::api_key(SecretString::from("secret".to_owned())).unwrap(),
            SetupIdentityIntent::Create,
        );
        let prepared = test_and_prepare_setup(&paths, input).await.unwrap();
        let status = commit_setup(&paths, prepared).unwrap();
        assert_eq!(status.kind, OpenSubsonicStatusKind::UpToDate);
        let original_backend = status.backend_id.clone().unwrap();
        let original_account = status.account_scope_id.clone().unwrap();
        assert_eq!(read_status(&paths).unwrap().kind, status.kind);
        assert_eq!(
            test_connection(&paths).await.unwrap().kind,
            OpenSubsonicStatusKind::UpToDate
        );
        let old_runtime = load_actor(&paths).await.unwrap().unwrap();
        let old_handle = old_runtime.handle();
        let old_provider = old_runtime.route_provider();
        old_runtime.activate();
        let old_target = crate::playback_target::CredentialedPlaybackRef::OpenSubsonic {
            backend_id: original_backend.as_str().to_owned(),
            account_scope_id: original_account.as_str().to_owned(),
            item_id: "old-route-item".to_owned(),
        };
        let old_route = old_provider
            .open_route(old_target.clone(), 1)
            .await
            .unwrap();
        let (old_route_url, _old_route_lease) = old_route.into_parts();
        let old_route_url = old_route_url.into_string();

        let update = SetupInput::new(
            "Renamed server",
            format!("http://127.0.0.1:{port}/"),
            true,
            None,
            ServerCredential::api_key(SecretString::from("updated-secret".to_owned())).unwrap(),
            SetupIdentityIntent::UpdateSameServerAndAccount,
        );
        crate::open_subsonic::transaction::fail_after_commit_marker_once_for_test();
        assert_eq!(
            commit_setup(
                &paths,
                test_and_prepare_setup(&paths, update).await.unwrap(),
            ),
            Err(ServiceError::Store(StoreError::StorageUnavailable))
        );
        assert_eq!(
            old_handle.profile_summary().await.unwrap_err(),
            ServerError::Offline
        );
        assert_eq!(
            old_provider
                .open_route(old_target, 2)
                .await
                .unwrap_err()
                .reason(),
            "route_provider_unavailable"
        );
        assert_eq!(
            reqwest::get(old_route_url).await.unwrap().status(),
            reqwest::StatusCode::NOT_FOUND
        );

        // The next owner load rolls the committed candidate forward, but never revives the old
        // handle or route.
        load_store_set(&paths).unwrap().unwrap();
        let updated = read_status(&paths).unwrap();
        assert_eq!(updated.kind, OpenSubsonicStatusKind::UpToDate);
        assert_eq!(updated.display_name.as_deref(), Some("Renamed server"));
        assert_eq!(updated.backend_id.as_ref(), Some(&original_backend));
        assert_eq!(updated.account_scope_id.as_ref(), Some(&original_account));
        assert_eq!(
            old_handle.profile_summary().await.unwrap_err(),
            ServerError::Offline
        );
        drop(old_runtime);

        let replacement = SetupInput::new(
            "Replacement server",
            format!("http://127.0.0.1:{port}/"),
            true,
            None,
            ServerCredential::api_key(SecretString::from("replacement-secret".to_owned())).unwrap(),
            SetupIdentityIntent::ReplaceServerOrAccount,
        );
        let replaced = commit_setup(
            &paths,
            test_and_prepare_setup(&paths, replacement).await.unwrap(),
        )
        .unwrap();
        assert_ne!(replaced.backend_id.as_ref(), Some(&original_backend));
        assert_ne!(replaced.account_scope_id.as_ref(), Some(&original_account));
        let removal_runtime = load_actor(&paths).await.unwrap().unwrap();
        let removal_handle = removal_runtime.handle();
        removal_runtime.activate();
        assert_eq!(
            remove_profile(&paths).unwrap().kind,
            OpenSubsonicStatusKind::Off
        );
        assert_eq!(
            removal_handle.profile_summary().await.unwrap_err(),
            ServerError::Offline
        );
        drop(removal_runtime);
        assert_eq!(
            read_status(&paths).unwrap().kind,
            OpenSubsonicStatusKind::Off
        );
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
