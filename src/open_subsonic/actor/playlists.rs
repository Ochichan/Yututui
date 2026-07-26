//! Preview-only preparation for explicit server-playlist imports and links.
//!
//! Nothing in this module persists or performs a mutation. The credential-owning actor first
//! fetches a strict full remote snapshot, prepares one bounded entry-occurrence plan here, shows
//! only the redacted counts to the App, then re-fetches and compares the fingerprint before
//! committing the prepared bridge-store records.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use age::secrecy::ExposeSecret as _;
use data_encoding::HEXLOWER;
use sha2::{Digest as _, Sha256};

use crate::personal_state::{
    ExternalOperationInput, Operation, PersonalPlaylistSnapshot, PlaylistEntryId, PlaylistId,
    PortableTrack, PortableTrackKey,
};

use super::super::OpenSubsonicClient;
use super::super::ServerError;
use super::super::bridge_event::portable_server_track;
use super::super::bridge_runtime::BridgeRuntime;
use super::super::bridge_store::{
    PendingPlaylistCreate, PendingPlaylistImportBatch, PendingPlaylistImportPurpose,
    PendingPlaylistProjection, PendingPlaylistProjectionStage, PlaylistLink, PlaylistLinkState,
    PlaylistShadow, PlaylistShadowOccurrence,
};
use super::super::client::MutationDeliveryError;
use super::super::linked_playlists::{
    InitialMergeOccurrence, LinkedPlaylistEntry, plan_initial_merge,
};
use super::super::model::{
    AccountScopeId, BackendId, ItemId, ServerPlaylistId, ServerPlaylistWriteSnapshot,
};
use super::super::transaction::OpenSubsonicStoreSet;
use super::{ActorCommand, OpenSubsonicHandle, OpenSubsonicPlaylistReceipt};

const MAX_PREVIEWS: usize = 32;
const PREVIEW_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistPreviewMode {
    ImportCopy,
    LinkNew,
    LinkExisting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistPreviewTarget {
    ImportCopy,
    LinkNew,
    LinkExisting(PersonalPlaylistSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistMergePreview {
    pub preview_id: String,
    pub mode: PlaylistPreviewMode,
    pub server_playlist_id: ServerPlaylistId,
    pub local_playlist_id: PlaylistId,
    pub name: String,
    pub remote_tracks: usize,
    pub add_to_local: usize,
    pub add_to_server: usize,
}

impl OpenSubsonicHandle {
    /// Prepare a bounded, deletion-free preview without changing either side.
    pub async fn prepare_playlist(
        &self,
        server_playlist_id: ServerPlaylistId,
        target: PlaylistPreviewTarget,
    ) -> Result<PlaylistMergePreview, ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::Prepare {
                server_playlist_id,
                target,
                reply,
            })
        })
        .await
    }

    /// Apply one single-use preview after the actor revalidates both snapshots.
    pub async fn apply_playlist_preview(
        &self,
        preview_id: String,
        server_playlist_id: ServerPlaylistId,
        current_local: Option<PersonalPlaylistSnapshot>,
    ) -> Result<PlaylistId, ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::ApplyPreview {
                preview_id,
                server_playlist_id,
                current_local,
                reply,
            })
        })
        .await
    }

    /// Queue current local playlist winners for durable linked-server projection.
    pub fn reconcile_playlists(
        &self,
        snapshots: Vec<PersonalPlaylistSnapshot>,
    ) -> Result<OpenSubsonicPlaylistReceipt, ServerError> {
        let (reply, receipt) = tokio::sync::oneshot::channel();
        self.try_send(ActorCommand::Playlist(PlaylistActorCommand::Reconcile {
            snapshots,
            reply,
        }))?;
        Ok(receipt)
    }

    /// Create a server-owned copy of one exact local playlist and link it after readback.
    pub async fn create_linked_playlist(
        &self,
        snapshot: PersonalPlaylistSnapshot,
    ) -> Result<ServerPlaylistId, ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::CreateLinked {
                snapshot,
                replace_missing: false,
                expected_missing_server_id: None,
                reply,
            })
        })
        .await
    }

    /// Re-create the missing server side of an existing durable link.
    pub async fn restore_linked_playlist(
        &self,
        server_playlist_id: ServerPlaylistId,
        snapshot: PersonalPlaylistSnapshot,
    ) -> Result<ServerPlaylistId, ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::CreateLinked {
                snapshot,
                replace_missing: true,
                expected_missing_server_id: Some(server_playlist_id),
                reply,
            })
        })
        .await
    }

    /// Stop synchronization while preserving both local and server playlists.
    pub async fn unlink_playlist(
        &self,
        server_playlist_id: ServerPlaylistId,
    ) -> Result<(), ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::Unlink {
                server_playlist_id,
                reply,
            })
        })
        .await
    }

    /// Delete both sides of one explicitly linked playlist.
    pub async fn delete_linked_playlist(
        &self,
        server_playlist_id: ServerPlaylistId,
    ) -> Result<(), ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::DeleteBoth {
                server_playlist_id,
                reply,
            })
        })
        .await
    }

    /// Delete only the local side of a link already known to be missing on the server.
    ///
    /// This path deliberately performs no server request. A server copy may have reappeared since
    /// the missing observation; deleting it would require the separate `DeleteBoth` confirmation.
    pub async fn delete_missing_local_playlist(
        &self,
        server_playlist_id: ServerPlaylistId,
    ) -> Result<(), ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::DeleteLocal {
                server_playlist_id,
                reply,
            })
        })
        .await
    }

    /// Forget an unresolved create intent after an explicit user decision to keep only the local
    /// side. The server may still contain a playlist when the original response was lost.
    pub async fn abandon_playlist_create(
        &self,
        local_playlist_id: PlaylistId,
    ) -> Result<(), ServerError> {
        self.request(|reply| {
            ActorCommand::Playlist(PlaylistActorCommand::AbandonCreate {
                local_playlist_id,
                reply,
            })
        })
        .await
    }
}

pub(super) enum PlaylistActorCommand {
    Prepare {
        server_playlist_id: ServerPlaylistId,
        target: PlaylistPreviewTarget,
        reply: tokio::sync::oneshot::Sender<Result<PlaylistMergePreview, ServerError>>,
    },
    ApplyPreview {
        preview_id: String,
        server_playlist_id: ServerPlaylistId,
        current_local: Option<PersonalPlaylistSnapshot>,
        reply: tokio::sync::oneshot::Sender<Result<PlaylistId, ServerError>>,
    },
    Reconcile {
        snapshots: Vec<PersonalPlaylistSnapshot>,
        reply: tokio::sync::oneshot::Sender<Result<(), super::ServiceError>>,
    },
    CreateLinked {
        snapshot: PersonalPlaylistSnapshot,
        replace_missing: bool,
        expected_missing_server_id: Option<ServerPlaylistId>,
        reply: tokio::sync::oneshot::Sender<Result<ServerPlaylistId, ServerError>>,
    },
    Unlink {
        server_playlist_id: ServerPlaylistId,
        reply: tokio::sync::oneshot::Sender<Result<(), ServerError>>,
    },
    DeleteBoth {
        server_playlist_id: ServerPlaylistId,
        reply: tokio::sync::oneshot::Sender<Result<(), ServerError>>,
    },
    DeleteLocal {
        server_playlist_id: ServerPlaylistId,
        reply: tokio::sync::oneshot::Sender<Result<(), ServerError>>,
    },
    AbandonCreate {
        local_playlist_id: PlaylistId,
        reply: tokio::sync::oneshot::Sender<Result<(), ServerError>>,
    },
}

pub(super) async fn handle_command(
    command: PlaylistActorCommand,
    cache: &mut PlaylistPreviewCache,
    store_set: &mut OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
    bridge: &BridgeRuntime,
) {
    match command {
        PlaylistActorCommand::Prepare {
            server_playlist_id,
            target,
            mut reply,
        } => {
            let operation = async {
                let remote = client
                    .get_playlist_write_snapshot(
                        store_set.private_state.credential(),
                        &server_playlist_id,
                    )
                    .await?;
                let writable =
                    has_exact_playlist_write_access(&remote, store_set.private_state.credential());
                cache.prepare(&remote, target, writable, crate::signals::unix_now())
            };
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = operation => result,
            };
            let _ = reply.send(result);
        }
        PlaylistActorCommand::ApplyPreview {
            preview_id,
            server_playlist_id,
            current_local,
            mut reply,
        } => {
            let operation = async {
                let remote = client
                    .get_playlist_write_snapshot(
                        store_set.private_state.credential(),
                        &server_playlist_id,
                    )
                    .await?;
                let writable =
                    has_exact_playlist_write_access(&remote, store_set.private_state.credential());
                let prepared =
                    cache.take(&preview_id, &remote, current_local.as_ref(), writable)?;
                let local_playlist_id = prepared.preview.local_playlist_id.clone();
                let PreparedPlaylistCommit {
                    pending_import,
                    link,
                    projection,
                    ..
                } = prepared;
                bridge
                    .commit_playlist_preview(store_set, pending_import, link, projection)
                    .map_err(service_error_as_server)?;
                Ok(local_playlist_id)
            };
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = operation => result,
            };
            let _ = reply.send(result);
        }
        PlaylistActorCommand::Reconcile { snapshots, reply } => {
            let result = bridge.reconcile_linked_playlists(store_set, &snapshots);
            let failed = result.err();
            let _ = reply.send(failed.map_or(Ok(()), Err));
            if let Some(error) = failed {
                tracing::warn!(reason = %error, "music server playlists will retry");
            }
        }
        PlaylistActorCommand::CreateLinked {
            snapshot,
            replace_missing,
            expected_missing_server_id,
            reply,
        } => {
            // A create request is replay-unsafe. Once its durable intent exists, finish the
            // operation even if the caller disappears so receiver cancellation cannot strand an
            // avoidable unknown outcome.
            let result = create_linked(
                store_set,
                client,
                bridge,
                snapshot,
                replace_missing,
                expected_missing_server_id.as_ref(),
            )
            .await;
            let _ = reply.send(result);
        }
        PlaylistActorCommand::Unlink {
            server_playlist_id,
            reply,
        } => {
            let result = linked_by_server(store_set, &server_playlist_id)
                .ok_or(ServerError::NotFound)
                .and_then(|link| {
                    bridge
                        .unlink_playlist(store_set, &link.local_playlist_id)
                        .map_err(service_error_as_server)
                });
            let _ = reply.send(result);
        }
        PlaylistActorCommand::DeleteBoth {
            server_playlist_id,
            mut reply,
        } => {
            let operation = delete_both(store_set, client, bridge, &server_playlist_id);
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = operation => result,
            };
            let _ = reply.send(result);
        }
        PlaylistActorCommand::DeleteLocal {
            server_playlist_id,
            reply,
        } => {
            let result = delete_missing_local(store_set, bridge, &server_playlist_id);
            let _ = reply.send(result);
        }
        PlaylistActorCommand::AbandonCreate {
            local_playlist_id,
            reply,
        } => {
            let result = bridge
                .cancel_playlist_create(store_set, &local_playlist_id)
                .map_err(service_error_as_server);
            let _ = reply.send(result);
        }
    }
}

fn has_exact_playlist_write_access(
    remote: &ServerPlaylistWriteSnapshot,
    credential: &super::super::private_store::ServerCredential,
) -> bool {
    remote.read_only() == Some(false)
        && credential
            .username()
            .is_some_and(|username| remote.owner() == Some(username.expose_secret()))
}

async fn create_linked(
    store_set: &mut OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
    bridge: &BridgeRuntime,
    snapshot: PersonalPlaylistSnapshot,
    replace_missing: bool,
    expected_missing_server_id: Option<&ServerPlaylistId>,
) -> Result<ServerPlaylistId, ServerError> {
    let existing = store_set
        .bridge_state
        .playlist_link(&snapshot.playlist_id)
        .cloned();
    if replace_missing {
        if !existing.as_ref().is_some_and(|link| {
            link.state == PlaylistLinkState::ServerMissing
                && expected_missing_server_id
                    .is_none_or(|expected| &link.server_playlist_id == expected)
        }) {
            return Err(ServerError::NotFound);
        }
    } else if existing.is_some() {
        return Err(ServerError::InvalidResponse);
    }
    let pending = store_set
        .bridge_state
        .pending_playlist_creates()
        .get(&snapshot.playlist_id)
        .cloned();
    if replace_missing && pending.is_none() {
        let missing = existing.ok_or(ServerError::NotFound)?;
        match client
            .get_playlist_write_snapshot(
                store_set.private_state.credential(),
                &missing.server_playlist_id,
            )
            .await
        {
            Ok(remote) => {
                if !has_exact_playlist_write_access(&remote, store_set.private_state.credential()) {
                    return Err(ServerError::PermissionDenied);
                }
                let server_playlist_id = remote.id().clone();
                bridge
                    .restore_reappeared_playlist(store_set, missing, remote)
                    .map_err(service_error_as_server)?;
                return Ok(server_playlist_id);
            }
            Err(ServerError::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    let (intent, created_id) = if let Some(pending) = pending {
        if pending.expected_missing_server_id.as_ref() != expected_missing_server_id {
            return Err(ServerError::InvalidResponse);
        }
        let created_id = pending
            .created_server_playlist_id
            .clone()
            .ok_or(ServerError::TemporarilyUnavailable)?;
        (pending, created_id)
    } else {
        let entries = local_item_refs(store_set, &snapshot)?;
        let intent = PendingPlaylistCreate {
            local_playlist_id: snapshot.playlist_id.clone(),
            expected_missing_server_id: expected_missing_server_id.cloned(),
            created_server_playlist_id: None,
            desired_name: snapshot.name.clone(),
            ordered_entry_ids: snapshot
                .entries
                .iter()
                .map(|entry| entry.entry_id.clone())
                .collect(),
            ordered_item_ids: entries
                .iter()
                .map(|entry| entry.item_id().clone())
                .collect(),
            started_at_unix: crate::signals::unix_now(),
        };
        bridge
            .begin_playlist_create(store_set, intent.clone())
            .map_err(service_error_as_server)?;
        let created = match client
            .create_playlist(
                store_set.private_state.credential(),
                &snapshot.name,
                &entries,
            )
            .await
        {
            Ok(Some(created)) => created,
            Ok(None) => return Err(ServerError::TemporarilyUnavailable),
            Err(MutationDeliveryError::Ambiguous(error)) => return Err(error),
            Err(MutationDeliveryError::DefinitelyNotApplied(error)) => {
                bridge
                    .cancel_playlist_create(store_set, &snapshot.playlist_id)
                    .map_err(service_error_as_server)?;
                return Err(error);
            }
        };
        let created_id = created.id().clone();
        bridge
            .record_playlist_create_server_id(store_set, &snapshot.playlist_id, created_id.clone())
            .map_err(service_error_as_server)?;
        (intent, created_id)
    };
    let remote = client
        .get_playlist_write_snapshot(store_set.private_state.credential(), &created_id)
        .await?;
    verify_created_snapshot(&intent, &remote, store_set.private_state.credential())?;
    let server_playlist_id = remote.id().clone();
    let link = PlaylistLink {
        local_playlist_id: intent.local_playlist_id,
        server_playlist_id: server_playlist_id.clone(),
        managed_by_yututui: true,
        state: PlaylistLinkState::Linked,
        content_needs_attention: false,
        shadow: PlaylistShadow {
            name: remote.name().to_owned(),
            occurrences: intent
                .ordered_entry_ids
                .into_iter()
                .zip(remote.entries())
                .map(|(entry_id, remote)| PlaylistShadowOccurrence {
                    entry_id,
                    item_id: remote.item.item_id().clone(),
                })
                .collect(),
            verified_at_unix: crate::signals::unix_now(),
        },
    };
    bridge
        .commit_created_playlist_link(store_set, link, &snapshot, replace_missing)
        .map_err(service_error_as_server)?;
    Ok(server_playlist_id)
}

async fn delete_both(
    store_set: &mut OpenSubsonicStoreSet,
    client: &OpenSubsonicClient,
    bridge: &BridgeRuntime,
    server_playlist_id: &ServerPlaylistId,
) -> Result<(), ServerError> {
    let link = linked_by_server(store_set, server_playlist_id).ok_or(ServerError::NotFound)?;
    let remote = match client
        .get_playlist_write_snapshot(store_set.private_state.credential(), server_playlist_id)
        .await
    {
        Ok(remote) => Some(remote),
        Err(ServerError::NotFound) => None,
        Err(error) => return Err(error),
    };
    if let Some(remote) = remote {
        let deletion = if link.managed_by_yututui {
            client
                .delete_managed_playlist(store_set.private_state.credential(), &remote)
                .await
        } else {
            client
                .delete_playlist(store_set.private_state.credential(), &remote)
                .await
        };
        if let Err(MutationDeliveryError::DefinitelyNotApplied(error)) = deletion {
            return Err(error);
        }
        match client
            .get_playlist_write_snapshot(store_set.private_state.credential(), server_playlist_id)
            .await
        {
            Err(ServerError::NotFound) => {}
            Ok(_) => return Err(ServerError::TemporarilyUnavailable),
            Err(error) => return Err(error),
        }
    }
    // Each accepted lifecycle deletion is a distinct causal operation. Reusing a hash of the
    // local/server pair would make delete -> restore/re-link -> delete collapse into the first
    // ledger envelope.
    let operation_id = random_opaque_id("playlist-delete")?;
    bridge
        .commit_deleted_playlist(store_set, &link, operation_id)
        .map_err(service_error_as_server)
}

fn delete_missing_local(
    store_set: &mut OpenSubsonicStoreSet,
    bridge: &BridgeRuntime,
    server_playlist_id: &ServerPlaylistId,
) -> Result<(), ServerError> {
    let link = linked_by_server(store_set, server_playlist_id).ok_or(ServerError::NotFound)?;
    if link.state != PlaylistLinkState::ServerMissing {
        return Err(ServerError::NotFound);
    }
    let operation_id = random_opaque_id("playlist-delete")?;
    bridge
        .commit_deleted_playlist(store_set, &link, operation_id)
        .map_err(service_error_as_server)
}

fn linked_by_server(
    store_set: &OpenSubsonicStoreSet,
    server_playlist_id: &ServerPlaylistId,
) -> Option<PlaylistLink> {
    store_set
        .bridge_state
        .playlist_links()
        .values()
        .find(|link| &link.server_playlist_id == server_playlist_id)
        .cloned()
}

fn local_item_refs(
    store_set: &OpenSubsonicStoreSet,
    snapshot: &PersonalPlaylistSnapshot,
) -> Result<Vec<super::super::OpenSubsonicItemRef>, ServerError> {
    snapshot
        .entries
        .iter()
        .map(|entry| {
            let PortableTrackKey::OpenSubsonic {
                backend_id,
                account_scope_id,
                item_id,
            } = &entry.track.key
            else {
                return Err(ServerError::InvalidResponse);
            };
            if backend_id != store_set.profile.backend_id().as_str()
                || account_scope_id != store_set.profile.account_scope_id().as_str()
            {
                return Err(ServerError::WrongAccountScope);
            }
            Ok(super::super::OpenSubsonicItemRef::new(
                store_set.profile.backend_id().clone(),
                store_set.profile.account_scope_id().clone(),
                ItemId::new(item_id.clone()).map_err(|_| ServerError::InvalidResponse)?,
            ))
        })
        .collect()
}

fn verify_created_snapshot(
    pending: &PendingPlaylistCreate,
    remote: &ServerPlaylistWriteSnapshot,
    credential: &super::super::private_store::ServerCredential,
) -> Result<(), ServerError> {
    if remote.read_only() != Some(false) {
        return Err(ServerError::PermissionDenied);
    }
    let owner_matches = credential
        .username()
        .zip(remote.owner())
        .is_some_and(|(username, owner)| username.expose_secret() == owner);
    if !owner_matches {
        return Err(ServerError::PermissionDenied);
    }
    let remote_ids = remote
        .entries()
        .iter()
        .map(|entry| entry.item.item_id())
        .collect::<Vec<_>>();
    if remote.name() != pending.desired_name
        || pending.ordered_item_ids.iter().collect::<Vec<_>>() != remote_ids
    {
        return Err(ServerError::InvalidResponse);
    }
    Ok(())
}

fn service_error_as_server(error: super::ServiceError) -> ServerError {
    match error {
        super::ServiceError::Server(error) => error,
        super::ServiceError::Store(_)
        | super::ServiceError::ActorUnavailable
        | super::ServiceError::ProxyUnavailable
        | super::ServiceError::InvalidSetup => ServerError::TemporarilyUnavailable,
    }
}

pub(super) struct PreparedPlaylistCommit {
    pub preview: PlaylistMergePreview,
    pub remote_fingerprint: String,
    pub local_fingerprint: Option<String>,
    pub pending_import: PendingPlaylistImportBatch,
    pub link: Option<PlaylistLink>,
    pub projection: Option<PendingPlaylistProjection>,
}

struct CachedPreview {
    expires_at: Instant,
    prepared: PreparedPlaylistCommit,
}

#[derive(Default)]
pub(super) struct PlaylistPreviewCache {
    previews: BTreeMap<String, CachedPreview>,
}

impl PlaylistPreviewCache {
    pub(super) fn prepare(
        &mut self,
        remote: &ServerPlaylistWriteSnapshot,
        target: PlaylistPreviewTarget,
        link_is_writable: bool,
        observed_at_unix: i64,
    ) -> Result<PlaylistMergePreview, ServerError> {
        self.expire();
        if self.previews.len() >= MAX_PREVIEWS {
            let Some(oldest) = self
                .previews
                .iter()
                .min_by_key(|(_, cached)| cached.expires_at)
                .map(|(id, _)| id.clone())
            else {
                return Err(ServerError::TemporarilyUnavailable);
            };
            self.previews.remove(&oldest);
        }
        let prepared = prepare_commit(remote, target, link_is_writable, observed_at_unix)?;
        let preview = prepared.preview.clone();
        self.previews.insert(
            preview.preview_id.clone(),
            CachedPreview {
                expires_at: Instant::now() + PREVIEW_TTL,
                prepared,
            },
        );
        Ok(preview)
    }

    pub(super) fn take(
        &mut self,
        preview_id: &str,
        remote: &ServerPlaylistWriteSnapshot,
        current_local: Option<&PersonalPlaylistSnapshot>,
        link_is_writable: bool,
    ) -> Result<PreparedPlaylistCommit, ServerError> {
        self.expire();
        let cached = self
            .previews
            .remove(preview_id)
            .ok_or(ServerError::InvalidResponse)?;
        if cached.prepared.preview.mode != PlaylistPreviewMode::ImportCopy && !link_is_writable {
            return Err(ServerError::PermissionDenied);
        }
        if remote_fingerprint(remote) != cached.prepared.remote_fingerprint {
            return Err(ServerError::TemporarilyUnavailable);
        }
        match (&cached.prepared.local_fingerprint, current_local) {
            (None, None) => {}
            (Some(expected), Some(current)) if *expected == local_fingerprint(current) => {}
            _ => return Err(ServerError::TemporarilyUnavailable),
        }
        Ok(cached.prepared)
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.previews.retain(|_, cached| cached.expires_at > now);
    }
}

fn prepare_commit(
    remote: &ServerPlaylistWriteSnapshot,
    target: PlaylistPreviewTarget,
    link_is_writable: bool,
    observed_at_unix: i64,
) -> Result<PreparedPlaylistCommit, ServerError> {
    let mode = match &target {
        PlaylistPreviewTarget::ImportCopy => PlaylistPreviewMode::ImportCopy,
        PlaylistPreviewTarget::LinkNew => PlaylistPreviewMode::LinkNew,
        PlaylistPreviewTarget::LinkExisting(_) => PlaylistPreviewMode::LinkExisting,
    };
    if mode != PlaylistPreviewMode::ImportCopy && !link_is_writable {
        return Err(ServerError::PermissionDenied);
    }
    let preview_id = random_opaque_id("playlist-preview")?;

    let local = match &target {
        PlaylistPreviewTarget::LinkExisting(local) => Some(local.clone()),
        PlaylistPreviewTarget::ImportCopy | PlaylistPreviewTarget::LinkNew => None,
    };
    let local_playlist_id = local.as_ref().map_or_else(
        || random_playlist_id(&preview_id),
        |playlist| Ok(playlist.playlist_id.clone()),
    )?;
    let local_name = local.as_ref().map_or_else(
        || remote.name().to_owned(),
        |playlist| playlist.name.clone(),
    );
    let local_entries = local
        .as_ref()
        .map(|playlist| linked_local_entries(remote, playlist))
        .transpose()?
        .unwrap_or_default();
    let remote_items = remote
        .entries()
        .iter()
        .map(|song| song.item.item_id().clone())
        .collect::<Vec<_>>();
    let merge = plan_initial_merge(&local_entries, &remote_items)
        .map_err(|_| ServerError::InvalidResponse)?;

    let mut local_tracks = BTreeMap::<PlaylistEntryId, PortableTrack>::new();
    if let Some(local) = &local {
        for entry in &local.entries {
            local_tracks.insert(entry.entry_id.clone(), entry.track.clone());
        }
    }
    let mut ordered = Vec::<(PlaylistEntryId, ItemId, PortableTrack)>::with_capacity(
        merge.ordered_occurrences().len(),
    );
    for occurrence in merge.ordered_occurrences() {
        match occurrence {
            InitialMergeOccurrence::Matched(matched) => {
                let song = remote
                    .entries()
                    .get(matched.remote_index)
                    .ok_or(ServerError::InvalidResponse)?;
                ordered.push((
                    matched.entry_id.clone(),
                    matched.item_id.clone(),
                    portable_server_track(song),
                ));
            }
            InitialMergeOccurrence::RemoteOnly(remote_only) => {
                let song = remote
                    .entries()
                    .get(remote_only.index)
                    .ok_or(ServerError::InvalidResponse)?;
                ordered.push((
                    deterministic_entry_id(&preview_id, remote_only.index, &remote_only.item_id)?,
                    remote_only.item_id.clone(),
                    portable_server_track(song),
                ));
            }
            InitialMergeOccurrence::LocalOnly(local_only) => {
                let track = local_tracks
                    .get(&local_only.entry.entry_id)
                    .cloned()
                    .ok_or(ServerError::InvalidResponse)?;
                ordered.push((
                    local_only.entry.entry_id.clone(),
                    local_only.entry.item_id.clone(),
                    track,
                ));
            }
        }
    }

    let mut operations = Vec::with_capacity(ordered.len() + 1);
    operations.push(ExternalOperationInput {
        acknowledgement_id: operation_acknowledgement(&preview_id, 0),
        operation: Operation::UpsertPlaylist {
            playlist_id: local_playlist_id.clone(),
            name: local_name.clone(),
        },
        recorded_at_unix: observed_at_unix,
    });
    let mut after_entry_id = None;
    for (index, (entry_id, _, track)) in ordered.iter().enumerate() {
        operations.push(ExternalOperationInput {
            acknowledgement_id: operation_acknowledgement(&preview_id, index + 1),
            operation: Operation::UpsertPlaylistEntry {
                playlist_id: local_playlist_id.clone(),
                entry_id: entry_id.clone(),
                track: track.clone(),
                after_entry_id: after_entry_id.clone(),
            },
            recorded_at_unix: observed_at_unix,
        });
        after_entry_id = Some(entry_id.clone());
    }
    let pending_import = PendingPlaylistImportBatch {
        operation_id: format!("playlist-batch-{}", digest_text(&preview_id)),
        local_playlist_id: local_playlist_id.clone(),
        purpose: PendingPlaylistImportPurpose::InitialOrImportCopy,
        operations,
    };

    let (link, projection) =
        if mode == PlaylistPreviewMode::ImportCopy {
            (None, None)
        } else {
            let current_occurrences = ordered
                .iter()
                .take(remote_items.len())
                .map(|(entry_id, item_id, _)| PlaylistShadowOccurrence {
                    entry_id: entry_id.clone(),
                    item_id: item_id.clone(),
                })
                .collect();
            let link = PlaylistLink {
                local_playlist_id: local_playlist_id.clone(),
                server_playlist_id: remote.id().clone(),
                managed_by_yututui: false,
                state: PlaylistLinkState::Linked,
                content_needs_attention: false,
                shadow: PlaylistShadow {
                    name: remote.name().to_owned(),
                    occurrences: current_occurrences,
                    verified_at_unix: observed_at_unix,
                },
            };
            let desired_item_ids = ordered
                .iter()
                .map(|(_, item_id, _)| item_id.clone())
                .collect::<Vec<_>>();
            let projection = (desired_item_ids != remote_items || local_name != remote.name())
                .then(|| PendingPlaylistProjection {
                    desired_name: local_name.clone(),
                    ordered_entry_ids: ordered
                        .iter()
                        .map(|(entry_id, _, _)| entry_id.clone())
                        .collect(),
                    ordered_item_ids: desired_item_ids,
                    stage: PendingPlaylistProjectionStage::Queued,
                    base_remote_fingerprint: remote_fingerprint(remote),
                });
            (Some(link), projection)
        };

    let preview = PlaylistMergePreview {
        preview_id,
        mode,
        server_playlist_id: remote.id().clone(),
        local_playlist_id,
        name: local_name,
        remote_tracks: remote.entries().len(),
        add_to_local: merge.preview().add_to_local,
        add_to_server: merge.preview().add_to_remote,
    };
    Ok(PreparedPlaylistCommit {
        remote_fingerprint: remote_fingerprint(remote),
        local_fingerprint: local.as_ref().map(local_fingerprint),
        preview,
        pending_import,
        link,
        projection,
    })
}

fn linked_local_entries(
    remote: &ServerPlaylistWriteSnapshot,
    local: &PersonalPlaylistSnapshot,
) -> Result<Vec<LinkedPlaylistEntry>, ServerError> {
    local
        .entries
        .iter()
        .map(|entry| {
            let PortableTrackKey::OpenSubsonic {
                backend_id,
                account_scope_id,
                item_id,
            } = &entry.track.key
            else {
                return Err(ServerError::InvalidResponse);
            };
            if backend_id != remote.backend_id().as_str()
                || account_scope_id != remote.account_scope_id().as_str()
            {
                return Err(ServerError::WrongAccountScope);
            }
            Ok(LinkedPlaylistEntry::new(
                entry.entry_id.clone(),
                ItemId::new(item_id.clone()).map_err(|_| ServerError::InvalidResponse)?,
            ))
        })
        .collect()
}

pub(crate) fn remote_fingerprint(remote: &ServerPlaylistWriteSnapshot) -> String {
    sequence_fingerprint(
        remote.backend_id(),
        remote.account_scope_id(),
        remote.id(),
        remote.name(),
        remote.entries().iter().map(|song| song.item.item_id()),
    )
}

fn local_fingerprint(local: &PersonalPlaylistSnapshot) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-local-playlist-preview-v1\0");
    update_part(&mut digest, local.playlist_id.as_str().as_bytes());
    update_part(&mut digest, local.name.as_bytes());
    for entry in &local.entries {
        update_part(&mut digest, entry.entry_id.as_str().as_bytes());
        let encoded =
            serde_json::to_vec(&entry.track).expect("portable playlist tracks always serialize");
        update_part(&mut digest, &encoded);
    }
    HEXLOWER.encode(&digest.finalize())
}

fn sequence_fingerprint<'a>(
    backend_id: &BackendId,
    account_scope_id: &AccountScopeId,
    playlist_id: &ServerPlaylistId,
    name: &str,
    item_ids: impl Iterator<Item = &'a ItemId>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-playlist-snapshot-v1\0");
    for part in [
        backend_id.as_str(),
        account_scope_id.as_str(),
        playlist_id.as_str(),
        name,
    ] {
        update_part(&mut digest, part.as_bytes());
    }
    for item_id in item_ids {
        update_part(&mut digest, item_id.as_str().as_bytes());
    }
    HEXLOWER.encode(&digest.finalize())
}

fn random_opaque_id(prefix: &str) -> Result<String, ServerError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| ServerError::TemporarilyUnavailable)?;
    Ok(format!("{prefix}-{}", HEXLOWER.encode(&random)))
}

fn random_playlist_id(preview_id: &str) -> Result<PlaylistId, ServerError> {
    PlaylistId::new(format!("server-{}", digest_text(preview_id)))
        .map_err(|_| ServerError::InvalidResponse)
}

fn deterministic_entry_id(
    preview_id: &str,
    index: usize,
    item_id: &ItemId,
) -> Result<PlaylistEntryId, ServerError> {
    let material = format!("{preview_id}\0{index}\0{}", item_id.as_str());
    PlaylistEntryId::new(format!("server-entry-{}", digest_text(&material)))
        .map_err(|_| ServerError::InvalidResponse)
}

fn operation_acknowledgement(preview_id: &str, index: usize) -> String {
    digest_text(&format!("{preview_id}\0operation\0{index}"))
}

fn digest_text(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    HEXLOWER.encode(&digest.finalize())
}

fn update_part(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::{OpenSubsonicItemRef, ServerSong};
    use crate::personal_state::PersonalPlaylistEntry;

    const BACKEND: &str = "backend";
    const ACCOUNT: &str = "account";

    fn item_id(value: &str) -> ItemId {
        ItemId::new(value).unwrap()
    }

    fn server_song(backend: &str, account: &str, item: &str) -> ServerSong {
        ServerSong {
            item: OpenSubsonicItemRef::new(
                BackendId::new(backend).unwrap(),
                AccountScopeId::new(account).unwrap(),
                item_id(item),
            ),
            title: format!("Server {item}"),
            artist: "Server artist".to_owned(),
            artists: vec!["Server artist".to_owned()],
            album: Some("Server album".to_owned()),
            album_id: None,
            album_artist: None,
            duration_secs: Some(180),
            track_number: None,
            disc_number: None,
            year: None,
            cover_art_id: None,
            content_type: Some("audio/flac".to_owned()),
            suffix: Some("flac".to_owned()),
            starred: false,
            user_rating: None,
            play_count: None,
            played_at: None,
        }
    }

    fn remote(name: &str, items: &[&str]) -> ServerPlaylistWriteSnapshot {
        remote_with_access(name, items, Some("owner"), Some(false))
    }

    fn remote_with_access(
        name: &str,
        items: &[&str],
        owner: Option<&str>,
        read_only: Option<bool>,
    ) -> ServerPlaylistWriteSnapshot {
        ServerPlaylistWriteSnapshot::new(
            BackendId::new(BACKEND).unwrap(),
            AccountScopeId::new(ACCOUNT).unwrap(),
            ServerPlaylistId::new("server-playlist").unwrap(),
            name.to_owned(),
            owner.map(str::to_owned),
            read_only,
            items
                .iter()
                .map(|item| server_song(BACKEND, ACCOUNT, item))
                .collect(),
        )
    }

    fn portable_track(backend: &str, account: &str, item: &str) -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: backend.to_owned(),
                account_scope_id: account.to_owned(),
                item_id: item.to_owned(),
            },
            title: format!("Local {item}"),
            artist: "Local artist".to_owned(),
            album: Some("Local album".to_owned()),
            duration_secs: Some(181),
            isrc: None,
        }
    }

    fn local(name: &str, entries: &[(&str, &str)]) -> PersonalPlaylistSnapshot {
        PersonalPlaylistSnapshot {
            playlist_id: PlaylistId::new("local-playlist").unwrap(),
            name: name.to_owned(),
            entries: entries
                .iter()
                .map(|(entry_id, item)| PersonalPlaylistEntry {
                    entry_id: PlaylistEntryId::new(*entry_id).unwrap(),
                    track: portable_track(BACKEND, ACCOUNT, item),
                })
                .collect(),
        }
    }

    fn imported_entries(
        prepared: &PreparedPlaylistCommit,
    ) -> Vec<(&PlaylistEntryId, &PortableTrack, &Option<PlaylistEntryId>)> {
        prepared
            .pending_import
            .operations
            .iter()
            .filter_map(|input| match &input.operation {
                Operation::UpsertPlaylistEntry {
                    entry_id,
                    track,
                    after_entry_id,
                    ..
                } => Some((entry_id, track, after_entry_id)),
                _ => None,
            })
            .collect()
    }

    fn track_item_id(track: &PortableTrack) -> &str {
        match &track.key {
            PortableTrackKey::OpenSubsonic { item_id, .. } => item_id,
            _ => panic!("expected an exact OpenSubsonic item"),
        }
    }

    #[test]
    fn import_copy_preserves_duplicate_occurrences_without_creating_a_link() {
        let remote = remote("Duplicates", &["same", "same"]);

        let prepared =
            prepare_commit(&remote, PlaylistPreviewTarget::ImportCopy, false, 100).unwrap();

        assert_eq!(prepared.preview.mode, PlaylistPreviewMode::ImportCopy);
        assert_eq!(prepared.preview.remote_tracks, 2);
        assert_eq!(prepared.preview.add_to_local, 2);
        assert_eq!(prepared.preview.add_to_server, 0);
        assert!(prepared.link.is_none());
        assert!(prepared.projection.is_none());
        let entries = imported_entries(&prepared);
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0].0, entries[1].0);
        assert_eq!(track_item_id(entries[0].1), "same");
        assert_eq!(track_item_id(entries[1].1), "same");
        assert_eq!(entries[0].2, &None);
        assert_eq!(entries[1].2.as_ref(), Some(entries[0].0));
    }

    #[test]
    fn link_new_requires_explicit_writable_proof() {
        assert!(matches!(
            prepare_commit(
                &remote("Remote", &["a"]),
                PlaylistPreviewTarget::LinkNew,
                false,
                100,
            ),
            Err(ServerError::PermissionDenied)
        ));
    }

    #[test]
    fn link_existing_keeps_remote_order_then_appends_local_only_occurrences() {
        let remote = remote("Same name", &["a", "b"]);
        let local = local("Same name", &[("local-a", "a"), ("local-c", "c")]);

        let prepared = prepare_commit(
            &remote,
            PlaylistPreviewTarget::LinkExisting(local.clone()),
            true,
            100,
        )
        .unwrap();

        assert_eq!(prepared.preview.mode, PlaylistPreviewMode::LinkExisting);
        assert_eq!(prepared.preview.local_playlist_id, local.playlist_id);
        assert_eq!(prepared.preview.add_to_local, 1);
        assert_eq!(prepared.preview.add_to_server, 1);
        assert_eq!(
            imported_entries(&prepared)
                .iter()
                .map(|(_, track, _)| track_item_id(track))
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        let link = prepared.link.as_ref().unwrap();
        assert_eq!(link.local_playlist_id, local.playlist_id);
        assert_eq!(&link.server_playlist_id, remote.id());
        assert_eq!(
            link.shadow
                .occurrences
                .iter()
                .map(|occurrence| occurrence.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            prepared
                .projection
                .as_ref()
                .unwrap()
                .ordered_item_ids
                .iter()
                .map(ItemId::as_str)
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn equal_names_do_not_infer_a_different_local_identity() {
        let remote = remote("Shared display name", &["a"]);
        let mut explicitly_selected = local("Shared display name", &[("entry", "a")]);
        explicitly_selected.playlist_id = PlaylistId::new("explicit-selection").unwrap();

        let prepared = prepare_commit(
            &remote,
            PlaylistPreviewTarget::LinkExisting(explicitly_selected.clone()),
            true,
            100,
        )
        .unwrap();

        assert_eq!(
            prepared.preview.local_playlist_id,
            explicitly_selected.playlist_id
        );
        assert_eq!(
            prepared.link.unwrap().local_playlist_id,
            explicitly_selected.playlist_id
        );
    }

    #[test]
    fn link_existing_rejects_wrong_backend_and_local_placeholder_tracks() {
        let remote = remote("Remote", &["a"]);
        let wrong_backend = PersonalPlaylistSnapshot {
            playlist_id: PlaylistId::new("wrong-backend").unwrap(),
            name: "Local".to_owned(),
            entries: vec![PersonalPlaylistEntry {
                entry_id: PlaylistEntryId::new("entry").unwrap(),
                track: portable_track("other-backend", ACCOUNT, "a"),
            }],
        };
        assert!(matches!(
            prepare_commit(
                &remote,
                PlaylistPreviewTarget::LinkExisting(wrong_backend),
                true,
                100,
            ),
            Err(ServerError::WrongAccountScope)
        ));

        let placeholder = PersonalPlaylistSnapshot {
            playlist_id: PlaylistId::new("placeholder").unwrap(),
            name: "Local".to_owned(),
            entries: vec![PersonalPlaylistEntry {
                entry_id: PlaylistEntryId::new("entry").unwrap(),
                track: PortableTrack {
                    key: PortableTrackKey::LocalPlaceholder {
                        portable_placeholder_id: "placeholder-track".to_owned(),
                    },
                    title: "Local file".to_owned(),
                    artist: "Artist".to_owned(),
                    album: None,
                    duration_secs: None,
                    isrc: None,
                },
            }],
        };
        assert!(matches!(
            prepare_commit(
                &remote,
                PlaylistPreviewTarget::LinkExisting(placeholder),
                true,
                100,
            ),
            Err(ServerError::InvalidResponse)
        ));
    }

    #[test]
    fn take_rejects_remote_change_and_consumes_the_preview() {
        let mut cache = PlaylistPreviewCache::default();
        let original = remote("Remote", &["a", "b"]);
        let preview = cache
            .prepare(&original, PlaylistPreviewTarget::LinkNew, true, 100)
            .unwrap();
        let changed = remote("Remote", &["b", "a"]);

        assert!(matches!(
            cache.take(&preview.preview_id, &changed, None, true),
            Err(ServerError::TemporarilyUnavailable)
        ));
        assert!(matches!(
            cache.take(&preview.preview_id, &original, None, true),
            Err(ServerError::InvalidResponse)
        ));
    }

    #[test]
    fn take_rechecks_exact_owner_and_writable_evidence_for_link_previews() {
        let credential = super::super::super::ServerCredential::password(
            "owner",
            age::secrecy::SecretString::from("password".to_owned()),
        )
        .unwrap();
        for changed_access in [
            remote_with_access("Remote", &["a"], Some("other-owner"), Some(false)),
            remote_with_access("Remote", &["a"], Some("owner"), Some(true)),
            remote_with_access("Remote", &["a"], None, Some(false)),
            remote_with_access("Remote", &["a"], Some("owner"), None),
        ] {
            let mut cache = PlaylistPreviewCache::default();
            let original = remote("Remote", &["a"]);
            assert!(has_exact_playlist_write_access(&original, &credential));
            let preview = cache
                .prepare(&original, PlaylistPreviewTarget::LinkNew, true, 100)
                .unwrap();
            assert_eq!(
                remote_fingerprint(&changed_access),
                remote_fingerprint(&original),
                "content-only equality must not hide changed access evidence"
            );
            let writable = has_exact_playlist_write_access(&changed_access, &credential);
            assert!(!writable);
            assert!(matches!(
                cache.take(&preview.preview_id, &changed_access, None, writable),
                Err(ServerError::PermissionDenied)
            ));
        }
    }

    #[test]
    fn take_rejects_any_local_snapshot_revision_change() {
        let mut cache = PlaylistPreviewCache::default();
        let remote = remote("Remote", &["a"]);
        let original = local("Local", &[("entry", "a")]);
        let preview = cache
            .prepare(
                &remote,
                PlaylistPreviewTarget::LinkExisting(original.clone()),
                true,
                100,
            )
            .unwrap();
        let mut changed_metadata = original.clone();
        changed_metadata.entries[0].track.title = "Edited after preview".to_owned();

        assert!(matches!(
            cache.take(&preview.preview_id, &remote, Some(&changed_metadata), true),
            Err(ServerError::TemporarilyUnavailable)
        ));
        assert!(matches!(
            cache.take(&preview.preview_id, &remote, Some(&original), true),
            Err(ServerError::InvalidResponse)
        ));
    }

    #[test]
    fn take_is_single_use_after_success() {
        let mut cache = PlaylistPreviewCache::default();
        let remote = remote("Remote", &["a"]);
        let preview = cache
            .prepare(&remote, PlaylistPreviewTarget::ImportCopy, false, 100)
            .unwrap();

        let prepared = cache
            .take(&preview.preview_id, &remote, None, false)
            .unwrap();
        assert_eq!(prepared.preview.preview_id, preview.preview_id);
        assert!(matches!(
            cache.take(&preview.preview_id, &remote, None, false),
            Err(ServerError::InvalidResponse)
        ));
    }

    #[test]
    fn take_rejects_an_expired_preview() {
        let mut cache = PlaylistPreviewCache::default();
        let remote = remote("Remote", &["a"]);
        let preview = cache
            .prepare(&remote, PlaylistPreviewTarget::ImportCopy, false, 100)
            .unwrap();
        cache
            .previews
            .get_mut(&preview.preview_id)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);

        assert!(matches!(
            cache.take(&preview.preview_id, &remote, None, false),
            Err(ServerError::InvalidResponse)
        ));
    }

    #[test]
    fn unchanged_existing_link_has_no_remote_projection() {
        let remote = remote("Same", &["a", "b"]);
        let local = local("Same", &[("a-entry", "a"), ("b-entry", "b")]);

        let prepared = prepare_commit(
            &remote,
            PlaylistPreviewTarget::LinkExisting(local),
            true,
            100,
        )
        .unwrap();

        assert_eq!(prepared.preview.add_to_local, 0);
        assert_eq!(prepared.preview.add_to_server, 0);
        assert!(prepared.link.is_some());
        assert!(prepared.projection.is_none());
    }

    #[test]
    fn changed_name_and_order_projection_keeps_entry_and_item_ids_parallel() {
        let remote = remote("Remote name", &["a", "b"]);
        let local = local(
            "Local name",
            &[("local-b", "b"), ("local-a", "a"), ("local-c", "c")],
        );

        let prepared = prepare_commit(
            &remote,
            PlaylistPreviewTarget::LinkExisting(local),
            true,
            100,
        )
        .unwrap();
        let projection = prepared.projection.as_ref().unwrap();

        assert_eq!(projection.desired_name, "Local name");
        assert_eq!(
            projection.ordered_entry_ids.len(),
            projection.ordered_item_ids.len()
        );
        assert_eq!(
            projection
                .ordered_item_ids
                .iter()
                .map(ItemId::as_str)
                .collect::<Vec<_>>(),
            vec!["a", "b", "a", "c"]
        );
        let imported = imported_entries(&prepared);
        let imported_by_id = imported
            .iter()
            .map(|(entry_id, track, _)| {
                (
                    entry_id.as_str().to_owned(),
                    track_item_id(track).to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (entry_id, item_id) in projection
            .ordered_entry_ids
            .iter()
            .zip(&projection.ordered_item_ids)
        {
            assert_eq!(
                imported_by_id.get(entry_id.as_str()).map(String::as_str),
                Some(item_id.as_str())
            );
        }
    }
}

#[cfg(test)]
#[path = "playlist_lifecycle_tests.rs"]
mod lifecycle_tests;
