//! Primary-owner orchestration for manual encrypted personal-state sync.
//!
//! Network workers only produce [`PreparedManualSync`] values. Installing one still goes through
//! the personal-state transaction coordinator and rechecks the exact local revision first.

mod devices;
mod manual;
mod pairing;
mod persistence;
mod setup;
mod status;
mod transition;

use std::fmt;

use super::{SyncFailureKind, SyncPaths, VaultCredential, VaultError, WebDavProfile};
use crate::personal_state::{DeviceId, PersonalStateV2};

pub use devices::{DeviceSummary, apply_prepared_revoke_now, revoke_device, revoke_device_now};
pub use manual::{
    AppliedManualSync, PreparedManualSync, apply_manual_sync, apply_prepared_sync_now,
    prepare_manual_sync, prepare_manual_sync_with_transport, sync_now,
};
pub use pairing::{
    PairingHostInvite, PairingJoinPreview, PairingReview, apply_pairing_join,
    approve_pairing_request, begin_pairing_join, cancel_pairing_join, create_pairing_invite,
    defer_pairing_join, poll_pairing_request, resume_pairing_join,
};
pub(crate) use persistence::rebase_local_operations;
pub use persistence::{PersonalSyncApplyKind, PersonalSyncPersistence};
pub use setup::{SetupRequest, SetupResult, setup};
pub use status::{
    LocalSyncSnapshot, SyncStatusReport, load_local_snapshot, load_personal_state_read_only,
    read_audit, read_current_status, read_devices, read_status,
};
pub(crate) use transition::recover_pending_anchor_transition;

/// Prepare one primary-owner sync attempt while retaining redacted attempt/failure state.
///
/// The TUI and daemon both use this entry point so a network failure is observable through the
/// same health and audit stores regardless of which owner is running. Those stores are
/// operational hints: a failure to update them must never replace the authoritative network
/// result.
pub fn prepare_owner_sync(
    state: &PersonalStateV2,
    revoke_target: Option<&DeviceId>,
    paths: &SyncPaths,
) -> Result<PreparedManualSync, SyncServiceError> {
    track_owner_preparation(paths, || match revoke_target {
        Some(target) => revoke_device(state, target, paths),
        None => prepare_manual_sync(state, paths),
    })
}

fn track_owner_preparation<T>(
    paths: &SyncPaths,
    prepare: impl FnOnce() -> Result<T, SyncServiceError>,
) -> Result<T, SyncServiceError> {
    let now = crate::signals::unix_now();
    let _ = manual::record_sync_started(paths, now);
    let result = prepare();
    if let Err(error) = &result {
        let _ = manual::record_sync_failure(paths, now, *error);
    }
    result
}

/// Open a profile which was capability-tested before it became durable.
///
/// This constructor performs no network I/O. Setup and a fresh pairing join use
/// [`setup::checked_webdav_transport`] instead; normal sync, revocation, and resumed/host pairing
/// must use this path so a no-op operation cannot rewrite the capability marker.
pub(super) fn open_saved_webdav_transport(
    profile: &WebDavProfile,
    credential: &VaultCredential,
) -> Result<super::webdav::BlockingWebDavTransport, SyncServiceError> {
    super::webdav::BlockingWebDavTransport::with_custom_ca(
        profile.endpoint(),
        profile.custom_ca_pem(),
        credential,
    )
    .map_err(map_webdav_error)
}

/// A redacted orchestration error safe for CLI output, retained outcomes, and audit state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncServiceError {
    NotConfigured,
    AlreadyConfigured,
    PendingApproval,
    Revoked,
    MissingCredential,
    UnsupportedServer,
    Authentication,
    Certificate,
    Offline,
    InvalidRemoteData,
    LocalStateChanged,
    Storage,
    PairingExpired,
    PairingRejected,
    PairingNeedsCleanup,
    RecoveryKitNotConfirmed,
}

impl SyncServiceError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotConfigured => "sync_not_configured",
            Self::AlreadyConfigured => "sync_already_configured",
            Self::PendingApproval => "sync_pending_approval",
            Self::Revoked => "sync_device_revoked",
            Self::MissingCredential => "sync_missing_credential",
            Self::UnsupportedServer => "sync_server_unsupported",
            Self::Authentication => "sync_authentication_failed",
            Self::Certificate => "sync_certificate_failed",
            Self::Offline => "sync_offline",
            Self::InvalidRemoteData => "sync_invalid_remote_data",
            Self::LocalStateChanged => "sync_local_state_changed",
            Self::Storage => "sync_storage_failed",
            Self::PairingExpired => "sync_pairing_expired",
            Self::PairingRejected => "sync_pairing_rejected",
            Self::PairingNeedsCleanup => "sync_pairing_needs_cleanup",
            Self::RecoveryKitNotConfirmed => "sync_recovery_not_confirmed",
        }
    }

    pub fn failure_kind(self) -> Option<SyncFailureKind> {
        match self {
            Self::Authentication => Some(SyncFailureKind::Authentication),
            Self::Certificate => Some(SyncFailureKind::Certificate),
            Self::Offline => Some(SyncFailureKind::Offline),
            Self::PendingApproval => Some(SyncFailureKind::DeviceApproval),
            Self::InvalidRemoteData | Self::PairingRejected | Self::PairingNeedsCleanup => {
                Some(SyncFailureKind::InvalidRemoteData)
            }
            Self::LocalStateChanged => Some(SyncFailureKind::LocalStateChanged),
            Self::Storage | Self::RecoveryKitNotConfirmed => Some(SyncFailureKind::Storage),
            Self::NotConfigured
            | Self::AlreadyConfigured
            | Self::Revoked
            | Self::MissingCredential
            | Self::UnsupportedServer
            | Self::PairingExpired => None,
        }
    }
}

impl fmt::Display for SyncServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotConfigured => "personal sync is not set up",
            Self::AlreadyConfigured => "personal sync is already set up",
            Self::PendingApproval => "this device is waiting for approval",
            Self::Revoked => "this device has been removed from personal sync",
            Self::MissingCredential => "the WebDAV credential is unavailable",
            Self::UnsupportedServer => "the server does not support encrypted personal sync",
            Self::Authentication => "the WebDAV credential was rejected",
            Self::Certificate => "the server certificate could not be verified",
            Self::Offline => "the sync server is unavailable",
            Self::InvalidRemoteData => "the encrypted remote state could not be verified",
            Self::LocalStateChanged => "local personal state changed; retrying is safe",
            Self::Storage => "personal sync state could not be stored",
            Self::PairingExpired => "the device connection code expired",
            Self::PairingRejected => "the device connection could not be verified",
            Self::PairingNeedsCleanup => {
                "an unfinished device connection must be cancelled before retrying"
            }
            Self::RecoveryKitNotConfirmed => "the recovery kit was not saved and verified",
        })
    }
}

impl std::error::Error for SyncServiceError {}

impl From<VaultError> for SyncServiceError {
    fn from(error: VaultError) -> Self {
        match error {
            VaultError::RevisionConflict => Self::LocalStateChanged,
            VaultError::RemoteAuthentication => Self::Authentication,
            VaultError::RemoteCertificate => Self::Certificate,
            VaultError::RemoteUnavailable => Self::Offline,
            VaultError::RemoteUnsupported => Self::UnsupportedServer,
            VaultError::StorageFailed | VaultError::StorageBusy => Self::Storage,
            VaultError::PairingExpired | VaultError::PairingConsumed => Self::PairingExpired,
            VaultError::InvalidPrivateStore => Self::NotConfigured,
            VaultError::RevokedOrUnknownDevice => Self::Revoked,
            VaultError::PairingProofFailed | VaultError::InvalidPairingCode => {
                Self::PairingRejected
            }
            _ => Self::InvalidRemoteData,
        }
    }
}

impl From<crate::personal_state::PersonalStateError> for SyncServiceError {
    fn from(error: crate::personal_state::PersonalStateError) -> Self {
        match error {
            crate::personal_state::PersonalStateError::Io(_) => Self::Storage,
            crate::personal_state::PersonalStateError::ProjectionMismatch => {
                Self::LocalStateChanged
            }
            _ => Self::InvalidRemoteData,
        }
    }
}

fn map_webdav_error(error: super::webdav::WebDavError) -> SyncServiceError {
    use super::webdav::WebDavError;

    match error {
        WebDavError::AuthenticationRequired | WebDavError::PermissionDenied => {
            SyncServiceError::Authentication
        }
        WebDavError::CertificateFailed => SyncServiceError::Certificate,
        WebDavError::MethodUnsupported => SyncServiceError::UnsupportedServer,
        WebDavError::RequestFailed
        | WebDavError::RateLimited
        | WebDavError::ServerUnavailable
        | WebDavError::Locked => SyncServiceError::Offline,
        WebDavError::PreconditionFailed => SyncServiceError::LocalStateChanged,
        WebDavError::InvalidEndpoint
        | WebDavError::UnsupportedScheme
        | WebDavError::EndpointCredentials
        | WebDavError::ResponseTooLarge
        | WebDavError::InvalidResponse
        | WebDavError::InvalidXml
        | WebDavError::InvalidEntityTag
        | WebDavError::MissingStrongEntityTag
        | WebDavError::CrossOriginRedirect
        | WebDavError::RedirectLimitExceeded
        | WebDavError::InvalidRedirect
        | WebDavError::NotFound
        | WebDavError::Conflict
        | WebDavError::UnexpectedStatus(_)
        | WebDavError::ResourceLimitExceeded
        | WebDavError::InvalidEncryptedObject
        | WebDavError::AmbiguousWrite => SyncServiceError::InvalidRemoteData,
    }
}

#[cfg(test)]
mod tests;
