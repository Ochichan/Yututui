//! Bounded durable state for explicitly linked OpenSubsonic playlists.

#![allow(
    dead_code,
    reason = "the linked-playlist owner runtime consumes this durable API in the next integration step"
)]

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{BridgeMutationError, OpenSubsonicBridgeState};
use crate::open_subsonic::model::{ItemId, ServerPlaylistId};
use crate::personal_state::{
    ExternalOperationInput, Operation, PlaylistEntryId, PlaylistId, PortableTrackKey,
};

pub(super) const MAX_PLAYLIST_LINKS: usize = 999;
pub(super) const MAX_PENDING_PLAYLIST_IMPORTS: usize = 999;
pub(super) const MAX_PENDING_PLAYLIST_PROJECTIONS: usize = 999;
pub(super) const MAX_PENDING_PLAYLIST_CREATES: usize = 999;
pub(super) const MAX_PLAYLIST_OCCURRENCES: usize = 999;

const MAX_PLAYLIST_IMPORT_OPERATIONS: usize = MAX_PLAYLIST_OCCURRENCES * 2 + 2;
const MAX_PLAYLIST_ID_CHARS: usize = 512;
const MAX_PLAYLIST_OPERATION_ID_CHARS: usize = 256;
const MAX_REMOTE_FINGERPRINT_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlaylistLinkState {
    Linked,
    /// The remote row no longer proves exact ownership and writable access.
    ///
    /// The link and any local projection remain durable, but automatic reads and writes stay
    /// dormant until an explicit same-account setup refresh requeues access verification.
    AccessNeedsAttention,
    ServerMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaylistShadowOccurrence {
    pub entry_id: PlaylistEntryId,
    pub item_id: ItemId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaylistShadow {
    pub name: String,
    pub occurrences: Vec<PlaylistShadowOccurrence>,
    pub verified_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaylistLink {
    pub local_playlist_id: PlaylistId,
    pub server_playlist_id: ServerPlaylistId,
    pub managed_by_yututui: bool,
    pub state: PlaylistLinkState,
    /// Independent of remote access/missing state: incompatible local content is resolved only by
    /// a later compatible local snapshot, never by reconnect/setup.
    #[serde(default)]
    pub content_needs_attention: bool,
    pub shadow: PlaylistShadow,
}

/// One all-or-nothing batch waiting to cross the personal-state owner boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingPlaylistImportPurpose {
    /// The deletion-free initial merge used by both import-copy and explicit link setup.
    InitialOrImportCopy,
    /// A later exact server snapshot observed for an existing explicit link.
    RemoteObservation,
    /// An explicit local deletion after the corresponding server lifecycle choice.
    Delete,
}

/// One all-or-nothing batch waiting to cross the personal-state owner boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingPlaylistImportBatch {
    /// Durable acknowledgement for the batch itself. The contained operations retain their own
    /// deterministic acknowledgement IDs so a partially applied crash replay remains idempotent.
    pub operation_id: String,
    /// Explicit causal target; never infer queue ordering from the opaque operation ID.
    pub local_playlist_id: PlaylistId,
    pub purpose: PendingPlaylistImportPurpose,
    pub operations: Vec<ExternalOperationInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingPlaylistProjectionStage {
    Queued,
    /// The write may have reached the server. Never resend it before a bounded readback.
    Ambiguous,
    Readback,
    /// A deterministic policy or permission failure requires user action. Automatic delivery must
    /// remain dormant until the link is explicitly resolved or a later setup refresh requeues it.
    NeedsAttention,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingPlaylistProjection {
    pub desired_name: String,
    pub ordered_entry_ids: Vec<PlaylistEntryId>,
    pub ordered_item_ids: Vec<ItemId>,
    pub stage: PendingPlaylistProjectionStage,
    pub base_remote_fingerprint: String,
}

/// A durable non-idempotent create intent.
///
/// This record crosses the storage boundary before `createPlaylist` is sent. If the response is
/// lost, its absence of a confirmed server ID deliberately blocks another create instead of
/// risking a duplicate server playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingPlaylistCreate {
    pub local_playlist_id: PlaylistId,
    pub expected_missing_server_id: Option<ServerPlaylistId>,
    pub created_server_playlist_id: Option<ServerPlaylistId>,
    pub desired_name: String,
    pub ordered_entry_ids: Vec<PlaylistEntryId>,
    pub ordered_item_ids: Vec<ItemId>,
    pub started_at_unix: i64,
}

impl OpenSubsonicBridgeState {
    pub(crate) fn playlist_links(&self) -> &BTreeMap<PlaylistId, PlaylistLink> {
        &self.playlist_links
    }

    pub(crate) fn playlist_link(&self, playlist_id: &PlaylistId) -> Option<&PlaylistLink> {
        self.playlist_links.get(playlist_id)
    }

    /// Insert a new explicit link or refresh the shadow for the same exact local/server pair.
    ///
    /// Reusing either side for a different pair is rejected; equal server names are irrelevant.
    pub(crate) fn upsert_playlist_link(
        &mut self,
        link: PlaylistLink,
    ) -> Result<(), BridgeMutationError> {
        validate_playlist_link(&link)?;
        if let Some(existing) = self.playlist_links.get(&link.local_playlist_id)
            && existing.server_playlist_id != link.server_playlist_id
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        if self.playlist_links.iter().any(|(local_id, existing)| {
            local_id != &link.local_playlist_id
                && existing.server_playlist_id == link.server_playlist_id
        }) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        if !self.playlist_links.contains_key(&link.local_playlist_id)
            && self.playlist_links.len() >= MAX_PLAYLIST_LINKS
        {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.playlist_links
            .insert(link.local_playlist_id.clone(), link);
        Ok(())
    }

    pub(crate) fn remove_playlist_link(
        &mut self,
        playlist_id: &PlaylistId,
    ) -> Option<PlaylistLink> {
        self.playlist_links.remove(playlist_id)
    }

    pub(crate) fn pending_playlist_imports(&self) -> &BTreeMap<String, PendingPlaylistImportBatch> {
        &self.pending_playlist_imports
    }

    pub(crate) fn pending_playlist_import(
        &self,
        playlist_id: &PlaylistId,
    ) -> Option<&PendingPlaylistImportBatch> {
        self.pending_playlist_imports
            .values()
            .find(|pending| &pending.local_playlist_id == playlist_id)
    }

    pub(crate) fn queue_playlist_import(
        &mut self,
        pending: PendingPlaylistImportBatch,
    ) -> Result<(), BridgeMutationError> {
        self.validate_pending_playlist_import(&pending)?;
        if let Some(existing) = self.pending_playlist_imports.get(&pending.operation_id) {
            return if existing == &pending {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if self
            .pending_playlist_imports
            .values()
            .any(|existing| existing.local_playlist_id == pending.local_playlist_id)
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        let incoming_acknowledgements = pending
            .operations
            .iter()
            .map(|operation| operation.acknowledgement_id.as_str())
            .collect::<BTreeSet<_>>();
        if self.pending_playlist_imports.values().any(|existing| {
            existing.operations.iter().any(|operation| {
                incoming_acknowledgements.contains(operation.acknowledgement_id.as_str())
            })
        }) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        if self.pending_playlist_imports.len() >= MAX_PENDING_PLAYLIST_IMPORTS {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.pending_playlist_imports
            .insert(pending.operation_id.clone(), pending);
        Ok(())
    }

    pub(crate) fn remove_playlist_import(
        &mut self,
        operation_id: &str,
    ) -> Option<PendingPlaylistImportBatch> {
        self.pending_playlist_imports.remove(operation_id)
    }

    pub(crate) fn retire_playlist_import(
        &mut self,
        playlist_id: &PlaylistId,
    ) -> Option<PendingPlaylistImportBatch> {
        let operation_id =
            self.pending_playlist_imports
                .iter()
                .find_map(|(operation_id, pending)| {
                    (&pending.local_playlist_id == playlist_id).then(|| operation_id.clone())
                })?;
        self.pending_playlist_imports.remove(&operation_id)
    }

    pub(crate) fn pending_playlist_projections(
        &self,
    ) -> &BTreeMap<PlaylistId, PendingPlaylistProjection> {
        &self.pending_playlist_projections
    }

    pub(crate) fn playlist_projections_needing_attention(&self) -> usize {
        let server_missing_ids = self
            .playlist_links
            .iter()
            .filter(|(_, link)| link.state == PlaylistLinkState::ServerMissing)
            .map(|(playlist_id, _)| playlist_id.clone())
            .collect::<BTreeSet<_>>();
        let mut playlist_ids = BTreeSet::new();
        playlist_ids.extend(
            self.playlist_links
                .iter()
                .filter(|(_, link)| link.state == PlaylistLinkState::AccessNeedsAttention)
                .map(|(playlist_id, _)| playlist_id.clone()),
        );
        playlist_ids.extend(
            self.pending_playlist_projections
                .iter()
                .filter(|(playlist_id, pending)| {
                    pending.stage == PendingPlaylistProjectionStage::NeedsAttention
                        && !server_missing_ids.contains(*playlist_id)
                })
                .map(|(playlist_id, _)| playlist_id.clone()),
        );
        playlist_ids.len()
    }

    pub(crate) fn playlist_contents_needing_attention(&self) -> usize {
        self.playlist_links
            .values()
            .filter(|link| link.content_needs_attention)
            .count()
    }

    /// Server-side deletions need an explicit keep/restore decision, not connection repair.
    /// An in-progress restore is already represented by create recovery and is not counted twice.
    pub(crate) fn playlist_links_needing_decision(&self) -> usize {
        self.playlist_links
            .iter()
            .filter(|(playlist_id, link)| {
                link.state == PlaylistLinkState::ServerMissing
                    && !self.pending_playlist_creates.contains_key(*playlist_id)
            })
            .count()
    }

    pub(crate) fn requeue_playlist_projections_needing_attention(&mut self) -> usize {
        let mut requeued = BTreeSet::new();
        for (playlist_id, link) in &mut self.playlist_links {
            if link.state == PlaylistLinkState::AccessNeedsAttention {
                link.state = PlaylistLinkState::Linked;
                requeued.insert(playlist_id.clone());
            }
        }
        for (playlist_id, pending) in &mut self.pending_playlist_projections {
            if pending.stage == PendingPlaylistProjectionStage::NeedsAttention {
                pending.stage = PendingPlaylistProjectionStage::Queued;
                requeued.insert(playlist_id.clone());
            }
        }
        requeued.len()
    }

    pub(crate) fn queue_playlist_projection(
        &mut self,
        playlist_id: PlaylistId,
        pending: PendingPlaylistProjection,
    ) -> Result<(), BridgeMutationError> {
        validate_playlist_id(&playlist_id)?;
        validate_pending_playlist_projection(&pending)?;
        if let Some(existing) = self.pending_playlist_projections.get(&playlist_id) {
            return if existing == &pending {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if self.pending_playlist_projections.len() >= MAX_PENDING_PLAYLIST_PROJECTIONS {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.pending_playlist_projections
            .insert(playlist_id, pending);
        Ok(())
    }

    /// Advance delivery state without allowing the desired projection or its comparison base to
    /// change underneath an ambiguous write.
    pub(crate) fn replace_playlist_projection(
        &mut self,
        playlist_id: &PlaylistId,
        pending: PendingPlaylistProjection,
    ) -> Result<(), BridgeMutationError> {
        validate_playlist_id(playlist_id)?;
        validate_pending_playlist_projection(&pending)?;
        let existing = self
            .pending_playlist_projections
            .get(playlist_id)
            .ok_or(BridgeMutationError::ConflictingEntry)?;
        if existing.desired_name != pending.desired_name
            || existing.ordered_entry_ids != pending.ordered_entry_ids
            || existing.ordered_item_ids != pending.ordered_item_ids
            || existing.base_remote_fingerprint != pending.base_remote_fingerprint
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        self.pending_playlist_projections
            .insert(playlist_id.clone(), pending);
        Ok(())
    }

    pub(crate) fn remove_playlist_projection(
        &mut self,
        playlist_id: &PlaylistId,
    ) -> Option<PendingPlaylistProjection> {
        self.pending_playlist_projections.remove(playlist_id)
    }

    pub(crate) fn pending_playlist_creates(&self) -> &BTreeMap<PlaylistId, PendingPlaylistCreate> {
        &self.pending_playlist_creates
    }

    pub(crate) fn queue_playlist_create(
        &mut self,
        pending: PendingPlaylistCreate,
    ) -> Result<(), BridgeMutationError> {
        validate_pending_playlist_create(self, &pending)?;
        if let Some(existing) = self
            .pending_playlist_creates
            .get(&pending.local_playlist_id)
        {
            return if existing == &pending {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        if self.pending_playlist_creates.len() >= MAX_PENDING_PLAYLIST_CREATES {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.pending_playlist_creates
            .insert(pending.local_playlist_id.clone(), pending);
        Ok(())
    }

    pub(crate) fn record_playlist_create_server_id(
        &mut self,
        playlist_id: &PlaylistId,
        server_playlist_id: ServerPlaylistId,
    ) -> Result<(), BridgeMutationError> {
        if self.playlist_links.values().any(|link| {
            link.local_playlist_id != *playlist_id && link.server_playlist_id == server_playlist_id
        }) || self
            .pending_playlist_creates
            .iter()
            .any(|(other_id, pending)| {
                other_id != playlist_id
                    && pending.created_server_playlist_id.as_ref() == Some(&server_playlist_id)
            })
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        let pending = self
            .pending_playlist_creates
            .get_mut(playlist_id)
            .ok_or(BridgeMutationError::ConflictingEntry)?;
        if let Some(existing) = &pending.created_server_playlist_id {
            return if existing == &server_playlist_id {
                Ok(())
            } else {
                Err(BridgeMutationError::ConflictingEntry)
            };
        }
        pending.created_server_playlist_id = Some(server_playlist_id);
        Ok(())
    }

    pub(crate) fn remove_playlist_create(
        &mut self,
        playlist_id: &PlaylistId,
    ) -> Option<PendingPlaylistCreate> {
        self.pending_playlist_creates.remove(playlist_id)
    }

    pub(super) fn validate_playlist_state(&self) -> Result<(), BridgeMutationError> {
        if self.playlist_links.len() > MAX_PLAYLIST_LINKS
            || self.pending_playlist_imports.len() > MAX_PENDING_PLAYLIST_IMPORTS
            || self.pending_playlist_projections.len() > MAX_PENDING_PLAYLIST_PROJECTIONS
            || self.pending_playlist_creates.len() > MAX_PENDING_PLAYLIST_CREATES
        {
            return Err(BridgeMutationError::CapacityExceeded);
        }

        let mut server_playlist_ids = BTreeSet::new();
        for (local_playlist_id, link) in &self.playlist_links {
            validate_playlist_id(local_playlist_id)?;
            validate_playlist_link(link)?;
            if local_playlist_id != &link.local_playlist_id
                || !server_playlist_ids.insert(&link.server_playlist_id)
            {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }

        let mut acknowledgement_ids = BTreeSet::new();
        let mut pending_playlist_ids = BTreeSet::new();
        for (operation_id, pending) in &self.pending_playlist_imports {
            validate_playlist_identifier(operation_id, MAX_PLAYLIST_OPERATION_ID_CHARS)?;
            self.validate_pending_playlist_import(pending)?;
            if operation_id != &pending.operation_id {
                return Err(BridgeMutationError::ConflictingEntry);
            }
            if !pending_playlist_ids.insert(&pending.local_playlist_id) {
                return Err(BridgeMutationError::ConflictingEntry);
            }
            if pending.purpose == PendingPlaylistImportPurpose::Delete
                && self.playlist_links.contains_key(&pending.local_playlist_id)
            {
                return Err(BridgeMutationError::ConflictingEntry);
            }
            for operation in &pending.operations {
                if !acknowledgement_ids.insert(operation.acknowledgement_id.as_str()) {
                    return Err(BridgeMutationError::ConflictingEntry);
                }
            }
        }

        for (playlist_id, pending) in &self.pending_playlist_projections {
            validate_playlist_id(playlist_id)?;
            validate_pending_playlist_projection(pending)?;
            if !self.playlist_links.contains_key(playlist_id) {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }
        let mut pending_server_ids = BTreeSet::new();
        for (playlist_id, pending) in &self.pending_playlist_creates {
            validate_playlist_id(playlist_id)?;
            validate_pending_playlist_create(self, pending)?;
            if playlist_id != &pending.local_playlist_id
                || pending
                    .created_server_playlist_id
                    .as_ref()
                    .is_some_and(|id| !pending_server_ids.insert(id))
            {
                return Err(BridgeMutationError::ConflictingEntry);
            }
        }
        Ok(())
    }

    fn validate_pending_playlist_import(
        &self,
        pending: &PendingPlaylistImportBatch,
    ) -> Result<(), BridgeMutationError> {
        validate_playlist_identifier(&pending.operation_id, MAX_PLAYLIST_OPERATION_ID_CHARS)?;
        validate_playlist_id(&pending.local_playlist_id)?;
        if pending.operations.is_empty() {
            return Err(BridgeMutationError::InvalidEntry);
        }
        if pending.operations.len() > MAX_PLAYLIST_IMPORT_OPERATIONS {
            return Err(BridgeMutationError::CapacityExceeded);
        }

        let mut acknowledgement_ids = BTreeSet::new();
        let mut playlist_id = None::<&str>;
        let mut inserted_occurrences = 0_usize;
        let mut playlist_upserts = 0_usize;
        for input in &pending.operations {
            validate_playlist_identifier(
                &input.acknowledgement_id,
                MAX_PLAYLIST_OPERATION_ID_CHARS,
            )?;
            if !acknowledgement_ids.insert(input.acknowledgement_id.as_str()) {
                return Err(BridgeMutationError::ConflictingEntry);
            }
            let operation_playlist_id =
                self.validate_playlist_import_operation(&input.operation)?;
            if operation_playlist_id != pending.local_playlist_id.as_str()
                || playlist_id
                    .replace(operation_playlist_id)
                    .is_some_and(|existing| existing != operation_playlist_id)
            {
                return Err(BridgeMutationError::InvalidEntry);
            }
            let purpose_matches = match pending.purpose {
                PendingPlaylistImportPurpose::InitialOrImportCopy => matches!(
                    input.operation,
                    Operation::UpsertPlaylist { .. } | Operation::UpsertPlaylistEntry { .. }
                ),
                PendingPlaylistImportPurpose::RemoteObservation => matches!(
                    input.operation,
                    Operation::UpsertPlaylist { .. }
                        | Operation::UpsertPlaylistEntry { .. }
                        | Operation::MovePlaylistEntry { .. }
                        | Operation::RemovePlaylistEntry { .. }
                ),
                PendingPlaylistImportPurpose::Delete => matches!(
                    input.operation,
                    Operation::DeletePlaylist { deleted: true, .. }
                ),
            };
            if !purpose_matches {
                return Err(BridgeMutationError::InvalidEntry);
            }
            if matches!(input.operation, Operation::UpsertPlaylistEntry { .. }) {
                inserted_occurrences += 1;
            }
            if matches!(input.operation, Operation::UpsertPlaylist { .. }) {
                playlist_upserts += 1;
            }
        }
        if pending.purpose == PendingPlaylistImportPurpose::Delete && pending.operations.len() != 1
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        if pending.purpose == PendingPlaylistImportPurpose::InitialOrImportCopy
            && (playlist_upserts != 1
                || !matches!(
                    pending.operations.first().map(|input| &input.operation),
                    Some(Operation::UpsertPlaylist { .. })
                ))
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        if pending.purpose == PendingPlaylistImportPurpose::RemoteObservation
            && playlist_upserts > 1
        {
            return Err(BridgeMutationError::InvalidEntry);
        }
        if inserted_occurrences > MAX_PLAYLIST_OCCURRENCES {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        Ok(())
    }

    fn validate_playlist_import_operation<'a>(
        &self,
        operation: &'a Operation,
    ) -> Result<&'a str, BridgeMutationError> {
        match operation {
            Operation::UpsertPlaylist { playlist_id, name } => {
                validate_playlist_id(playlist_id)?;
                super::validate_portable_text(name)?;
                Ok(playlist_id.as_str())
            }
            Operation::DeletePlaylist { playlist_id, .. } => {
                validate_playlist_id(playlist_id)?;
                Ok(playlist_id.as_str())
            }
            Operation::UpsertPlaylistEntry {
                playlist_id,
                entry_id,
                track,
                after_entry_id,
            } => {
                validate_playlist_entry_ids(playlist_id, entry_id, after_entry_id.as_ref())?;
                track
                    .validate()
                    .map_err(|_| BridgeMutationError::InvalidEntry)?;
                let PortableTrackKey::OpenSubsonic {
                    backend_id,
                    account_scope_id,
                    item_id,
                } = &track.key
                else {
                    return Err(BridgeMutationError::InvalidEntry);
                };
                if backend_id != self.backend_id.as_str()
                    || account_scope_id != self.account_scope_id.as_str()
                    || ItemId::new(item_id.clone()).is_err()
                {
                    return Err(BridgeMutationError::InvalidEntry);
                }
                Ok(playlist_id.as_str())
            }
            Operation::MovePlaylistEntry {
                playlist_id,
                entry_id,
                after_entry_id,
            } => {
                validate_playlist_entry_ids(playlist_id, entry_id, after_entry_id.as_ref())?;
                Ok(playlist_id.as_str())
            }
            Operation::RemovePlaylistEntry {
                playlist_id,
                entry_id,
                ..
            } => {
                validate_playlist_entry_ids(playlist_id, entry_id, None)?;
                Ok(playlist_id.as_str())
            }
            _ => Err(BridgeMutationError::InvalidEntry),
        }
    }
}

fn validate_playlist_link(link: &PlaylistLink) -> Result<(), BridgeMutationError> {
    validate_playlist_id(&link.local_playlist_id)?;
    super::validate_portable_text(&link.shadow.name)?;
    if link.shadow.occurrences.len() > MAX_PLAYLIST_OCCURRENCES {
        return Err(BridgeMutationError::CapacityExceeded);
    }
    let mut entry_ids = BTreeSet::new();
    for occurrence in &link.shadow.occurrences {
        validate_playlist_entry_id(&occurrence.entry_id)?;
        if !entry_ids.insert(&occurrence.entry_id) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
    }
    Ok(())
}

fn validate_pending_playlist_projection(
    pending: &PendingPlaylistProjection,
) -> Result<(), BridgeMutationError> {
    super::validate_portable_text(&pending.desired_name)?;
    validate_playlist_identifier(
        &pending.base_remote_fingerprint,
        MAX_REMOTE_FINGERPRINT_CHARS,
    )?;
    if pending.ordered_entry_ids.len() != pending.ordered_item_ids.len() {
        return Err(BridgeMutationError::InvalidEntry);
    }
    if pending.ordered_item_ids.len() > MAX_PLAYLIST_OCCURRENCES {
        return Err(BridgeMutationError::CapacityExceeded);
    }
    let mut entry_ids = BTreeSet::new();
    for entry_id in &pending.ordered_entry_ids {
        validate_playlist_entry_id(entry_id)?;
        if !entry_ids.insert(entry_id) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
    }
    Ok(())
}

fn validate_pending_playlist_create(
    state: &OpenSubsonicBridgeState,
    pending: &PendingPlaylistCreate,
) -> Result<(), BridgeMutationError> {
    validate_playlist_id(&pending.local_playlist_id)?;
    super::validate_portable_text(&pending.desired_name)?;
    if pending.ordered_entry_ids.len() != pending.ordered_item_ids.len() {
        return Err(BridgeMutationError::InvalidEntry);
    }
    if pending.ordered_item_ids.len() > MAX_PLAYLIST_OCCURRENCES {
        return Err(BridgeMutationError::CapacityExceeded);
    }
    let mut entry_ids = BTreeSet::new();
    for entry_id in &pending.ordered_entry_ids {
        validate_playlist_entry_id(entry_id)?;
        if !entry_ids.insert(entry_id) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
    }
    match (
        state.playlist_links.get(&pending.local_playlist_id),
        pending.expected_missing_server_id.as_ref(),
    ) {
        (None, None) => {}
        (Some(link), Some(expected))
            if link.state == PlaylistLinkState::ServerMissing
                && &link.server_playlist_id == expected => {}
        _ => return Err(BridgeMutationError::ConflictingEntry),
    }
    if let Some(created) = &pending.created_server_playlist_id
        && state.playlist_links.values().any(|link| {
            link.local_playlist_id != pending.local_playlist_id
                && &link.server_playlist_id == created
        })
    {
        return Err(BridgeMutationError::ConflictingEntry);
    }
    Ok(())
}

fn validate_playlist_entry_ids(
    playlist_id: &PlaylistId,
    entry_id: &PlaylistEntryId,
    after_entry_id: Option<&PlaylistEntryId>,
) -> Result<(), BridgeMutationError> {
    validate_playlist_id(playlist_id)?;
    validate_playlist_entry_id(entry_id)?;
    if let Some(after_entry_id) = after_entry_id {
        validate_playlist_entry_id(after_entry_id)?;
        if after_entry_id == entry_id {
            return Err(BridgeMutationError::InvalidEntry);
        }
    }
    Ok(())
}

fn validate_playlist_id(playlist_id: &PlaylistId) -> Result<(), BridgeMutationError> {
    validate_playlist_identifier(playlist_id.as_str(), MAX_PLAYLIST_ID_CHARS)
}

fn validate_playlist_entry_id(entry_id: &PlaylistEntryId) -> Result<(), BridgeMutationError> {
    validate_playlist_identifier(entry_id.as_str(), MAX_PLAYLIST_ID_CHARS)
}

fn validate_playlist_identifier(value: &str, max_chars: usize) -> Result<(), BridgeMutationError> {
    if value.trim().is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(forbidden_playlist_character)
    {
        return Err(BridgeMutationError::InvalidEntry);
    }
    Ok(())
}

fn forbidden_playlist_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::bridge_store::{decode_bridge, encode_bridge};
    use crate::open_subsonic::model::{AccountScopeId, BackendId};
    use crate::personal_state::{PortableTrack, PortableTrackKey};

    fn bridge() -> OpenSubsonicBridgeState {
        OpenSubsonicBridgeState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
        )
    }

    fn local_id(value: impl Into<String>) -> PlaylistId {
        PlaylistId::new(value).unwrap()
    }

    fn entry_id(value: impl Into<String>) -> PlaylistEntryId {
        PlaylistEntryId::new(value).unwrap()
    }

    fn item_id(value: impl Into<String>) -> ItemId {
        ItemId::new(value).unwrap()
    }

    fn shadow(name: &str, occurrence_count: usize) -> PlaylistShadow {
        PlaylistShadow {
            name: name.to_owned(),
            occurrences: (0..occurrence_count)
                .map(|index| PlaylistShadowOccurrence {
                    entry_id: entry_id(format!("entry-{index}")),
                    item_id: item_id(format!("song-{index}")),
                })
                .collect(),
            verified_at_unix: 42,
        }
    }

    fn link(local: &str, server: &str) -> PlaylistLink {
        PlaylistLink {
            local_playlist_id: local_id(local),
            server_playlist_id: ServerPlaylistId::new(server).unwrap(),
            managed_by_yututui: false,
            state: PlaylistLinkState::Linked,
            content_needs_attention: false,
            shadow: shadow("Remote playlist", 2),
        }
    }

    fn portable(item: &str, backend: &str, account: &str) -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: backend.to_owned(),
                account_scope_id: account.to_owned(),
                item_id: item.to_owned(),
            },
            title: "Title".to_owned(),
            artist: "Artist".to_owned(),
            album: None,
            duration_secs: Some(180),
            isrc: None,
        }
    }

    fn import_batch(operation_id: &str, playlist_id: &str) -> PendingPlaylistImportBatch {
        PendingPlaylistImportBatch {
            operation_id: operation_id.to_owned(),
            local_playlist_id: local_id(playlist_id),
            purpose: PendingPlaylistImportPurpose::InitialOrImportCopy,
            operations: vec![ExternalOperationInput {
                acknowledgement_id: format!("{operation_id}-playlist"),
                operation: Operation::UpsertPlaylist {
                    playlist_id: local_id(playlist_id),
                    name: "Imported playlist".to_owned(),
                },
                recorded_at_unix: 50,
            }],
        }
    }

    fn projection() -> PendingPlaylistProjection {
        PendingPlaylistProjection {
            desired_name: "Desired playlist".to_owned(),
            ordered_entry_ids: vec![
                entry_id("desired-entry-1"),
                entry_id("desired-entry-2"),
                entry_id("desired-entry-3"),
            ],
            ordered_item_ids: vec![item_id("song-1"), item_id("song-1"), item_id("song-2")],
            stage: PendingPlaylistProjectionStage::Ambiguous,
            base_remote_fingerprint: "sha256:0123456789abcdef".to_owned(),
        }
    }

    fn pending_create(
        local: &str,
        expected_missing: Option<&str>,
        created: Option<&str>,
    ) -> PendingPlaylistCreate {
        PendingPlaylistCreate {
            local_playlist_id: local_id(local),
            expected_missing_server_id: expected_missing
                .map(|id| ServerPlaylistId::new(id).unwrap()),
            created_server_playlist_id: created.map(|id| ServerPlaylistId::new(id).unwrap()),
            desired_name: "Desired playlist".to_owned(),
            ordered_entry_ids: vec![entry_id("desired-entry-1")],
            ordered_item_ids: vec![item_id("song-1")],
            started_at_unix: 52,
        }
    }

    #[test]
    fn schema_three_round_trip_preserves_every_playlist_lifecycle_record() {
        let mut state = bridge();
        let local = local_id("local");
        state
            .upsert_playlist_link(PlaylistLink {
                managed_by_yututui: true,
                state: PlaylistLinkState::ServerMissing,
                ..link("local", "remote")
            })
            .unwrap();
        let import = PendingPlaylistImportBatch {
            operation_id: "remote-snapshot".to_owned(),
            local_playlist_id: local.clone(),
            purpose: PendingPlaylistImportPurpose::RemoteObservation,
            operations: vec![
                ExternalOperationInput {
                    acknowledgement_id: "remote-snapshot-playlist".to_owned(),
                    operation: Operation::UpsertPlaylist {
                        playlist_id: local.clone(),
                        name: "Remote playlist".to_owned(),
                    },
                    recorded_at_unix: 51,
                },
                ExternalOperationInput {
                    acknowledgement_id: "remote-snapshot-entry".to_owned(),
                    operation: Operation::UpsertPlaylistEntry {
                        playlist_id: local.clone(),
                        entry_id: entry_id("entry-1"),
                        track: portable("song-1", "backend", "account"),
                        after_entry_id: None,
                    },
                    recorded_at_unix: 51,
                },
            ],
        };
        state.queue_playlist_import(import.clone()).unwrap();
        state
            .queue_playlist_projection(local.clone(), projection())
            .unwrap();
        let create = pending_create("local", Some("remote"), Some("created-remote"));
        state.queue_playlist_create(create.clone()).unwrap();

        let decoded = decode_bridge(&encode_bridge(&state).unwrap()).unwrap();

        assert_eq!(decoded.playlist_link(&local), state.playlist_link(&local));
        assert_eq!(
            decoded.pending_playlist_imports().get("remote-snapshot"),
            Some(&import)
        );
        assert_eq!(
            decoded.pending_playlist_projections().get(&local),
            Some(&projection())
        );
        assert_eq!(
            decoded.pending_playlist_creates().get(&local),
            Some(&create)
        );
    }

    #[test]
    fn projection_attention_is_durable_and_explicit_setup_requeue_is_bounded() {
        let mut state = bridge();
        let local = local_id("local");
        let mut linked = link("local", "remote");
        linked.state = PlaylistLinkState::AccessNeedsAttention;
        state.upsert_playlist_link(linked).unwrap();
        let mut pending = projection();
        pending.stage = PendingPlaylistProjectionStage::NeedsAttention;
        state
            .queue_playlist_projection(local.clone(), pending)
            .unwrap();
        let mut incompatible = link("content-local", "content-remote");
        incompatible.content_needs_attention = true;
        state.upsert_playlist_link(incompatible).unwrap();

        let mut decoded = decode_bridge(&encode_bridge(&state).unwrap()).unwrap();

        assert_eq!(decoded.playlist_projections_needing_attention(), 1);
        assert_eq!(decoded.playlist_contents_needing_attention(), 1);
        assert_eq!(decoded.requeue_playlist_projections_needing_attention(), 1);
        assert_eq!(decoded.playlist_projections_needing_attention(), 0);
        assert_eq!(
            decoded.playlist_contents_needing_attention(),
            1,
            "connection setup cannot clear incompatible local content"
        );
        assert_eq!(
            decoded
                .pending_playlist_projections()
                .get(&local)
                .map(|pending| pending.stage),
            Some(PendingPlaylistProjectionStage::Queued)
        );
        assert_eq!(
            decoded.playlist_link(&local).map(|link| link.state),
            Some(PlaylistLinkState::Linked)
        );
        assert_eq!(decoded.requeue_playlist_projections_needing_attention(), 0);
    }

    #[test]
    fn pending_import_has_one_explicit_playlist_and_causal_purpose() {
        let mut state = bridge();
        let first = import_batch("z-first", "local");
        state.queue_playlist_import(first.clone()).unwrap();
        assert_eq!(
            state.queue_playlist_import(import_batch("a-lexically-earlier", "local")),
            Err(BridgeMutationError::ConflictingEntry),
            "opaque map ordering must not create a per-playlist backlog"
        );
        state.queue_playlist_import(first).unwrap();

        let mut wrong_target = import_batch("other", "other-local");
        wrong_target.local_playlist_id = local_id("declared-local");
        assert_eq!(
            state.queue_playlist_import(wrong_target),
            Err(BridgeMutationError::InvalidEntry)
        );

        let mut wrong_purpose = import_batch("delete-purpose", "delete-local");
        wrong_purpose.purpose = PendingPlaylistImportPurpose::Delete;
        assert_eq!(
            state.queue_playlist_import(wrong_purpose),
            Err(BridgeMutationError::InvalidEntry)
        );
    }

    #[test]
    fn missing_playlist_is_counted_once_as_a_decision_instead_of_a_reconnect() {
        let mut state = bridge();
        let local = local_id("local");
        let mut missing = link("local", "remote");
        missing.state = PlaylistLinkState::ServerMissing;
        state.upsert_playlist_link(missing).unwrap();
        let mut pending = projection();
        pending.stage = PendingPlaylistProjectionStage::NeedsAttention;
        state.queue_playlist_projection(local, pending).unwrap();

        assert_eq!(state.playlist_links_needing_decision(), 1);
        assert_eq!(state.playlist_projections_needing_attention(), 0);

        state
            .queue_playlist_create(pending_create("local", Some("remote"), None))
            .unwrap();
        assert_eq!(
            state.playlist_links_needing_decision(),
            0,
            "an in-progress restore is represented once by create recovery"
        );
    }

    #[test]
    fn schema_two_migrates_with_empty_playlist_state_and_reencodes_as_three() {
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&bridge()).unwrap()).unwrap();
        legacy["schema_version"] = serde_json::Value::from(2);
        let object = legacy.as_object_mut().unwrap();
        object.remove("playlist_links");
        object.remove("pending_playlist_imports");
        object.remove("pending_playlist_projections");
        object.remove("pending_playlist_creates");

        let migrated = decode_bridge(&serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert!(migrated.playlist_links().is_empty());
        assert!(migrated.pending_playlist_imports().is_empty());
        assert!(migrated.pending_playlist_projections().is_empty());
        assert!(migrated.pending_playlist_creates().is_empty());
        let encoded: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&migrated).unwrap()).unwrap();
        assert_eq!(
            encoded["schema_version"],
            serde_json::Value::from(super::super::BRIDGE_SCHEMA_VERSION)
        );
    }

    #[test]
    fn schema_three_rejects_projection_without_its_link() {
        let mut state = bridge();
        let local = local_id("local");
        state.upsert_playlist_link(link("local", "remote")).unwrap();
        state
            .queue_playlist_projection(local.clone(), projection())
            .unwrap();
        let mut encoded: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();
        encoded["playlist_links"]
            .as_object_mut()
            .unwrap()
            .remove(local.as_str());

        assert!(matches!(
            decode_bridge(&serde_json::to_vec(&encoded).unwrap()),
            Err(crate::open_subsonic::StoreError::InvalidState)
        ));
    }

    #[test]
    fn schema_three_rejects_delete_import_while_link_is_still_present() {
        let mut state = bridge();
        let local = local_id("local");
        state.upsert_playlist_link(link("local", "remote")).unwrap();
        let mut encoded: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();
        let pending = PendingPlaylistImportBatch {
            operation_id: "delete-local".to_owned(),
            local_playlist_id: local.clone(),
            purpose: PendingPlaylistImportPurpose::Delete,
            operations: vec![ExternalOperationInput {
                acknowledgement_id: "delete-local-operation".to_owned(),
                operation: Operation::DeletePlaylist {
                    playlist_id: local,
                    deleted: true,
                },
                recorded_at_unix: 60,
            }],
        };
        encoded["pending_playlist_imports"]["delete-local"] =
            serde_json::to_value(pending).unwrap();

        assert!(matches!(
            decode_bridge(&serde_json::to_vec(&encoded).unwrap()),
            Err(crate::open_subsonic::StoreError::InvalidState)
        ));
    }

    #[test]
    fn uncertain_create_is_single_identity_and_requires_the_expected_missing_link() {
        let mut state = bridge();
        let new_create = pending_create("new-local", None, None);
        state.queue_playlist_create(new_create.clone()).unwrap();
        state.queue_playlist_create(new_create).unwrap();
        assert_eq!(
            state.queue_playlist_create(pending_create("new-local", None, Some("server"))),
            Err(BridgeMutationError::ConflictingEntry)
        );
        state
            .record_playlist_create_server_id(
                &local_id("new-local"),
                ServerPlaylistId::new("server").unwrap(),
            )
            .unwrap();
        assert_eq!(
            state
                .pending_playlist_creates()
                .get(&local_id("new-local"))
                .and_then(|pending| pending.created_server_playlist_id.as_ref())
                .map(ServerPlaylistId::as_str),
            Some("server")
        );

        let missing = PlaylistLink {
            state: PlaylistLinkState::ServerMissing,
            ..link("missing-local", "missing-server")
        };
        state.upsert_playlist_link(missing).unwrap();
        assert_eq!(
            state.queue_playlist_create(pending_create(
                "missing-local",
                Some("wrong-server"),
                None,
            )),
            Err(BridgeMutationError::ConflictingEntry)
        );
        state
            .queue_playlist_create(pending_create(
                "missing-local",
                Some("missing-server"),
                None,
            ))
            .unwrap();
    }

    #[test]
    fn duplicate_links_and_wrong_scope_imports_are_rejected() {
        let mut state = bridge();
        state
            .upsert_playlist_link(link("local-a", "server-a"))
            .unwrap();
        assert_eq!(
            state.upsert_playlist_link(link("local-a", "server-b")),
            Err(BridgeMutationError::ConflictingEntry)
        );
        assert_eq!(
            state.upsert_playlist_link(link("local-b", "server-a")),
            Err(BridgeMutationError::ConflictingEntry)
        );

        let bad_scope = PendingPlaylistImportBatch {
            operation_id: "wrong-scope".to_owned(),
            local_playlist_id: local_id("local-a"),
            purpose: PendingPlaylistImportPurpose::RemoteObservation,
            operations: vec![ExternalOperationInput {
                acknowledgement_id: "wrong-scope-entry".to_owned(),
                operation: Operation::UpsertPlaylistEntry {
                    playlist_id: local_id("local-a"),
                    entry_id: entry_id("entry"),
                    track: portable("song", "backend", "other-account"),
                    after_entry_id: None,
                },
                recorded_at_unix: 52,
            }],
        };
        assert_eq!(
            state.queue_playlist_import(bad_scope),
            Err(BridgeMutationError::InvalidEntry)
        );
    }

    #[test]
    fn corrupt_duplicate_server_link_and_bidi_text_are_rejected_on_decode() {
        let mut state = bridge();
        state
            .upsert_playlist_link(link("local-a", "server-a"))
            .unwrap();
        state
            .upsert_playlist_link(link("local-b", "server-b"))
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();
        value["playlist_links"]["local-b"]["server_playlist_id"] =
            serde_json::Value::from("server-a");
        assert_eq!(
            decode_bridge(&serde_json::to_vec(&value).unwrap()),
            Err(crate::open_subsonic::profile::StoreError::InvalidState)
        );

        let mut value: serde_json::Value =
            serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();
        value["playlist_links"]["local-a"]["shadow"]["name"] =
            serde_json::Value::from("visible\u{202e}hidden");
        assert_eq!(
            decode_bridge(&serde_json::to_vec(&value).unwrap()),
            Err(crate::open_subsonic::profile::StoreError::InvalidState)
        );
    }

    #[test]
    fn projection_occurrence_ids_must_be_unique_and_align_with_items() {
        let mut state = bridge();
        let mut misaligned = projection();
        misaligned.ordered_entry_ids.pop();
        assert_eq!(
            state.queue_playlist_projection(local_id("misaligned"), misaligned),
            Err(BridgeMutationError::InvalidEntry)
        );

        let mut duplicate = projection();
        duplicate.ordered_entry_ids[1] = duplicate.ordered_entry_ids[0].clone();
        assert_eq!(
            state.queue_playlist_projection(local_id("duplicate"), duplicate),
            Err(BridgeMutationError::ConflictingEntry)
        );
    }

    #[test]
    fn every_playlist_collection_and_occurrence_list_is_bounded() {
        let mut links = bridge();
        for index in 0..MAX_PLAYLIST_LINKS {
            links
                .upsert_playlist_link(link(&format!("local-{index}"), &format!("server-{index}")))
                .unwrap();
        }
        assert_eq!(
            links.upsert_playlist_link(link("local-overflow", "server-overflow")),
            Err(BridgeMutationError::CapacityExceeded)
        );

        let mut imports = bridge();
        for index in 0..MAX_PENDING_PLAYLIST_IMPORTS {
            imports
                .queue_playlist_import(import_batch(
                    &format!("import-{index}"),
                    &format!("local-{index}"),
                ))
                .unwrap();
        }
        assert_eq!(
            imports.queue_playlist_import(import_batch("import-overflow", "local-overflow")),
            Err(BridgeMutationError::CapacityExceeded)
        );

        let mut projections = bridge();
        for index in 0..MAX_PENDING_PLAYLIST_PROJECTIONS {
            projections
                .queue_playlist_projection(local_id(format!("local-{index}")), projection())
                .unwrap();
        }
        assert_eq!(
            projections.queue_playlist_projection(local_id("local-overflow"), projection()),
            Err(BridgeMutationError::CapacityExceeded)
        );

        let mut oversized_link = link("local", "server");
        oversized_link.shadow = shadow("Remote playlist", MAX_PLAYLIST_OCCURRENCES + 1);
        assert_eq!(
            bridge().upsert_playlist_link(oversized_link),
            Err(BridgeMutationError::CapacityExceeded)
        );
    }
}
