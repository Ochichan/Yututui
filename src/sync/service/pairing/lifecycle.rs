use crate::personal_state::{DeviceId, DeviceRecord, ImportSummary, PersonalStateV2};

use super::super::manual::PreparedManualSync;
use super::super::{SyncServiceError, rebase_local_operations};

/// Redacted state returned after a join request is durably published.
///
/// It contains no endpoint, credential, pairing code, request nonce, or private key and is safe to
/// retain in UI state while one-shot polling is scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingJoinWaiting {
    pub device_id: String,
    pub expires_at_unix: i64,
    pub resumed: bool,
}

/// A remote pairing handoff prepared by a network worker but not installed in the local ledger.
///
/// Debug and serde are intentionally absent so future changes cannot accidentally make this a
/// retained diagnostic payload. The contained manual-sync candidate is already redacted.
#[derive(Clone)]
pub struct PreparedPairingApproval {
    pub(super) candidate: PreparedManualSync,
    pub(super) target_device: DeviceRecord,
    pub(super) target_device_name: String,
    pub(super) target_fingerprint: String,
    pub(super) invite_id: String,
}

impl PreparedPairingApproval {
    pub fn candidate(&self) -> &PreparedManualSync {
        &self.candidate
    }

    pub fn into_candidate(self) -> PreparedManualSync {
        self.candidate
    }

    pub fn target_device_id(&self) -> &DeviceId {
        &self.target_device.device_id
    }

    pub fn target_device_name(&self) -> &str {
        &self.target_device_name
    }

    pub fn target_fingerprint(&self) -> &str {
        &self.target_fingerprint
    }

    #[cfg(test)]
    pub(crate) fn for_persistence_test(
        candidate: PreparedManualSync,
        target_device: DeviceRecord,
    ) -> Self {
        Self {
            target_device_name: target_device.name.clone(),
            target_fingerprint: "test-fingerprint".to_owned(),
            invite_id: "test-invite".to_owned(),
            candidate,
            target_device,
        }
    }

    /// Rebase a detached host approval over local operations recorded while it was in flight.
    ///
    /// The authenticated membership/checkpoint result and private-store revision stay unchanged.
    /// Only the observed local suffix is re-authored after that result, and the candidate is bound
    /// to the latest owner revision.
    pub fn retarget(
        &self,
        observed: &PersonalStateV2,
        current: &PersonalStateV2,
    ) -> Result<Self, SyncServiceError> {
        if self.candidate.expected_local_revision != observed.revision {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let mut candidate = self.candidate.clone();
        candidate.state = rebase_local_operations(
            &candidate.state,
            observed,
            current,
            &candidate.local_device_id,
        )?;
        candidate.expected_local_revision = current.revision;
        Ok(Self {
            candidate,
            target_device: self.target_device.clone(),
            target_device_name: self.target_device_name.clone(),
            target_fingerprint: self.target_fingerprint.clone(),
            invite_id: self.invite_id.clone(),
        })
    }
}

#[derive(Clone)]
pub struct PairingJoinPreview {
    pub summary: ImportSummary,
    pub device_id: String,
    pub(super) candidate: PersonalStateV2,
    pub(super) checkpoint: super::super::super::SignedCheckpoint,
    pub(super) expected_local_revision: u64,
    pub(super) expected_private_revision: u64,
}

impl PairingJoinPreview {
    pub fn target_state(&self) -> &PersonalStateV2 {
        &self.candidate
    }

    pub fn expected_local_revision(&self) -> u64 {
        self.expected_local_revision
    }

    pub fn expected_private_revision(&self) -> u64 {
        self.expected_private_revision
    }

    pub fn into_activation(self) -> PreparedPairingJoinActivation {
        PreparedPairingJoinActivation { preview: self }
    }
}

/// Clone-safe, secret-free payload for applying a join on the persistence-owner lane.
///
/// The exact private-store revision and signed checkpoint stay bound to the preview, so a worker
/// cannot turn this into an unguarded ledger write.
#[derive(Clone)]
pub struct PreparedPairingJoinActivation {
    pub(super) preview: PairingJoinPreview,
}

impl PreparedPairingJoinActivation {
    pub fn target_state(&self) -> &PersonalStateV2 {
        self.preview.target_state()
    }

    pub fn expected_local_revision(&self) -> u64 {
        self.preview.expected_local_revision
    }

    pub fn expected_private_revision(&self) -> u64 {
        self.preview.expected_private_revision
    }

    pub fn device_id(&self) -> &str {
        &self.preview.device_id
    }

    pub fn summary(&self) -> &ImportSummary {
        &self.preview.summary
    }

    /// Recompute the deletion-free join target against the latest local owner state.
    ///
    /// This performs no file or network I/O. The authenticated checkpoint and the pending private
    /// revision remain fixed; only the portable local contribution and expected local revision
    /// are refreshed.
    pub fn retarget(&self, current: &PersonalStateV2) -> Result<Self, SyncServiceError> {
        let device_id = DeviceId::new(self.preview.device_id.clone())
            .map_err(|_| SyncServiceError::InvalidRemoteData)?;
        let plan = crate::personal_state::plan_join_import(
            &self.preview.checkpoint.payload.state,
            current,
            &device_id,
        )?;
        Ok(Self {
            preview: PairingJoinPreview {
                summary: plan.summary,
                device_id: self.preview.device_id.clone(),
                candidate: plan.candidate,
                checkpoint: self.preview.checkpoint.clone(),
                expected_local_revision: current.revision,
                expected_private_revision: self.preview.expected_private_revision,
            },
        })
    }

    pub fn target_state_for(
        &self,
        current: &PersonalStateV2,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        Ok(self.retarget(current)?.preview.candidate)
    }
}
