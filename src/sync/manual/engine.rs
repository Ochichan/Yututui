use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::personal_state::{DeviceId, OperationEnvelope, PersonalStateV2, merge};

use super::super::batch::{
    BatchAcceptance, BatchAnchor, SignedOperationBatch, apply_operation_batch,
};
use super::super::checkpoint::{CheckpointAnchor, SignedCheckpoint};
use super::super::crypto::{
    DeviceSecretMaterial, EncryptedObject, decrypt_json_with_identity, encrypt_json_to_recipients,
};
use super::super::membership::{MembershipAnchor, MembershipChain, VerifiedMembership};
use super::super::{
    ListCost, ListLimits, MAX_BATCH_OPERATIONS, MAX_BATCH_PLAINTEXT_BYTES, MAX_VAULT_PAYLOAD_BYTES,
    ObjectCondition, ObjectKey, ObjectMetadata, ObjectWriteResult, VaultDeadline, VaultError,
    VaultTransport,
};
use super::protocol::{SignedDeviceHead, SignedVaultManifest, segment_bounds};

const MAX_SYNC_ATTEMPTS: usize = 8;
const MAX_LISTED_OBJECTS: usize = 10_000;
const MAX_READ_REQUESTS: usize = 20_000;
const MAX_LIST_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCANNED_COLLECTIONS: usize = 10_000;
const MAX_SCANNED_RESOURCES: usize = 10_000;
const MAX_SYNC_DURATION: Duration = Duration::from_secs(5 * 60);
const SMALL_OBJECT_LIMIT: usize = 2 * 1024 * 1024;

/// A hook supplied by the primary state owner.
///
/// It is checked before every remote mutation and again before returning a candidate. A caller
/// that changes the local ledger while sync is in flight therefore rejects the stale result and
/// can safely rerun the merge.
pub trait LocalRevisionGuard {
    fn ensure_current(&self, expected_revision: u64) -> Result<(), VaultError>;
}

impl<F> LocalRevisionGuard for F
where
    F: Fn(u64) -> Result<(), VaultError>,
{
    fn ensure_current(&self, expected_revision: u64) -> Result<(), VaultError> {
        self(expected_revision)
    }
}

pub struct ManualSyncInput<'a> {
    pub local_state: &'a PersonalStateV2,
    pub membership: &'a MembershipChain,
    pub membership_anchor: &'a MembershipAnchor,
    pub device: &'a DeviceSecretMaterial,
    pub checkpoint_anchor: &'a CheckpointAnchor,
    /// The exact setup/pairing checkpoint may be supplied for the first vault write.
    pub bootstrap_checkpoint: Option<&'a SignedCheckpoint>,
    pub expected_local_revision: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManualSyncSummary {
    pub attempts: usize,
    pub downloaded_operations: usize,
    pub uploaded_operations: usize,
    pub downloaded_segments: usize,
    pub uploaded_segments: usize,
    pub checkpoint_written: bool,
    pub manifest_written: bool,
    pub remote_writes: usize,
}

pub struct ManualSyncCandidate {
    pub state: PersonalStateV2,
    pub membership: MembershipChain,
    pub checkpoint_anchor: CheckpointAnchor,
    pub expected_local_revision: u64,
    pub summary: ManualSyncSummary,
}

impl ManualSyncCandidate {
    pub fn ensure_revision(&self, current_revision: u64) -> Result<(), VaultError> {
        if current_revision == self.expected_local_revision {
            Ok(())
        } else {
            Err(VaultError::RevisionConflict)
        }
    }
}

pub struct ManualSyncEngine<'a, T: VaultTransport + ?Sized> {
    transport: &'a T,
}

impl<'a, T: VaultTransport + ?Sized> ManualSyncEngine<'a, T> {
    pub fn new(transport: &'a T) -> Self {
        Self { transport }
    }

    pub fn synchronize<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
    ) -> Result<ManualSyncCandidate, VaultError> {
        let mut budget = ManualSyncBudget::default();
        self.synchronize_with_budget(input, revision_guard, &mut budget)
    }

    pub fn synchronize_with_deadline<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        deadline: VaultDeadline,
    ) -> Result<ManualSyncCandidate, VaultError> {
        let mut budget = ManualSyncBudget::with_deadline(deadline);
        self.synchronize_with_budget(input, revision_guard, &mut budget)
    }

    pub fn synchronize_with_budget<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        budget: &mut ManualSyncBudget,
    ) -> Result<ManualSyncCandidate, VaultError> {
        budget.check_deadline()?;
        validate_input(input)?;
        for attempt in 1..=MAX_SYNC_ATTEMPTS {
            budget.check_deadline()?;
            revision_guard.ensure_current(input.expected_local_revision)?;
            match self.synchronize_once(input, revision_guard, budget) {
                Ok(mut candidate) => {
                    budget.check_deadline()?;
                    revision_guard.ensure_current(input.expected_local_revision)?;
                    candidate.summary.attempts = attempt;
                    return Ok(candidate);
                }
                Err(VaultError::PreconditionFailed) if attempt < MAX_SYNC_ATTEMPTS => {}
                Err(error) => return Err(error),
            }
        }
        Err(VaultError::PreconditionFailed)
    }

    fn synchronize_once<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        budget: &mut ReadBudget,
    ) -> Result<ManualSyncCandidate, VaultError> {
        let manifest_key = super::manifest_key(&input.local_state.dataset_id)?;
        let remote_manifest = self.get(&manifest_key, MAX_VAULT_PAYLOAD_BYTES, budget)?;
        let Some((manifest_object, manifest_metadata)) = remote_manifest else {
            return self.bootstrap(input, revision_guard, manifest_key, budget);
        };

        let manifest = SignedVaultManifest::decrypt_for_device(
            &manifest_object,
            input.device,
            input.membership_anchor,
        )?;
        let remote_membership = manifest.verify(input.membership_anchor)?;
        self.verify_registry(input, &manifest, budget)?;
        let checkpoint = self.load_manifest_checkpoint(input, &manifest, budget)?;
        if checkpoint.payload.membership != manifest.payload.membership
            || checkpoint.payload.checkpoint_sequence != manifest.payload.checkpoint_sequence
            || checkpoint.hash()? != manifest.payload.checkpoint_hash
        {
            return Err(VaultError::MembershipFork);
        }
        self.verify_checkpoint_progress(input, &checkpoint, budget)?;

        let (mut remote_state, stream_anchors, mut summary) =
            self.download_visible_segments(input, &checkpoint, &remote_membership, budget)?;
        let selected_membership = select_membership(
            input.membership,
            &manifest.payload.membership,
            input.membership_anchor,
        )?;
        let selected_verified = selected_membership.verify(input.membership_anchor)?;
        ensure_local_device(input.device, &selected_verified)?;

        let membership_advanced = selected_verified.head_hash != remote_membership.head_hash;
        let (mut merged, _) = merge(&remote_state, input.local_state)
            .map_err(|_| VaultError::InvalidEncryptedObject)?;
        if merged.device_registry != selected_verified.devices {
            return Err(VaultError::RegistryMismatch);
        }

        if membership_advanced {
            // Membership operations and their new recipient set become visible atomically through
            // the next signed checkpoint. Publishing a partial membership segment would make the
            // batch reducer's registry invariant temporarily false.
        } else {
            let pending = pending_local_operations(
                input.local_state,
                &remote_state,
                input.device.device_id(),
            )?;
            if !pending.is_empty() {
                revision_guard.ensure_current(input.expected_local_revision)?;
                let local_device_id = DeviceId::new(input.device.device_id())
                    .map_err(|_| VaultError::InvalidDeviceIdentity)?;
                let anchor = stream_anchors
                    .get(&local_device_id)
                    .cloned()
                    .ok_or(VaultError::InvalidEncryptedObject)?;
                let (state, uploaded) = self.upload_local_segments(
                    input,
                    revision_guard,
                    &selected_verified,
                    remote_state,
                    anchor,
                    pending,
                    budget,
                )?;
                remote_state = state;
                summary.uploaded_operations = uploaded.operations;
                summary.uploaded_segments = uploaded.segments;
                summary.remote_writes = summary.remote_writes.saturating_add(uploaded.writes);
            }
            let (candidate, _) = merge(&remote_state, input.local_state)
                .map_err(|_| VaultError::InvalidEncryptedObject)?;
            merged = candidate;
        }

        if merged.device_registry != selected_verified.devices {
            return Err(VaultError::RegistryMismatch);
        }
        let remote_changed =
            !sync_equivalent(&checkpoint.payload.state, &merged) || membership_advanced;
        let checkpoint_anchor = if remote_changed {
            revision_guard.ensure_current(input.expected_local_revision)?;
            let current_anchor = CheckpointAnchor::from_trusted(
                checkpoint.payload.checkpoint_sequence,
                checkpoint.hash()?,
            )?;
            let canonical_state = canonical_checkpoint_state(&merged);
            let next = SignedCheckpoint::create(
                selected_membership.clone(),
                input.membership_anchor,
                DeviceId::new(input.device.device_id())
                    .map_err(|_| VaultError::InvalidDeviceIdentity)?,
                input.device.signing_key(),
                &current_anchor,
                canonical_state,
            )?;
            let next_hash = next.hash()?;
            let next_key = super::checkpoint_key(
                &merged.dataset_id,
                next.payload.membership_epoch,
                &next_hash,
            )?;
            if membership_advanced {
                self.put_registry(
                    input,
                    revision_guard,
                    &selected_membership,
                    &selected_verified,
                    &mut summary,
                    budget,
                )?;
            }
            let encrypted = next.encrypt(input.membership_anchor)?;
            self.put_immutable_checkpoint(
                input,
                revision_guard,
                &next_key,
                &encrypted,
                &next_hash,
                &mut summary,
                budget,
            )?;
            let next_manifest = SignedVaultManifest::create(
                &merged.dataset_id,
                manifest
                    .payload
                    .generation
                    .checked_add(1)
                    .ok_or(VaultError::InvalidEncryptedObject)?,
                selected_membership.clone(),
                input.membership_anchor,
                DeviceId::new(input.device.device_id())
                    .map_err(|_| VaultError::InvalidDeviceIdentity)?,
                input.device,
                &next,
            )?;
            let encrypted_manifest = next_manifest.encrypt(input.membership_anchor)?;
            if membership_advanced {
                self.verify_new_revocation_heads(
                    input,
                    &checkpoint,
                    &remote_membership,
                    &selected_verified,
                    &stream_anchors,
                    budget,
                )?;
            }
            revision_guard.ensure_current(input.expected_local_revision)?;
            self.put_with_readback(
                &manifest_key,
                &encrypted_manifest,
                ObjectCondition::Match(manifest_metadata.etag),
                budget,
            )?;
            summary.remote_writes = summary.remote_writes.saturating_add(1);
            summary.checkpoint_written = true;
            summary.manifest_written = true;
            CheckpointAnchor::from_trusted(next.payload.checkpoint_sequence, next_hash)?
        } else {
            CheckpointAnchor::from_trusted(
                checkpoint.payload.checkpoint_sequence,
                checkpoint.hash()?,
            )?
        };

        let state = local_candidate_state(input.local_state, merged);
        Ok(ManualSyncCandidate {
            state,
            membership: selected_membership,
            checkpoint_anchor,
            expected_local_revision: input.expected_local_revision,
            summary,
        })
    }

    fn bootstrap<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        manifest_key: ObjectKey,
        budget: &mut ReadBudget,
    ) -> Result<ManualSyncCandidate, VaultError> {
        let verified = input.membership.verify(input.membership_anchor)?;
        ensure_local_device(input.device, &verified)?;
        if input.local_state.device_registry != verified.devices {
            return Err(VaultError::RegistryMismatch);
        }
        let checkpoint = match input.bootstrap_checkpoint {
            Some(checkpoint) => {
                let checkpoint_membership = checkpoint.verify(input.membership_anchor)?;
                if checkpoint.payload.membership != *input.membership
                    || checkpoint_membership.head_hash != verified.head_hash
                    || !sync_equivalent(&checkpoint.payload.state, input.local_state)
                {
                    return Err(VaultError::InvalidEncryptedObject);
                }
                checkpoint.clone()
            }
            None => SignedCheckpoint::create(
                input.membership.clone(),
                input.membership_anchor,
                DeviceId::new(input.device.device_id())
                    .map_err(|_| VaultError::InvalidDeviceIdentity)?,
                input.device.signing_key(),
                input.checkpoint_anchor,
                canonical_checkpoint_state(input.local_state),
            )?,
        };
        let checkpoint_hash = checkpoint.hash()?;
        if let Some(expected) = input.checkpoint_anchor.checkpoint_hash.as_deref()
            && checkpoint.payload.checkpoint_sequence == input.checkpoint_anchor.checkpoint_sequence
            && checkpoint_hash != expected
        {
            return Err(VaultError::RollbackDetected);
        }

        let mut summary = ManualSyncSummary::default();
        self.put_registry(
            input,
            revision_guard,
            input.membership,
            &verified,
            &mut summary,
            budget,
        )?;
        let checkpoint_key = super::checkpoint_key(
            &input.local_state.dataset_id,
            checkpoint.payload.membership_epoch,
            &checkpoint_hash,
        )?;
        let encrypted_checkpoint = checkpoint.encrypt(input.membership_anchor)?;
        self.put_immutable_checkpoint(
            input,
            revision_guard,
            &checkpoint_key,
            &encrypted_checkpoint,
            &checkpoint_hash,
            &mut summary,
            budget,
        )?;
        let manifest = SignedVaultManifest::create(
            &input.local_state.dataset_id,
            1,
            input.membership.clone(),
            input.membership_anchor,
            DeviceId::new(input.device.device_id())
                .map_err(|_| VaultError::InvalidDeviceIdentity)?,
            input.device,
            &checkpoint,
        )?;
        let encrypted_manifest = manifest.encrypt(input.membership_anchor)?;
        revision_guard.ensure_current(input.expected_local_revision)?;
        self.put_with_readback(
            &manifest_key,
            &encrypted_manifest,
            ObjectCondition::CreateOnly,
            budget,
        )?;
        summary.remote_writes = summary.remote_writes.saturating_add(1);
        summary.checkpoint_written = true;
        summary.manifest_written = true;
        Ok(ManualSyncCandidate {
            state: local_candidate_state(input.local_state, input.local_state.clone()),
            membership: input.membership.clone(),
            checkpoint_anchor: CheckpointAnchor::from_trusted(
                checkpoint.payload.checkpoint_sequence,
                checkpoint_hash,
            )?,
            expected_local_revision: input.expected_local_revision,
            summary,
        })
    }

    fn verify_registry(
        &self,
        input: &ManualSyncInput<'_>,
        manifest: &SignedVaultManifest,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        let (object, _) = self
            .get(
                &manifest.payload.registry_key,
                MAX_VAULT_PAYLOAD_BYTES,
                budget,
            )?
            .ok_or(VaultError::InvalidMembership)?;
        let chain: MembershipChain =
            decrypt_json_with_identity(&object, input.device.age_identity())?;
        let verified = chain.verify(input.membership_anchor)?;
        if chain != manifest.payload.membership
            || verified.head_hash != manifest.payload.membership_head_hash
        {
            return Err(VaultError::MembershipFork);
        }
        Ok(())
    }

    fn load_manifest_checkpoint(
        &self,
        input: &ManualSyncInput<'_>,
        manifest: &SignedVaultManifest,
        budget: &mut ReadBudget,
    ) -> Result<SignedCheckpoint, VaultError> {
        let (object, _) = self
            .get(
                &manifest.payload.checkpoint_key,
                MAX_VAULT_PAYLOAD_BYTES,
                budget,
            )?
            .ok_or(VaultError::RollbackDetected)?;
        let checkpoint =
            SignedCheckpoint::decrypt_for_device(&object, input.device, input.membership_anchor)?;
        Ok(checkpoint)
    }

    /// Re-read every newly revoked device head immediately before the membership manifest CAS.
    ///
    /// Device heads are independent mutable WebDAV resources, so a target can publish one after
    /// the revoker selected its cutoff without changing the manifest ETag. Refuse that stale
    /// cutoff here; the service-level revoke loop will merge the newly visible segment and rebuild
    /// the membership change with its exact high-water.
    fn verify_new_revocation_heads(
        &self,
        input: &ManualSyncInput<'_>,
        checkpoint: &SignedCheckpoint,
        remote_membership: &VerifiedMembership,
        selected_membership: &VerifiedMembership,
        stream_anchors: &BTreeMap<DeviceId, BatchAnchor>,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        for (device_id, cutoff) in &selected_membership.revocation_cutoffs {
            let newly_revoked = remote_membership
                .devices
                .get(device_id)
                .is_some_and(|device| !device.revoked);
            if !newly_revoked {
                continue;
            }
            let downloaded = stream_anchors
                .get(device_id)
                .ok_or(VaultError::InvalidMembership)?;
            if downloaded.last_sequence != *cutoff {
                return Err(VaultError::InvalidMembership);
            }

            let covered = checkpoint.payload.state.version_vector.observed(device_id);
            let head_key = super::device_head_key(&checkpoint.payload.dataset_id, device_id)?;
            let current = self.get(&head_key, SMALL_OBJECT_LIMIT, budget)?;
            let Some((head_object, _)) = current else {
                if *cutoff > covered {
                    return Err(VaultError::PreconditionFailed);
                }
                continue;
            };
            let head = SignedDeviceHead::decrypt_for_device(
                &head_object,
                input.device,
                remote_membership,
            )?;
            if &head.payload.signer_device_id != device_id || head.payload.last_sequence > *cutoff {
                return Err(VaultError::PreconditionFailed);
            }

            // When this revoke absorbed work newer than the manifest checkpoint, make sure the
            // exact terminal head we authenticated during download is still present. A covered
            // older head may legitimately predate the checkpoint's synthetic batch anchor.
            if *cutoff > covered {
                let first = downloaded
                    .last_batch_first_sequence
                    .ok_or(VaultError::InvalidMembership)?;
                let expected_key =
                    super::segment_key(&checkpoint.payload.dataset_id, device_id, first, *cutoff)?;
                if head.payload.last_sequence != *cutoff
                    || downloaded.last_batch_hash.as_deref()
                        != Some(head.payload.last_batch_hash.as_str())
                    || head.payload.last_segment_key != expected_key
                {
                    return Err(VaultError::PreconditionFailed);
                }
            }
        }
        Ok(())
    }

    fn verify_checkpoint_progress(
        &self,
        input: &ManualSyncInput<'_>,
        latest: &SignedCheckpoint,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        let local = input.checkpoint_anchor;
        let latest_hash = latest.hash()?;
        if local.checkpoint_sequence == 0 {
            if local.checkpoint_hash.is_some() {
                return Err(VaultError::RollbackDetected);
            }
            return Ok(());
        }
        let local_hash = local
            .checkpoint_hash
            .as_deref()
            .ok_or(VaultError::RollbackDetected)?;
        if latest.payload.checkpoint_sequence < local.checkpoint_sequence {
            return Err(VaultError::RollbackDetected);
        }
        if latest.payload.checkpoint_sequence == local.checkpoint_sequence {
            return if latest_hash == local_hash {
                Ok(())
            } else {
                Err(VaultError::RollbackDetected)
            };
        }

        let mut current = latest.clone();
        let mut index = None::<BTreeMap<String, ObjectKey>>;
        loop {
            if current.payload.checkpoint_sequence == local.checkpoint_sequence.saturating_add(1) {
                return if current.payload.previous_checkpoint_hash.as_deref() == Some(local_hash) {
                    Ok(())
                } else {
                    Err(VaultError::RollbackDetected)
                };
            }
            if current.payload.checkpoint_sequence <= local.checkpoint_sequence {
                return Err(VaultError::RollbackDetected);
            }
            let previous_hash = current
                .payload
                .previous_checkpoint_hash
                .clone()
                .ok_or(VaultError::SequenceGap)?;
            if index.is_none() {
                let listed = self.list(
                    &super::checkpoint_prefix(&input.local_state.dataset_id)?,
                    budget,
                )?;
                let mut keys = BTreeMap::new();
                for metadata in listed {
                    let Some(hash) = checkpoint_hash_from_key(&metadata.key) else {
                        continue;
                    };
                    if keys.insert(hash, metadata.key).is_some() {
                        return Err(VaultError::MembershipFork);
                    }
                }
                index = Some(keys);
            }
            let key = index
                .as_ref()
                .and_then(|keys| keys.get(&previous_hash))
                .ok_or(VaultError::SequenceGap)?;
            let (object, _) = self
                .get(key, MAX_VAULT_PAYLOAD_BYTES, budget)?
                .ok_or(VaultError::SequenceGap)?;
            let previous = SignedCheckpoint::decrypt_for_device(
                &object,
                input.device,
                input.membership_anchor,
            )?;
            if previous.hash()? != previous_hash
                || previous.payload.checkpoint_sequence.saturating_add(1)
                    != current.payload.checkpoint_sequence
                || current.payload.previous_checkpoint_hash.as_deref()
                    != Some(previous_hash.as_str())
            {
                return Err(VaultError::RollbackDetected);
            }
            current = previous;
            if current.hash()? != previous_hash {
                return Err(VaultError::RollbackDetected);
            }
        }
    }

    fn download_visible_segments(
        &self,
        input: &ManualSyncInput<'_>,
        checkpoint: &SignedCheckpoint,
        membership: &VerifiedMembership,
        budget: &mut ReadBudget,
    ) -> Result<
        (
            PersonalStateV2,
            BTreeMap<DeviceId, BatchAnchor>,
            ManualSyncSummary,
        ),
        VaultError,
    > {
        let mut state = checkpoint.payload.state.clone();
        let mut anchors = BTreeMap::new();
        let mut summary = ManualSyncSummary::default();
        for device in membership.active_devices() {
            let device_id = device.device_id.clone();
            let mut anchor = checkpoint.next_batch_anchor(input.membership_anchor, &device_id)?;
            let head_key = super::device_head_key(&state.dataset_id, &device_id)?;
            let Some((head_object, _)) = self.get(&head_key, SMALL_OBJECT_LIMIT, budget)? else {
                anchors.insert(device_id, anchor);
                continue;
            };
            let head = match SignedDeviceHead::decrypt_for_device(
                &head_object,
                input.device,
                membership,
            ) {
                Ok(head) => head,
                Err(VaultError::DecryptionFailed)
                    if state.version_vector.observed(&device_id) > 0 =>
                {
                    // A newly admitted device cannot decrypt a head written for an older
                    // recipient set. It is safe to ignore only when every immutable segment is
                    // already covered by the checkpoint. An unreadable head with visible work
                    // beyond that anchor is corruption, not a legacy compatibility case.
                    let checkpoint_sequence = anchor.last_sequence;
                    if self
                        .list_device_segments(&state.dataset_id, &device_id, budget)?
                        .iter()
                        .any(|(_, last, _)| *last > checkpoint_sequence)
                    {
                        return Err(VaultError::DecryptionFailed);
                    }
                    anchors.insert(device_id, anchor);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if head.payload.signer_device_id != device_id {
                return Err(VaultError::InvalidEncryptedObject);
            }
            if head.payload.last_sequence <= anchor.last_sequence {
                anchors.insert(device_id, anchor);
                continue;
            }

            let mut segments = BTreeMap::<u64, (u64, ObjectKey)>::new();
            for (first, last, key) in
                self.list_device_segments(&state.dataset_id, &device_id, budget)?
            {
                if first > anchor.last_sequence
                    && last <= head.payload.last_sequence
                    && segments.insert(first, (last, key)).is_some()
                {
                    return Err(VaultError::MembershipFork);
                }
            }
            let mut terminal_segment_key = None;
            while anchor.last_sequence < head.payload.last_sequence {
                let expected = anchor
                    .last_sequence
                    .checked_add(1)
                    .ok_or(VaultError::SequenceGap)?;
                let (last, key) = segments.remove(&expected).ok_or(VaultError::SequenceGap)?;
                let (object, _) = self
                    .get(&key, MAX_BATCH_PLAINTEXT_BYTES, budget)?
                    .ok_or(VaultError::SequenceGap)?;
                let batch =
                    SignedOperationBatch::decrypt_for_device(&object, input.device, membership)?;
                if batch.payload.first_sequence != expected
                    || batch.payload.last_sequence != last
                    || batch.payload.signer_device_id != device_id
                {
                    return Err(VaultError::InvalidEncryptedObject);
                }
                if apply_operation_batch(&mut state, &mut anchor, membership, &batch)?
                    == BatchAcceptance::Applied
                {
                    summary.downloaded_operations = summary
                        .downloaded_operations
                        .saturating_add(batch.payload.operations.len());
                    summary.downloaded_segments = summary.downloaded_segments.saturating_add(1);
                }
                terminal_segment_key = Some(key);
            }
            if anchor.last_batch_hash.as_deref() != Some(&head.payload.last_batch_hash)
                || terminal_segment_key.as_ref() != Some(&head.payload.last_segment_key)
            {
                return Err(VaultError::RollbackDetected);
            }
            anchors.insert(device_id, anchor);
        }
        Ok((state, anchors, summary))
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_local_segments<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        membership: &VerifiedMembership,
        mut state: PersonalStateV2,
        mut anchor: BatchAnchor,
        pending: Vec<OperationEnvelope>,
        budget: &mut ReadBudget,
    ) -> Result<(PersonalStateV2, UploadSummary), VaultError> {
        let device_id = anchor.signer_device_id.clone();
        let head_key = super::device_head_key(&state.dataset_id, &device_id)?;
        let current_head = match self.get(&head_key, SMALL_OBJECT_LIMIT, budget)? {
            Some((object, metadata)) => {
                let head = SignedDeviceHead::decrypt_for_device(&object, input.device, membership)?;
                if head.payload.signer_device_id != device_id {
                    return Err(VaultError::InvalidEncryptedObject);
                }
                // The download anchor and the ETag must describe the same high-water view.
                // Otherwise a concurrent writer using this device identity advanced the stream
                // after our download; using its fresh ETag with our stale anchor could overwrite
                // the head backwards and leave overlapping immutable segments.
                if head.payload.last_sequence > anchor.last_sequence {
                    return Err(VaultError::PreconditionFailed);
                }
                if head.payload.last_sequence == anchor.last_sequence
                    && let Some(first_sequence) = anchor.last_batch_first_sequence
                    && (anchor.last_batch_hash.as_deref()
                        != Some(head.payload.last_batch_hash.as_str())
                        || head.payload.last_segment_key
                            != super::segment_key(
                                &state.dataset_id,
                                &device_id,
                                first_sequence,
                                anchor.last_sequence,
                            )?)
                {
                    return Err(VaultError::RollbackDetected);
                }
                Some(metadata)
            }
            None => None,
        };
        let mut offset = 0;
        let mut uploaded = UploadSummary::default();
        let mut last_segment_key = None;
        while offset < pending.len() {
            revision_guard.ensure_current(input.expected_local_revision)?;
            let batch = largest_batch(
                membership,
                &state,
                &device_id,
                input.device,
                &anchor,
                &pending[offset..],
            )?;
            let count = batch.payload.operations.len();
            let segment_key = super::segment_key(
                &state.dataset_id,
                &device_id,
                batch.payload.first_sequence,
                batch.payload.last_sequence,
            )?;
            let encrypted = batch.encrypt(membership)?;
            self.put_immutable_batch(
                input,
                revision_guard,
                &segment_key,
                &encrypted,
                &batch,
                membership,
                budget,
            )?;
            apply_operation_batch(&mut state, &mut anchor, membership, &batch)?;
            uploaded.operations = uploaded.operations.saturating_add(count);
            uploaded.segments = uploaded.segments.saturating_add(1);
            uploaded.writes = uploaded.writes.saturating_add(1);
            offset += count;
            last_segment_key = Some(segment_key);
        }
        let last_segment_key = last_segment_key.ok_or(VaultError::InvalidEncryptedObject)?;
        let head = SignedDeviceHead::create(
            &state.dataset_id,
            membership,
            device_id,
            input.device,
            anchor.last_sequence,
            anchor
                .last_batch_hash
                .clone()
                .ok_or(VaultError::InvalidEncryptedObject)?,
            last_segment_key,
        )?;
        let encrypted_head = head.encrypt(membership)?;
        let condition = current_head.map_or(ObjectCondition::CreateOnly, |metadata| {
            ObjectCondition::Match(metadata.etag)
        });
        revision_guard.ensure_current(input.expected_local_revision)?;
        self.put_with_readback(&head_key, &encrypted_head, condition, budget)?;
        uploaded.writes = uploaded.writes.saturating_add(1);
        Ok((state, uploaded))
    }

    fn put_registry<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        membership: &MembershipChain,
        verified: &VerifiedMembership,
        summary: &mut ManualSyncSummary,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        let key = super::registry_key(&verified.dataset_id, &verified.head_hash)?;
        let encrypted = encrypt_json_to_recipients(membership, &verified.active_recipients()?)?;
        revision_guard.ensure_current(input.expected_local_revision)?;
        match self.put(&key, &encrypted, ObjectCondition::CreateOnly, budget) {
            Ok(_) => {
                summary.remote_writes = summary.remote_writes.saturating_add(1);
                Ok(())
            }
            Err(VaultError::StorageFailed) => {
                self.readback_exact(&key, &encrypted, budget)?;
                summary.remote_writes = summary.remote_writes.saturating_add(1);
                Ok(())
            }
            Err(VaultError::PreconditionFailed) => {
                let (existing, _) = self
                    .get(&key, MAX_VAULT_PAYLOAD_BYTES, budget)?
                    .ok_or(VaultError::MembershipFork)?;
                let chain: MembershipChain =
                    decrypt_json_with_identity(&existing, input.device.age_identity())?;
                if chain == *membership
                    && chain.verify(input.membership_anchor)?.head_hash == verified.head_hash
                {
                    Ok(())
                } else {
                    Err(VaultError::MembershipFork)
                }
            }
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn put_immutable_checkpoint<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        key: &ObjectKey,
        encrypted: &EncryptedObject,
        expected_hash: &str,
        summary: &mut ManualSyncSummary,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        revision_guard.ensure_current(input.expected_local_revision)?;
        match self.put(key, encrypted, ObjectCondition::CreateOnly, budget) {
            Ok(_) => {
                summary.remote_writes = summary.remote_writes.saturating_add(1);
                Ok(())
            }
            Err(VaultError::StorageFailed) => {
                self.readback_exact(key, encrypted, budget)?;
                summary.remote_writes = summary.remote_writes.saturating_add(1);
                Ok(())
            }
            Err(VaultError::PreconditionFailed) => {
                let (existing, _) = self
                    .get(key, MAX_VAULT_PAYLOAD_BYTES, budget)?
                    .ok_or(VaultError::RollbackDetected)?;
                let checkpoint = SignedCheckpoint::decrypt_for_device(
                    &existing,
                    input.device,
                    input.membership_anchor,
                )?;
                if checkpoint.hash()? == expected_hash {
                    Ok(())
                } else {
                    Err(VaultError::RollbackDetected)
                }
            }
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn put_immutable_batch<G: LocalRevisionGuard>(
        &self,
        input: &ManualSyncInput<'_>,
        revision_guard: &G,
        key: &ObjectKey,
        encrypted: &EncryptedObject,
        expected: &SignedOperationBatch,
        membership: &VerifiedMembership,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        revision_guard.ensure_current(input.expected_local_revision)?;
        match self.put(key, encrypted, ObjectCondition::CreateOnly, budget) {
            Ok(_) => Ok(()),
            Err(VaultError::StorageFailed) => self.readback_exact(key, encrypted, budget),
            Err(VaultError::PreconditionFailed) => {
                let (existing, _) = self
                    .get(key, MAX_BATCH_PLAINTEXT_BYTES, budget)?
                    .ok_or(VaultError::MembershipFork)?;
                let batch =
                    SignedOperationBatch::decrypt_for_device(&existing, input.device, membership)?;
                if batch.hash()? == expected.hash()? {
                    Ok(())
                } else {
                    Err(VaultError::MembershipFork)
                }
            }
            Err(error) => Err(error),
        }
    }

    fn put_with_readback(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
        budget: &mut ReadBudget,
    ) -> Result<ObjectMetadata, VaultError> {
        match self.put(key, object, condition, budget) {
            Ok(result) => Ok(write_metadata(result)),
            Err(VaultError::StorageFailed) => self.readback_exact_metadata(key, object, budget),
            Err(error) => Err(error),
        }
    }

    fn readback_exact(
        &self,
        key: &ObjectKey,
        expected: &EncryptedObject,
        budget: &mut ReadBudget,
    ) -> Result<(), VaultError> {
        self.readback_exact_metadata(key, expected, budget)
            .map(|_| ())
    }

    fn readback_exact_metadata(
        &self,
        key: &ObjectKey,
        expected: &EncryptedObject,
        budget: &mut ReadBudget,
    ) -> Result<ObjectMetadata, VaultError> {
        let (actual, metadata) = self
            .get(key, MAX_VAULT_PAYLOAD_BYTES, budget)?
            .ok_or(VaultError::StorageFailed)?;
        if actual.as_bytes() == expected.as_bytes() {
            Ok(metadata)
        } else {
            // The write may have committed and then lost a race to another conditional writer.
            // Refetching the manifest/head is the only safe way to classify that outcome.
            Err(VaultError::PreconditionFailed)
        }
    }

    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
        budget: &mut ReadBudget,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        budget.get(self.transport, key, max_bytes)
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        budget: &mut ReadBudget,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        budget.list(self.transport, prefix)
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
        budget: &mut ReadBudget,
    ) -> Result<ObjectWriteResult, VaultError> {
        budget.check_deadline()?;
        let result = self
            .transport
            .put_with_deadline(key, object, condition, budget.deadline);
        budget.check_deadline()?;
        result
    }

    fn list_device_segments(
        &self,
        dataset_id: &str,
        device_id: &DeviceId,
        budget: &mut ReadBudget,
    ) -> Result<Vec<(u64, u64, ObjectKey)>, VaultError> {
        let listed = self.list(&super::segment_prefix(dataset_id, device_id)?, budget)?;
        let mut result = Vec::with_capacity(listed.len());
        for metadata in listed {
            let (first, last) = segment_bounds(&metadata.key)?;
            if metadata.key != super::segment_key(dataset_id, device_id, first, last)? {
                return Err(VaultError::InvalidObjectKey);
            }
            result.push((first, last, metadata.key));
        }
        Ok(result)
    }
}

#[derive(Default)]
struct UploadSummary {
    operations: usize,
    segments: usize,
    writes: usize,
}

pub struct ManualSyncBudget {
    consumed: usize,
    requests: usize,
    listed_objects: usize,
    list_response_bytes: usize,
    scanned_collections: usize,
    scanned_resources: usize,
    deadline: VaultDeadline,
}

type ReadBudget = ManualSyncBudget;

impl Default for ManualSyncBudget {
    fn default() -> Self {
        Self::with_deadline(VaultDeadline::from_now(MAX_SYNC_DURATION))
    }
}

impl ManualSyncBudget {
    pub fn with_deadline(deadline: VaultDeadline) -> Self {
        Self {
            consumed: 0,
            requests: 0,
            listed_objects: 0,
            list_response_bytes: 0,
            scanned_collections: 0,
            scanned_resources: 0,
            deadline,
        }
    }

    pub fn check_deadline(&self) -> Result<(), VaultError> {
        self.deadline.check()
    }

    #[cfg(test)]
    pub(crate) fn consumed_requests(&self) -> usize {
        self.requests
    }

    pub fn get<T: VaultTransport + ?Sized>(
        &mut self,
        transport: &T,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.check_deadline()?;
        self.consume_requests(1)?;
        let result = transport.get_with_deadline(key, max_bytes, self.deadline);
        self.check_deadline()?;
        let result = result?;
        if let Some((object, metadata)) = &result {
            let actual_length: u64 = object
                .as_bytes()
                .len()
                .try_into()
                .map_err(|_| VaultError::PayloadTooLarge)?;
            self.consume_ciphertext(metadata.content_length.max(actual_length))?;
        }
        Ok(result)
    }

    fn list<T: VaultTransport + ?Sized>(
        &mut self,
        transport: &T,
        prefix: &ObjectKey,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.check_deadline()?;
        let limits = self.list_limits()?;
        let outcome = transport.list_with_limits(prefix, limits, self.deadline);
        self.check_deadline()?;
        let outcome = outcome?;
        outcome.validate(limits)?;
        self.consume_list_cost(outcome.cost)?;
        Ok(outcome.objects)
    }

    fn consume_ciphertext(&mut self, bytes: u64) -> Result<(), VaultError> {
        let bytes: usize = bytes.try_into().map_err(|_| VaultError::PayloadTooLarge)?;
        self.consume(bytes)
    }

    fn consume(&mut self, bytes: usize) -> Result<(), VaultError> {
        self.consumed = self
            .consumed
            .checked_add(bytes)
            .ok_or(VaultError::PayloadTooLarge)?;
        if self.consumed > MAX_VAULT_PAYLOAD_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        Ok(())
    }

    fn consume_requests(&mut self, requests: usize) -> Result<(), VaultError> {
        self.requests = self
            .requests
            .checked_add(requests)
            .ok_or(VaultError::ResourceLimitExceeded)?;
        if self.requests > MAX_READ_REQUESTS {
            return Err(VaultError::ResourceLimitExceeded);
        }
        Ok(())
    }

    fn list_limits(&self) -> Result<ListLimits, VaultError> {
        Ok(ListLimits {
            requests: remaining(MAX_READ_REQUESTS, self.requests)?,
            response_bytes: remaining(MAX_LIST_RESPONSE_BYTES, self.list_response_bytes)?,
            scanned_collections: remaining(MAX_SCANNED_COLLECTIONS, self.scanned_collections)?,
            scanned_resources: remaining(MAX_SCANNED_RESOURCES, self.scanned_resources)?,
            returned_objects: remaining(MAX_LISTED_OBJECTS, self.listed_objects)?,
        })
    }

    fn consume_list_cost(&mut self, cost: ListCost) -> Result<(), VaultError> {
        self.consume_requests(cost.requests)?;
        self.list_response_bytes = checked_total(
            self.list_response_bytes,
            cost.response_bytes,
            MAX_LIST_RESPONSE_BYTES,
        )?;
        self.scanned_collections = checked_total(
            self.scanned_collections,
            cost.scanned_collections,
            MAX_SCANNED_COLLECTIONS,
        )?;
        self.scanned_resources = checked_total(
            self.scanned_resources,
            cost.scanned_resources,
            MAX_SCANNED_RESOURCES,
        )?;
        self.listed_objects = self
            .listed_objects
            .checked_add(cost.returned_objects)
            .ok_or(VaultError::ResourceLimitExceeded)?;
        if self.listed_objects > MAX_LISTED_OBJECTS {
            return Err(VaultError::ResourceLimitExceeded);
        }
        Ok(())
    }
}

fn remaining(maximum: usize, consumed: usize) -> Result<usize, VaultError> {
    maximum
        .checked_sub(consumed)
        .filter(|remaining| *remaining > 0)
        .ok_or(VaultError::ResourceLimitExceeded)
}

fn checked_total(current: usize, added: usize, maximum: usize) -> Result<usize, VaultError> {
    current
        .checked_add(added)
        .filter(|total| *total <= maximum)
        .ok_or(VaultError::ResourceLimitExceeded)
}

fn validate_input(input: &ManualSyncInput<'_>) -> Result<(), VaultError> {
    input
        .local_state
        .validate()
        .map_err(|_| VaultError::InvalidEncryptedObject)?;
    if input.local_state.revision != input.expected_local_revision {
        return Err(VaultError::RevisionConflict);
    }
    let membership = input.membership.verify(input.membership_anchor)?;
    if input.local_state.dataset_id != membership.dataset_id
        || input.local_state.device_registry != membership.devices
    {
        return Err(VaultError::RegistryMismatch);
    }
    ensure_local_device(input.device, &membership)?;
    if (input.checkpoint_anchor.checkpoint_sequence == 0)
        != input.checkpoint_anchor.checkpoint_hash.is_none()
    {
        return Err(VaultError::RollbackDetected);
    }
    Ok(())
}

fn ensure_local_device(
    device: &DeviceSecretMaterial,
    membership: &VerifiedMembership,
) -> Result<(), VaultError> {
    membership
        .devices
        .values()
        .find(|record| record.device_id.as_str() == device.device_id())
        .filter(|record| !record.revoked && device.matches_personal_record(record))
        .map(|_| ())
        .ok_or(VaultError::RevokedOrUnknownDevice)
}

fn select_membership(
    local: &MembershipChain,
    remote: &MembershipChain,
    anchor: &MembershipAnchor,
) -> Result<MembershipChain, VaultError> {
    let local_verified = local.verify(anchor)?;
    let remote_verified = remote.verify(anchor)?;
    if local_verified.root_hash != remote_verified.root_hash || local.root != remote.root {
        return Err(VaultError::MembershipFork);
    }
    if local.changes.starts_with(&remote.changes) {
        return Ok(local.clone());
    }
    if remote.changes.starts_with(&local.changes) {
        return Ok(remote.clone());
    }
    Err(VaultError::MembershipFork)
}

fn pending_local_operations(
    local: &PersonalStateV2,
    remote: &PersonalStateV2,
    local_device_id: &str,
) -> Result<Vec<OperationEnvelope>, VaultError> {
    let remote_by_id = remote
        .operations
        .iter()
        .map(|operation| (&operation.operation_id, operation))
        .collect::<BTreeMap<_, _>>();
    let remote_dots = remote
        .operations
        .iter()
        .map(|operation| &operation.stamp.dot)
        .collect::<BTreeSet<_>>();
    let mut pending = Vec::new();
    for operation in &local.operations {
        match remote_by_id.get(&operation.operation_id) {
            Some(existing) if *existing == operation => continue,
            Some(_) => return Err(VaultError::InvalidEncryptedObject),
            None => {}
        }
        if remote_dots.contains(&operation.stamp.dot) {
            return Err(VaultError::InvalidEncryptedObject);
        }
        if operation.stamp.dot.device_id.as_str() != local_device_id {
            return Err(VaultError::RollbackDetected);
        }
        pending.push(operation.clone());
    }
    pending.sort_by_key(|operation| operation.stamp.dot.sequence);
    let local_device =
        DeviceId::new(local_device_id).map_err(|_| VaultError::InvalidDeviceIdentity)?;
    let first = remote
        .version_vector
        .observed(&local_device)
        .checked_add(1)
        .ok_or(VaultError::SequenceGap)?;
    for (offset, operation) in pending.iter().enumerate() {
        if operation.stamp.dot.sequence
            != first
                .checked_add(offset as u64)
                .ok_or(VaultError::SequenceGap)?
        {
            return Err(VaultError::SequenceGap);
        }
    }
    if local.version_vector.observed(&local_device)
        > remote.version_vector.observed(&local_device) + pending.len() as u64
    {
        return Err(VaultError::SequenceGap);
    }
    Ok(pending)
}

fn largest_batch(
    membership: &VerifiedMembership,
    state: &PersonalStateV2,
    device_id: &DeviceId,
    device: &DeviceSecretMaterial,
    anchor: &BatchAnchor,
    pending: &[OperationEnvelope],
) -> Result<SignedOperationBatch, VaultError> {
    let maximum = pending.len().min(MAX_BATCH_OPERATIONS);
    let mut low = 1;
    let mut high = maximum;
    let mut best = None;
    while low <= high {
        let middle = low + (high - low) / 2;
        match SignedOperationBatch::create(
            membership,
            state,
            device_id.clone(),
            device.signing_key(),
            anchor,
            pending[..middle].to_vec(),
        ) {
            Ok(batch) => {
                best = Some(batch);
                low = middle.saturating_add(1);
            }
            Err(VaultError::PayloadTooLarge) => {
                high = middle.saturating_sub(1);
            }
            Err(error) => return Err(error),
        }
    }
    best.ok_or(VaultError::PayloadTooLarge)
}

fn sync_equivalent(left: &PersonalStateV2, right: &PersonalStateV2) -> bool {
    left.dataset_id == right.dataset_id
        && left.device_registry == right.device_registry
        && left.version_vector == right.version_vector
        && left.operations == right.operations
        && left.compaction_checkpoint == right.compaction_checkpoint
}

fn canonical_checkpoint_state(state: &PersonalStateV2) -> PersonalStateV2 {
    let mut state = state.clone();
    state.projection_fingerprint = None;
    state
}

fn local_candidate_state(local: &PersonalStateV2, mut merged: PersonalStateV2) -> PersonalStateV2 {
    let changed = !sync_equivalent(local, &merged);
    merged.revision = local.revision;
    merged.projection_fingerprint = if changed {
        None
    } else {
        local.projection_fingerprint.clone()
    };
    merged
}

fn checkpoint_hash_from_key(key: &ObjectKey) -> Option<String> {
    let file = key.as_str().rsplit('/').next()?;
    let hash = file.strip_suffix(".age")?;
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Some(hash.to_owned())
    } else {
        None
    }
}

fn write_metadata(result: ObjectWriteResult) -> ObjectMetadata {
    match result {
        ObjectWriteResult::Created(metadata)
        | ObjectWriteResult::Updated(metadata)
        | ObjectWriteResult::AlreadyPresent(metadata) => metadata,
    }
}
