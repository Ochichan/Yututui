//! Owner-only foundation for exact server observations and later projection bridges.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::model::{AccountScopeId, BackendId};
use super::profile::StoreError;

pub(crate) const BRIDGE_KIND: &str = "yututui_open_subsonic_bridge";
pub(crate) const BRIDGE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_BRIDGE_BYTES: u64 = 16 * 1024 * 1024;

/// The PR 6 bridge is deliberately empty: later rating/history work can add versioned observations
/// without coupling the profile or credentials to portable personal state.
pub struct OpenSubsonicBridgeState {
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
}

impl OpenSubsonicBridgeState {
    pub fn new(backend_id: BackendId, account_scope_id: AccountScopeId) -> Self {
        Self {
            revision: 0,
            backend_id,
            account_scope_id,
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn backend_id(&self) -> &BackendId {
        &self.backend_id
    }

    pub fn account_scope_id(&self) -> &AccountScopeId {
        &self.account_scope_id
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskBridgeState {
    kind: String,
    schema_version: u32,
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
}

pub(crate) fn encode_bridge(
    state: &OpenSubsonicBridgeState,
) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let disk = DiskBridgeState {
        kind: BRIDGE_KIND.to_owned(),
        schema_version: BRIDGE_SCHEMA_VERSION,
        revision: state.revision,
        backend_id: state.backend_id.clone(),
        account_scope_id: state.account_scope_id.clone(),
    };
    let bytes =
        Zeroizing::new(serde_json::to_vec(&disk).map_err(|_| StoreError::SerializationFailed)?);
    if bytes.len() as u64 > MAX_BRIDGE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_bridge(bytes: &[u8]) -> Result<OpenSubsonicBridgeState, StoreError> {
    if bytes.len() as u64 > MAX_BRIDGE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    let disk: DiskBridgeState =
        serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidState)?;
    if disk.kind != BRIDGE_KIND || disk.schema_version != BRIDGE_SCHEMA_VERSION {
        return Err(StoreError::InvalidState);
    }
    Ok(OpenSubsonicBridgeState {
        revision: disk.revision,
        backend_id: disk.backend_id,
        account_scope_id: disk.account_scope_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_round_trip_preserves_scope() {
        let bridge = OpenSubsonicBridgeState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
        );
        let decoded = decode_bridge(&encode_bridge(&bridge).unwrap()).unwrap();
        assert_eq!(decoded.backend_id().as_str(), "backend");
        assert_eq!(decoded.account_scope_id().as_str(), "account");
    }
}
