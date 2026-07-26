//! Durable linked-playlist projection and remote-observation lifecycle.

use std::collections::{BTreeMap, BTreeSet};

use age::secrecy::ExposeSecret as _;
use data_encoding::HEXLOWER;
use sha2::{Digest as _, Sha256};

use super::super::actor::ServiceError;
use super::super::bridge_event::portable_server_track;
use super::super::bridge_store::{
    BridgeMutationError, PendingPlaylistCreate, PendingPlaylistImportBatch,
    PendingPlaylistImportPurpose, PendingPlaylistProjection, PendingPlaylistProjectionStage,
    PlaylistLink, PlaylistLinkState, PlaylistShadow, PlaylistShadowOccurrence,
};
use super::super::client::{MutationDeliveryError, OpenSubsonicClient};
use super::super::linked_playlists::{
    LinkedPlaylistEntry, PendingMergeOccurrence, PendingRemoteMergeMode, PendingRemoteOccurrence,
    plan_pending_remote_merge, plan_remote_delta, plan_remote_update,
};
use super::super::model::{ItemId, OpenSubsonicItemRef, ServerPlaylistWriteSnapshot};
use super::super::private_store::ServerCredential;
use super::super::transaction::OpenSubsonicStoreSet;
use super::BridgeRuntime;
use crate::personal_state::{
    ExternalOperationInput, Operation, PersonalPlaylistSnapshot, PlaylistEntryId, PlaylistId,
    PortableTrackKey,
};

impl BridgeRuntime {
    pub(crate) fn begin_playlist_create(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        pending: PendingPlaylistCreate,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let before = store_set.bridge_state.clone();
        if let Err(error) = store_set.bridge_state.queue_playlist_create(pending) {
            store_set.bridge_state = before;
            return Err(error.into());
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn record_playlist_create_server_id(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        playlist_id: &PlaylistId,
        server_playlist_id: super::super::model::ServerPlaylistId,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let before = store_set.bridge_state.clone();
        if let Err(error) = store_set
            .bridge_state
            .record_playlist_create_server_id(playlist_id, server_playlist_id)
        {
            store_set.bridge_state = before;
            return Err(error.into());
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn cancel_playlist_create(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        playlist_id: &PlaylistId,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let before = store_set.bridge_state.clone();
        if store_set
            .bridge_state
            .remove_playlist_create(playlist_id)
            .is_none()
        {
            return Err(ServiceError::InvalidSetup);
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn commit_playlist_preview(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        pending_import: PendingPlaylistImportBatch,
        link: Option<PlaylistLink>,
        projection: Option<PendingPlaylistProjection>,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let before = store_set.bridge_state.clone();
        let mutation = (|| {
            store_set
                .bridge_state
                .queue_playlist_import(pending_import)?;
            if let Some(link) = link {
                let local_playlist_id = link.local_playlist_id.clone();
                store_set.bridge_state.upsert_playlist_link(link)?;
                if let Some(projection) = projection {
                    store_set
                        .bridge_state
                        .queue_playlist_projection(local_playlist_id, projection)?;
                }
            } else if projection.is_some() {
                return Err(BridgeMutationError::InvalidEntry);
            }
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(error.into());
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    pub(crate) fn commit_created_playlist_link(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        link: PlaylistLink,
        current_snapshot: &PersonalPlaylistSnapshot,
        replace_missing: bool,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        if current_snapshot.playlist_id != link.local_playlist_id {
            return Err(ServiceError::InvalidSetup);
        }
        let (current_entry_ids, current_item_ids) =
            exact_local_occurrences(store_set, current_snapshot)?;
        let current_matches_created_shadow = link.shadow.name == current_snapshot.name
            && link.shadow.occurrences.len() == current_entry_ids.len()
            && link
                .shadow
                .occurrences
                .iter()
                .zip(current_entry_ids.iter().zip(&current_item_ids))
                .all(|(shadow, (entry_id, item_id))| {
                    &shadow.entry_id == entry_id && &shadow.item_id == item_id
                });
        let follow_up_projection =
            (!current_matches_created_shadow).then(|| PendingPlaylistProjection {
                desired_name: current_snapshot.name.clone(),
                ordered_entry_ids: current_entry_ids,
                ordered_item_ids: current_item_ids,
                stage: PendingPlaylistProjectionStage::Queued,
                base_remote_fingerprint: shadow_fingerprint(store_set, &link),
            });
        let before = store_set.bridge_state.clone();
        let local_playlist_id = link.local_playlist_id.clone();
        let mutation = (|| {
            let pending_create = store_set
                .bridge_state
                .remove_playlist_create(&local_playlist_id);
            if let Some(pending) = &pending_create
                && (pending.created_server_playlist_id.as_ref() != Some(&link.server_playlist_id)
                    || pending.expected_missing_server_id.is_some() != replace_missing)
            {
                return Err(BridgeMutationError::ConflictingEntry);
            }
            if replace_missing {
                let replaceable = store_set
                    .bridge_state
                    .playlist_link(&local_playlist_id)
                    .is_some_and(|existing| existing.state == PlaylistLinkState::ServerMissing);
                if !replaceable {
                    return Err(BridgeMutationError::ConflictingEntry);
                }
                store_set
                    .bridge_state
                    .remove_playlist_projection(&local_playlist_id);
                store_set
                    .bridge_state
                    .remove_playlist_link(&local_playlist_id);
            }
            store_set.bridge_state.upsert_playlist_link(link)?;
            if let Some(projection) = follow_up_projection {
                store_set
                    .bridge_state
                    .queue_playlist_projection(local_playlist_id, projection)?;
            }
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(error.into());
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn unlink_playlist(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        playlist_id: &PlaylistId,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let before = store_set.bridge_state.clone();
        store_set
            .bridge_state
            .remove_playlist_projection(playlist_id);
        store_set.bridge_state.remove_playlist_create(playlist_id);
        if store_set
            .bridge_state
            .remove_playlist_link(playlist_id)
            .is_none()
        {
            store_set.bridge_state = before;
            return Err(ServiceError::InvalidSetup);
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn commit_deleted_playlist(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        link: &PlaylistLink,
        operation_id: String,
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let before = store_set.bridge_state.clone();
        let mutation = (|| {
            // The explicit delete is causally newer than every unacknowledged observation for
            // this local playlist. Retire the older batch in the same bridge-store transaction so
            // lexical BTree ordering or restart replay can never resurrect it before the delete.
            store_set
                .bridge_state
                .retire_playlist_import(&link.local_playlist_id);
            store_set
                .bridge_state
                .queue_playlist_import(PendingPlaylistImportBatch {
                    operation_id: operation_id.clone(),
                    local_playlist_id: link.local_playlist_id.clone(),
                    purpose: PendingPlaylistImportPurpose::Delete,
                    operations: vec![ExternalOperationInput {
                        acknowledgement_id: format!("{operation_id}-delete"),
                        operation: Operation::DeletePlaylist {
                            playlist_id: link.local_playlist_id.clone(),
                            deleted: true,
                        },
                        recorded_at_unix: crate::signals::unix_now(),
                    }],
                })?;
            store_set
                .bridge_state
                .remove_playlist_projection(&link.local_playlist_id);
            store_set
                .bridge_state
                .remove_playlist_create(&link.local_playlist_id);
            let removed = store_set
                .bridge_state
                .remove_playlist_link(&link.local_playlist_id)
                .ok_or(BridgeMutationError::ConflictingEntry)?;
            if removed.server_playlist_id != link.server_playlist_id {
                return Err(BridgeMutationError::ConflictingEntry);
            }
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(error.into());
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    /// Queue the canonical local winners for linked server projection.
    ///
    /// A missing local playlist uses the documented safe default: unlink it while leaving the
    /// server copy untouched. A projection already in an ambiguous/readback stage is never
    /// replaced; after its readback settles, the latest local snapshot queues a follow-up.
    pub(crate) fn reconcile_linked_playlists(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        snapshots: &[PersonalPlaylistSnapshot],
    ) -> Result<(), ServiceError> {
        if !self.is_writable() {
            return Err(ServiceError::InvalidSetup);
        }
        let by_id = snapshots
            .iter()
            .map(|snapshot| (&snapshot.playlist_id, snapshot))
            .collect::<BTreeMap<_, _>>();
        let before = store_set.bridge_state.clone();
        let links = store_set
            .bridge_state
            .playlist_links()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mutation = (|| {
            for link in links {
                let snapshot = by_id.get(&link.local_playlist_id).copied();
                if let Some(pending) = store_set
                    .bridge_state
                    .pending_playlist_import(&link.local_playlist_id)
                    .cloned()
                {
                    match (snapshot, pending.purpose) {
                        // Until the owner acknowledges the initial merge, absence is not evidence
                        // of deletion and the pre-import snapshot is not a new local winner.
                        (_, PendingPlaylistImportPurpose::InitialOrImportCopy) | (Some(_), _) => {
                            continue;
                        }
                        // The owner has no current local playlist despite a later remote
                        // observation. Treat that as the explicit/current local deletion: discard
                        // the stale observation and unlink while keeping the server copy.
                        (None, PendingPlaylistImportPurpose::RemoteObservation) => {
                            store_set
                                .bridge_state
                                .retire_playlist_import(&link.local_playlist_id);
                            store_set
                                .bridge_state
                                .remove_playlist_projection(&link.local_playlist_id);
                            store_set
                                .bridge_state
                                .remove_playlist_create(&link.local_playlist_id);
                            store_set
                                .bridge_state
                                .remove_playlist_link(&link.local_playlist_id);
                            continue;
                        }
                        // A delete batch should already have atomically removed its link. If an
                        // older/corrupt in-memory caller presents both, never advance either side.
                        (None, PendingPlaylistImportPurpose::Delete) => continue,
                    }
                }
                let Some(snapshot) = snapshot else {
                    store_set
                        .bridge_state
                        .remove_playlist_projection(&link.local_playlist_id);
                    store_set
                        .bridge_state
                        .remove_playlist_create(&link.local_playlist_id);
                    store_set
                        .bridge_state
                        .remove_playlist_link(&link.local_playlist_id);
                    continue;
                };
                if link.state == PlaylistLinkState::ServerMissing {
                    continue;
                }
                let in_flight_projection = store_set
                    .bridge_state
                    .pending_playlist_projections()
                    .get(&link.local_playlist_id)
                    .is_some_and(|pending| {
                        matches!(
                            pending.stage,
                            PendingPlaylistProjectionStage::Ambiguous
                                | PendingPlaylistProjectionStage::Readback
                        )
                    });
                let (entry_ids, item_ids) = match exact_local_occurrences(store_set, snapshot) {
                    Ok(occurrences) => occurrences,
                    Err(_) => {
                        let mut attention = link.clone();
                        attention.content_needs_attention = true;
                        store_set.bridge_state.upsert_playlist_link(attention)?;
                        // First settle a write that may already be on the server. The independent
                        // content bit keeps status red without abandoning or replacing its
                        // ambiguous/readback record.
                        if in_flight_projection {
                            continue;
                        }
                        if store_set
                            .bridge_state
                            .pending_playlist_projections()
                            .get(&link.local_playlist_id)
                            .is_some_and(|pending| {
                                matches!(
                                    pending.stage,
                                    PendingPlaylistProjectionStage::Queued
                                        | PendingPlaylistProjectionStage::NeedsAttention
                                )
                            })
                        {
                            store_set
                                .bridge_state
                                .remove_playlist_projection(&link.local_playlist_id);
                        }
                        continue;
                    }
                };
                if link.content_needs_attention {
                    let mut recovered = link.clone();
                    recovered.content_needs_attention = false;
                    store_set.bridge_state.upsert_playlist_link(recovered)?;
                }
                let matches_shadow = link.shadow.name == snapshot.name
                    && link.shadow.occurrences.len() == entry_ids.len()
                    && link
                        .shadow
                        .occurrences
                        .iter()
                        .zip(entry_ids.iter().zip(&item_ids))
                        .all(|(shadow, (entry_id, item_id))| {
                            &shadow.entry_id == entry_id && &shadow.item_id == item_id
                        });
                let existing = store_set
                    .bridge_state
                    .pending_playlist_projections()
                    .get(&link.local_playlist_id)
                    .cloned();
                if matches_shadow {
                    if existing.is_some_and(|pending| {
                        matches!(
                            pending.stage,
                            PendingPlaylistProjectionStage::Queued
                                | PendingPlaylistProjectionStage::NeedsAttention
                        )
                    }) {
                        store_set
                            .bridge_state
                            .remove_playlist_projection(&link.local_playlist_id);
                    }
                    continue;
                }
                let pending = PendingPlaylistProjection {
                    desired_name: snapshot.name.clone(),
                    ordered_entry_ids: entry_ids,
                    ordered_item_ids: item_ids,
                    stage: PendingPlaylistProjectionStage::Queued,
                    base_remote_fingerprint: shadow_fingerprint(store_set, &link),
                };
                if let Some(existing) = existing {
                    if existing.stage != PendingPlaylistProjectionStage::Queued {
                        continue;
                    }
                    store_set
                        .bridge_state
                        .remove_playlist_projection(&link.local_playlist_id);
                }
                store_set
                    .bridge_state
                    .queue_playlist_projection(link.local_playlist_id, pending)?;
            }
            Ok::<(), ServiceError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(error);
        }
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) async fn flush_one_playlist_projection(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        client: &OpenSubsonicClient,
    ) -> Result<(), ServiceError> {
        let Some((playlist_id, pending)) = self.next_playlist_work(store_set) else {
            return Ok(());
        };
        let Some(link) = store_set.bridge_state.playlist_link(&playlist_id).cloned() else {
            let before = store_set.bridge_state.clone();
            store_set
                .bridge_state
                .remove_playlist_projection(&playlist_id);
            self.persist_or_restore(store_set, before)?;
            return Ok(());
        };
        let remote = match client
            .get_playlist_write_snapshot(
                store_set.private_state.credential(),
                &link.server_playlist_id,
            )
            .await
        {
            Ok(remote) => remote,
            Err(super::super::ServerError::NotFound) => {
                return self.mark_server_playlist_missing(store_set, link);
            }
            Err(error) if pending.is_some() && playlist_projection_error_needs_attention(error) => {
                return self.mark_playlist_projection_attention(
                    store_set,
                    &playlist_id,
                    pending.expect("pending projection was checked"),
                );
            }
            Err(error) => return Err(error.into()),
        };

        let remote_changed = super::super::actor::playlist_snapshot_fingerprint(&remote)
            != shadow_fingerprint(store_set, &link);
        if !has_exact_playlist_write_access(&remote, store_set.private_state.credential()) {
            if remote_changed {
                if let Some(pending) = pending {
                    return self.observe_pending_remote_playlist(
                        store_set,
                        link,
                        pending,
                        remote,
                        PlaylistLinkState::AccessNeedsAttention,
                    );
                }
                return self.observe_changed_remote_playlist(
                    store_set,
                    link,
                    remote,
                    PlaylistLinkState::AccessNeedsAttention,
                );
            }
            if pending
                .as_ref()
                .is_some_and(|pending| playlist_readback_matches(pending, &remote))
            {
                return self.settle_playlist_readback(
                    store_set,
                    link,
                    pending.expect("pending projection was checked"),
                    remote,
                );
            }
            return self.mark_playlist_link_access_attention(store_set, link);
        }

        let Some(pending) = pending else {
            if remote_changed || link.state == PlaylistLinkState::ServerMissing {
                return self.observe_changed_remote_playlist(
                    store_set,
                    link,
                    remote,
                    PlaylistLinkState::Linked,
                );
            }
            return Ok(());
        };

        // A changed readback is always merged using the durable delivery stage. In particular,
        // Queued means the local write was not sent, even when equal item IDs happen to exist on
        // both sides; those are distinct occurrences.
        if remote_changed {
            return self.observe_pending_remote_playlist(
                store_set,
                link,
                pending,
                remote,
                PlaylistLinkState::Linked,
            );
        }
        if playlist_readback_matches(&pending, &remote) {
            return self.settle_playlist_readback(store_set, link, pending, remote);
        }
        if pending.stage != PendingPlaylistProjectionStage::Queued {
            return self.settle_playlist_readback(store_set, link, pending, remote);
        }

        let current = remote
            .entries()
            .iter()
            .map(|song| song.item.item_id().clone())
            .collect::<Vec<_>>();
        let update = plan_remote_update(&current, &pending.ordered_item_ids)
            .map_err(|_| ServiceError::InvalidSetup)?;
        let additions = update
            .append_item_ids()
            .iter()
            .cloned()
            .map(|item_id| {
                OpenSubsonicItemRef::new(
                    store_set.profile.backend_id().clone(),
                    store_set.profile.account_scope_id().clone(),
                    item_id,
                )
            })
            .collect::<Vec<_>>();
        let removals = update
            .remove_indexes_descending()
            .iter()
            .copied()
            .map(u32::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ServiceError::InvalidSetup)?;
        let desired_name =
            (pending.desired_name != remote.name()).then_some(pending.desired_name.as_str());
        let delivery = if link.managed_by_yututui {
            client
                .update_managed_playlist(
                    store_set.private_state.credential(),
                    &remote,
                    desired_name,
                    &additions,
                    &removals,
                )
                .await
        } else {
            client
                .update_playlist(
                    store_set.private_state.credential(),
                    &remote,
                    desired_name,
                    &additions,
                    &removals,
                )
                .await
        };
        match delivery {
            Ok(()) => {
                self.set_playlist_projection_stage(
                    store_set,
                    &playlist_id,
                    pending.clone(),
                    PendingPlaylistProjectionStage::Readback,
                )?;
            }
            Err(MutationDeliveryError::DefinitelyNotApplied(error))
                if playlist_projection_error_needs_attention(error) =>
            {
                return self.mark_playlist_projection_attention(store_set, &playlist_id, pending);
            }
            Err(MutationDeliveryError::DefinitelyNotApplied(error)) => return Err(error.into()),
            Err(MutationDeliveryError::Ambiguous(_)) => {
                self.set_playlist_projection_stage(
                    store_set,
                    &playlist_id,
                    pending,
                    PendingPlaylistProjectionStage::Ambiguous,
                )?;
                return Ok(());
            }
        }

        let readback = client
            .get_playlist_write_snapshot(
                store_set.private_state.credential(),
                &link.server_playlist_id,
            )
            .await?;
        let current_pending = store_set
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id)
            .cloned()
            .ok_or(ServiceError::InvalidSetup)?;
        self.settle_playlist_readback(store_set, link, current_pending, readback)
    }

    fn next_playlist_work(
        &self,
        store_set: &OpenSubsonicStoreSet,
    ) -> Option<(PlaylistId, Option<PendingPlaylistProjection>)> {
        let mut cursors = self
            .retry_cursors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let after = cursors.playlist_after.as_ref();
        let links = store_set.bridge_state.playlist_links();
        let is_active = |playlist_id: &PlaylistId, link: &PlaylistLink| {
            let pending = store_set
                .bridge_state
                .pending_playlist_projections()
                .get(playlist_id);
            let in_flight = pending.is_some_and(|pending| {
                matches!(
                    pending.stage,
                    PendingPlaylistProjectionStage::Ambiguous
                        | PendingPlaylistProjectionStage::Readback
                )
            });
            link.state == PlaylistLinkState::Linked
                && (!link.content_needs_attention || in_flight)
                && store_set
                    .bridge_state
                    .pending_playlist_import(playlist_id)
                    .is_none()
                && pending.is_none_or(|pending| {
                    pending.stage != PendingPlaylistProjectionStage::NeedsAttention
                })
        };
        let selected = links
            .iter()
            .filter(|(playlist_id, link)| is_active(playlist_id, link))
            .find(|(playlist_id, _)| after.is_none_or(|after| *playlist_id > after))
            .or_else(|| {
                links
                    .iter()
                    .find(|(playlist_id, link)| is_active(playlist_id, link))
            })
            .map(|(playlist_id, _)| {
                (
                    playlist_id.clone(),
                    store_set
                        .bridge_state
                        .pending_playlist_projections()
                        .get(playlist_id)
                        .cloned(),
                )
            });
        if let Some((playlist_id, _)) = &selected {
            cursors.playlist_after = Some(playlist_id.clone());
        }
        selected
    }

    fn mark_playlist_projection_attention(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        playlist_id: &PlaylistId,
        pending: PendingPlaylistProjection,
    ) -> Result<(), ServiceError> {
        tracing::warn!("linked music server playlist needs review before it can be updated");
        self.set_playlist_projection_stage(
            store_set,
            playlist_id,
            pending,
            PendingPlaylistProjectionStage::NeedsAttention,
        )
    }

    fn mark_playlist_link_access_attention(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        mut link: PlaylistLink,
    ) -> Result<(), ServiceError> {
        tracing::warn!("linked music server playlist access needs review");
        let before = store_set.bridge_state.clone();
        link.state = PlaylistLinkState::AccessNeedsAttention;
        store_set.bridge_state.upsert_playlist_link(link)?;
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    fn settle_playlist_readback(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        mut link: PlaylistLink,
        pending: PendingPlaylistProjection,
        remote: ServerPlaylistWriteSnapshot,
    ) -> Result<(), ServiceError> {
        let final_state =
            if has_exact_playlist_write_access(&remote, store_set.private_state.credential()) {
                PlaylistLinkState::Linked
            } else {
                PlaylistLinkState::AccessNeedsAttention
            };
        let readback_matches = playlist_readback_matches(&pending, &remote);
        if readback_matches {
            let before = store_set.bridge_state.clone();
            let local_playlist_id = link.local_playlist_id.clone();
            link.state = final_state;
            link.shadow = PlaylistShadow {
                name: pending.desired_name,
                occurrences: pending
                    .ordered_entry_ids
                    .into_iter()
                    .zip(pending.ordered_item_ids)
                    .map(|(entry_id, item_id)| PlaylistShadowOccurrence { entry_id, item_id })
                    .collect(),
                verified_at_unix: crate::signals::unix_now(),
            };
            store_set.bridge_state.upsert_playlist_link(link)?;
            store_set
                .bridge_state
                .remove_playlist_projection(&local_playlist_id);
            self.persist_or_restore(store_set, before)?;
            return Ok(());
        }

        let readback_fingerprint = super::super::actor::playlist_snapshot_fingerprint(&remote);
        if final_state == PlaylistLinkState::AccessNeedsAttention {
            if readback_fingerprint != pending.base_remote_fingerprint {
                return self.observe_pending_remote_playlist(
                    store_set,
                    link,
                    pending,
                    remote,
                    final_state,
                );
            }
            return self.mark_playlist_link_access_attention(store_set, link);
        }
        if readback_fingerprint == pending.base_remote_fingerprint
            && pending.stage == PendingPlaylistProjectionStage::Ambiguous
        {
            return self.set_playlist_projection_stage(
                store_set,
                &link.local_playlist_id,
                pending,
                PendingPlaylistProjectionStage::Queued,
            );
        }
        if readback_fingerprint != pending.base_remote_fingerprint {
            return self.observe_pending_remote_playlist(
                store_set,
                link,
                pending,
                remote,
                PlaylistLinkState::Linked,
            );
        }
        Err(ServiceError::Server(
            super::super::ServerError::InvalidResponse,
        ))
    }

    fn set_playlist_projection_stage(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        playlist_id: &PlaylistId,
        mut pending: PendingPlaylistProjection,
        stage: PendingPlaylistProjectionStage,
    ) -> Result<(), ServiceError> {
        let before = store_set.bridge_state.clone();
        pending.stage = stage;
        store_set
            .bridge_state
            .replace_playlist_projection(playlist_id, pending)?;
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    fn mark_server_playlist_missing(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        mut link: PlaylistLink,
    ) -> Result<(), ServiceError> {
        let before = store_set.bridge_state.clone();
        link.state = PlaylistLinkState::ServerMissing;
        store_set
            .bridge_state
            .remove_playlist_projection(&link.local_playlist_id);
        store_set.bridge_state.upsert_playlist_link(link)?;
        self.persist_or_restore(store_set, before)?;
        Ok(())
    }

    pub(crate) fn restore_reappeared_playlist(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        link: PlaylistLink,
        remote: ServerPlaylistWriteSnapshot,
    ) -> Result<(), ServiceError> {
        if link.state != PlaylistLinkState::ServerMissing || link.server_playlist_id != *remote.id()
        {
            return Err(ServiceError::InvalidSetup);
        }
        self.observe_changed_remote_playlist(store_set, link, remote, PlaylistLinkState::Linked)
    }

    fn observe_pending_remote_playlist(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        mut link: PlaylistLink,
        pending: PendingPlaylistProjection,
        remote: ServerPlaylistWriteSnapshot,
        final_state: PlaylistLinkState,
    ) -> Result<(), ServiceError> {
        let previous = link
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| {
                LinkedPlaylistEntry::new(occurrence.entry_id.clone(), occurrence.item_id.clone())
            })
            .collect::<Vec<_>>();
        let desired = pending
            .ordered_entry_ids
            .iter()
            .cloned()
            .zip(pending.ordered_item_ids.iter().cloned())
            .map(|(entry_id, item_id)| LinkedPlaylistEntry::new(entry_id, item_id))
            .collect::<Vec<_>>();
        let current = remote
            .entries()
            .iter()
            .map(|song| song.item.item_id().clone())
            .collect::<Vec<_>>();
        let mode = pending_remote_merge_mode(pending.stage)?;
        let plan = plan_pending_remote_merge(&previous, &desired, &current, mode)
            .map_err(|_| ServiceError::InvalidSetup)?;
        let merged_name = pending_remote_merge_name(
            &link.shadow.name,
            &pending.desired_name,
            remote.name(),
            mode,
        )
        .to_owned();
        let fingerprint = super::super::actor::playlist_snapshot_fingerprint(&remote);
        let observation_revision = store_set.bridge_state.revision().saturating_add(1);

        let mut entry_by_remote = BTreeMap::new();
        for occurrence in plan.remote_occurrences() {
            let (remote_index, entry_id) = match occurrence {
                PendingRemoteOccurrence::Existing(existing) => {
                    (existing.remote_index, existing.entry.entry_id.clone())
                }
                PendingRemoteOccurrence::RemoteOnly(remote_only) => (
                    remote_only.index,
                    remote_entry_id(
                        &link.local_playlist_id,
                        &fingerprint,
                        observation_revision,
                        remote_only.index,
                        &remote_only.item_id,
                    )?,
                ),
            };
            entry_by_remote.insert(remote_index, entry_id);
        }
        let remote_entry_ids = (0..current.len())
            .map(|index| {
                entry_by_remote
                    .get(&index)
                    .cloned()
                    .ok_or(ServiceError::InvalidSetup)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut remote_position_in_merged = BTreeMap::new();
        let merged_entry_ids = plan
            .ordered_occurrences()
            .iter()
            .enumerate()
            .map(|(merged_index, occurrence)| match occurrence {
                PendingMergeOccurrence::Existing(existing) => Ok(existing.entry.entry_id.clone()),
                PendingMergeOccurrence::RemoteOnly(remote_only) => {
                    remote_position_in_merged.insert(remote_only.index, merged_index);
                    entry_by_remote
                        .get(&remote_only.index)
                        .cloned()
                        .ok_or(ServiceError::InvalidSetup)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let observed_at_unix = crate::signals::unix_now();
        let mut operations = Vec::new();
        if pending.desired_name != merged_name {
            operations.push(Operation::UpsertPlaylist {
                playlist_id: link.local_playlist_id.clone(),
                name: merged_name.clone(),
            });
        }
        operations.extend(plan.removed_existing().iter().map(|removed| {
            Operation::RemovePlaylistEntry {
                playlist_id: link.local_playlist_id.clone(),
                entry_id: removed.entry.entry_id.clone(),
                removed: true,
            }
        }));
        for occurrence in plan.remote_occurrences() {
            let PendingRemoteOccurrence::RemoteOnly(remote_only) = occurrence else {
                continue;
            };
            let merged_index = *remote_position_in_merged
                .get(&remote_only.index)
                .ok_or(ServiceError::InvalidSetup)?;
            let song = remote
                .entries()
                .get(remote_only.index)
                .ok_or(ServiceError::InvalidSetup)?;
            operations.push(Operation::UpsertPlaylistEntry {
                playlist_id: link.local_playlist_id.clone(),
                entry_id: entry_by_remote
                    .get(&remote_only.index)
                    .cloned()
                    .ok_or(ServiceError::InvalidSetup)?,
                track: portable_server_track(song),
                after_entry_id: merged_index
                    .checked_sub(1)
                    .and_then(|index| merged_entry_ids.get(index).cloned()),
            });
        }
        let retained_existing = plan
            .ordered_occurrences()
            .iter()
            .filter_map(|occurrence| match occurrence {
                PendingMergeOccurrence::Existing(existing) => Some(existing.entry.entry_id.clone()),
                PendingMergeOccurrence::RemoteOnly(_) => None,
            })
            .collect::<BTreeSet<_>>();
        let mut current_after = BTreeMap::new();
        let mut previous_retained = None;
        for entry_id in &pending.ordered_entry_ids {
            if retained_existing.contains(entry_id) {
                current_after.insert(entry_id.clone(), previous_retained.clone());
                previous_retained = Some(entry_id.clone());
            }
        }
        for (index, occurrence) in plan.ordered_occurrences().iter().enumerate() {
            let PendingMergeOccurrence::Existing(existing) = occurrence else {
                continue;
            };
            let expected_after = index
                .checked_sub(1)
                .and_then(|previous| merged_entry_ids.get(previous).cloned());
            if current_after.get(&existing.entry.entry_id) == Some(&expected_after) {
                continue;
            }
            operations.push(Operation::MovePlaylistEntry {
                playlist_id: link.local_playlist_id.clone(),
                entry_id: existing.entry.entry_id.clone(),
                after_entry_id: expected_after,
            });
        }
        let inputs = operations
            .into_iter()
            .enumerate()
            .map(|(index, operation)| ExternalOperationInput {
                acknowledgement_id: remote_operation_id(
                    &link,
                    &fingerprint,
                    observation_revision,
                    index,
                ),
                operation,
                recorded_at_unix: observed_at_unix,
            })
            .collect::<Vec<_>>();

        let playlist_id = link.local_playlist_id.clone();
        let merged_differs_from_remote = merged_entry_ids != remote_entry_ids
            || plan.desired_remote() != current.as_slice()
            || merged_name != remote.name();
        let follow_up = merged_differs_from_remote.then(|| PendingPlaylistProjection {
            desired_name: merged_name,
            ordered_entry_ids: merged_entry_ids,
            ordered_item_ids: plan.desired_remote().to_vec(),
            stage: PendingPlaylistProjectionStage::Queued,
            base_remote_fingerprint: fingerprint.clone(),
        });
        let exact_remote_shadow = remote_entry_ids
            .into_iter()
            .zip(current)
            .map(|(entry_id, item_id)| PlaylistShadowOccurrence { entry_id, item_id })
            .collect();
        let before = store_set.bridge_state.clone();
        let mutation = (|| {
            if !inputs.is_empty() {
                store_set
                    .bridge_state
                    .queue_playlist_import(PendingPlaylistImportBatch {
                        operation_id: format!(
                            "playlist-remote-batch-{}",
                            digest_text(&format!(
                                "{}\0{fingerprint}\0{observation_revision}",
                                playlist_id.as_str(),
                            ))
                        ),
                        local_playlist_id: playlist_id.clone(),
                        purpose: PendingPlaylistImportPurpose::RemoteObservation,
                        operations: inputs,
                    })?;
            }
            store_set
                .bridge_state
                .remove_playlist_projection(&playlist_id);
            if let Some(follow_up) = follow_up {
                store_set
                    .bridge_state
                    .queue_playlist_projection(playlist_id.clone(), follow_up)?;
            }
            link.state = final_state;
            link.shadow = PlaylistShadow {
                name: remote.name().to_owned(),
                occurrences: exact_remote_shadow,
                verified_at_unix: observed_at_unix,
            };
            store_set.bridge_state.upsert_playlist_link(link)?;
            Ok::<(), BridgeMutationError>(())
        })();
        if let Err(error) = mutation {
            store_set.bridge_state = before;
            return Err(error.into());
        }
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }

    fn observe_changed_remote_playlist(
        &self,
        store_set: &mut OpenSubsonicStoreSet,
        mut link: PlaylistLink,
        remote: ServerPlaylistWriteSnapshot,
        final_state: PlaylistLinkState,
    ) -> Result<(), ServiceError> {
        let previous = link
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| {
                LinkedPlaylistEntry::new(occurrence.entry_id.clone(), occurrence.item_id.clone())
            })
            .collect::<Vec<_>>();
        let current = remote
            .entries()
            .iter()
            .map(|song| song.item.item_id().clone())
            .collect::<Vec<_>>();
        let delta =
            plan_remote_delta(&previous, &current).map_err(|_| ServiceError::InvalidSetup)?;
        let fingerprint = super::super::actor::playlist_snapshot_fingerprint(&remote);
        // A server can move from A → B → A more than once. The current fingerprint alone would
        // reuse acknowledgement IDs on the second transition back to A, causing the personal
        // ledger to deduplicate a real later observation. The bridge revision is the durable
        // observation sequence: retries before a commit reuse it, while every committed remote
        // transition advances it.
        let observation_revision = store_set.bridge_state.revision().saturating_add(1);
        let mut entry_by_remote = delta
            .retained
            .iter()
            .map(|matched| (matched.remote_index, matched.entry_id.clone()))
            .collect::<BTreeMap<_, _>>();
        for added in &delta.added {
            entry_by_remote.insert(
                added.index,
                remote_entry_id(
                    &link.local_playlist_id,
                    &fingerprint,
                    observation_revision,
                    added.index,
                    &added.item_id,
                )?,
            );
        }
        let ordered_entry_ids = (0..current.len())
            .map(|index| {
                entry_by_remote
                    .get(&index)
                    .cloned()
                    .ok_or(ServiceError::InvalidSetup)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let observed_at_unix = crate::signals::unix_now();
        let mut operations = Vec::new();
        if link.shadow.name != remote.name() {
            operations.push(Operation::UpsertPlaylist {
                playlist_id: link.local_playlist_id.clone(),
                name: remote.name().to_owned(),
            });
        }
        operations.extend(
            delta
                .removed
                .iter()
                .map(|removed| Operation::RemovePlaylistEntry {
                    playlist_id: link.local_playlist_id.clone(),
                    entry_id: removed.entry.entry_id.clone(),
                    removed: true,
                }),
        );
        for added in &delta.added {
            let song = remote
                .entries()
                .get(added.index)
                .ok_or(ServiceError::InvalidSetup)?;
            operations.push(Operation::UpsertPlaylistEntry {
                playlist_id: link.local_playlist_id.clone(),
                entry_id: entry_by_remote
                    .get(&added.index)
                    .cloned()
                    .ok_or(ServiceError::InvalidSetup)?,
                track: portable_server_track(song),
                after_entry_id: added
                    .index
                    .checked_sub(1)
                    .and_then(|index| entry_by_remote.get(&index).cloned()),
            });
        }
        let retained_entry_ids = delta
            .retained
            .iter()
            .map(|matched| matched.entry_id.clone())
            .collect::<BTreeSet<_>>();
        for (index, entry_id) in ordered_entry_ids.iter().enumerate() {
            if !retained_entry_ids.contains(entry_id) {
                continue;
            }
            operations.push(Operation::MovePlaylistEntry {
                playlist_id: link.local_playlist_id.clone(),
                entry_id: entry_id.clone(),
                after_entry_id: index
                    .checked_sub(1)
                    .and_then(|previous| ordered_entry_ids.get(previous).cloned()),
            });
        }
        let inputs = operations
            .into_iter()
            .enumerate()
            .map(|(index, operation)| ExternalOperationInput {
                acknowledgement_id: remote_operation_id(
                    &link,
                    &fingerprint,
                    observation_revision,
                    index,
                ),
                operation,
                recorded_at_unix: observed_at_unix,
            })
            .collect::<Vec<_>>();

        let before = store_set.bridge_state.clone();
        if !inputs.is_empty() {
            store_set
                .bridge_state
                .queue_playlist_import(PendingPlaylistImportBatch {
                    operation_id: format!(
                        "playlist-remote-batch-{}",
                        digest_text(&format!(
                            "{}\0{fingerprint}\0{observation_revision}",
                            link.local_playlist_id.as_str(),
                        ))
                    ),
                    local_playlist_id: link.local_playlist_id.clone(),
                    purpose: PendingPlaylistImportPurpose::RemoteObservation,
                    operations: inputs,
                })?;
        }
        link.state = final_state;
        link.shadow = PlaylistShadow {
            name: remote.name().to_owned(),
            occurrences: ordered_entry_ids
                .into_iter()
                .zip(current)
                .map(|(entry_id, item_id)| PlaylistShadowOccurrence { entry_id, item_id })
                .collect(),
            verified_at_unix: observed_at_unix,
        };
        store_set
            .bridge_state
            .remove_playlist_projection(&link.local_playlist_id);
        store_set.bridge_state.upsert_playlist_link(link)?;
        self.persist_or_restore(store_set, before)?;
        self.emit_pending(store_set);
        Ok(())
    }
}

fn pending_remote_merge_mode(
    stage: PendingPlaylistProjectionStage,
) -> Result<PendingRemoteMergeMode, ServiceError> {
    match stage {
        PendingPlaylistProjectionStage::Queued => Ok(PendingRemoteMergeMode::LocalNotDelivered),
        PendingPlaylistProjectionStage::Readback => Ok(PendingRemoteMergeMode::LocalDelivered),
        PendingPlaylistProjectionStage::Ambiguous => Ok(PendingRemoteMergeMode::DeliveryUnknown),
        PendingPlaylistProjectionStage::NeedsAttention => Err(ServiceError::InvalidSetup),
    }
}

fn pending_remote_merge_name<'a>(
    base: &'a str,
    desired: &'a str,
    remote: &'a str,
    mode: PendingRemoteMergeMode,
) -> &'a str {
    match mode {
        PendingRemoteMergeMode::LocalDelivered => remote,
        PendingRemoteMergeMode::LocalNotDelivered | PendingRemoteMergeMode::DeliveryUnknown => {
            if remote != base {
                remote
            } else {
                desired
            }
        }
    }
}

fn has_exact_playlist_write_access(
    remote: &ServerPlaylistWriteSnapshot,
    credential: &ServerCredential,
) -> bool {
    remote.read_only() == Some(false)
        && credential
            .username()
            .is_some_and(|username| remote.owner() == Some(username.expose_secret()))
}

fn playlist_projection_error_needs_attention(error: super::super::ServerError) -> bool {
    matches!(
        error,
        super::super::ServerError::AuthenticationRequired
            | super::super::ServerError::PermissionDenied
            | super::super::ServerError::CertificateFailed
            | super::super::ServerError::OriginRejected
            | super::super::ServerError::UnsupportedFeature
            | super::super::ServerError::InvalidResponse
            | super::super::ServerError::ResponseTooLarge
            | super::super::ServerError::WrongAccountScope
    )
}

fn playlist_readback_matches(
    pending: &PendingPlaylistProjection,
    remote: &ServerPlaylistWriteSnapshot,
) -> bool {
    remote.name() == pending.desired_name
        && remote.entries().len() == pending.ordered_item_ids.len()
        && remote
            .entries()
            .iter()
            .zip(&pending.ordered_item_ids)
            .all(|(song, expected)| song.item.item_id() == expected)
}

fn exact_local_occurrences(
    store_set: &OpenSubsonicStoreSet,
    snapshot: &PersonalPlaylistSnapshot,
) -> Result<(Vec<PlaylistEntryId>, Vec<ItemId>), ServiceError> {
    let mut entry_ids = Vec::with_capacity(snapshot.entries.len());
    let mut item_ids = Vec::with_capacity(snapshot.entries.len());
    for entry in &snapshot.entries {
        let PortableTrackKey::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } = &entry.track.key
        else {
            return Err(ServiceError::InvalidSetup);
        };
        if backend_id != store_set.profile.backend_id().as_str()
            || account_scope_id != store_set.profile.account_scope_id().as_str()
        {
            return Err(ServiceError::Server(
                super::super::ServerError::WrongAccountScope,
            ));
        }
        entry_ids.push(entry.entry_id.clone());
        item_ids.push(ItemId::new(item_id.clone()).map_err(|_| ServiceError::InvalidSetup)?);
    }
    Ok((entry_ids, item_ids))
}

fn shadow_fingerprint(store_set: &OpenSubsonicStoreSet, link: &PlaylistLink) -> String {
    sequence_fingerprint(
        store_set.profile.backend_id().as_str(),
        store_set.profile.account_scope_id().as_str(),
        link.server_playlist_id.as_str(),
        &link.shadow.name,
        link.shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.item_id.as_str()),
    )
}

fn sequence_fingerprint<'a>(
    backend_id: &str,
    account_scope_id: &str,
    playlist_id: &str,
    name: &str,
    item_ids: impl Iterator<Item = &'a str>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-playlist-snapshot-v1\0");
    for part in [backend_id, account_scope_id, playlist_id, name] {
        update_part(&mut digest, part.as_bytes());
    }
    for item_id in item_ids {
        update_part(&mut digest, item_id.as_bytes());
    }
    HEXLOWER.encode(&digest.finalize())
}

fn remote_entry_id(
    playlist_id: &PlaylistId,
    fingerprint: &str,
    observation_revision: u64,
    index: usize,
    item_id: &ItemId,
) -> Result<PlaylistEntryId, ServiceError> {
    PlaylistEntryId::new(format!(
        "server-entry-{}",
        digest_text(&format!(
            "{}\0{fingerprint}\0{observation_revision}\0{index}\0{}",
            playlist_id.as_str(),
            item_id.as_str()
        ))
    ))
    .map_err(|_| ServiceError::InvalidSetup)
}

fn remote_operation_id(
    link: &PlaylistLink,
    fingerprint: &str,
    observation_revision: u64,
    index: usize,
) -> String {
    digest_text(&format!(
        "{}\0{}\0{fingerprint}\0{observation_revision}\0{index}",
        link.local_playlist_id.as_str(),
        link.server_playlist_id.as_str()
    ))
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
