//! Owner-only OpenSubsonic credentials.

use age::secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::model::{AccountScopeId, BackendId};
use super::profile::StoreError;

pub(crate) const PRIVATE_KIND: &str = "yututui_open_subsonic_private";
pub(crate) const PRIVATE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_PRIVATE_BYTES: u64 = 256 * 1024;
const MAX_USERNAME_BYTES: usize = 1_024;
const MAX_PASSWORD_BYTES: usize = 64 * 1024;
const MAX_API_KEY_BYTES: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    Password,
}

/// Secret-bearing setup input. Deliberately lacks `Debug`, `Clone`, and serde.
pub struct ServerCredential {
    kind: CredentialKind,
    username: Option<SecretString>,
    secret: SecretString,
}

impl ServerCredential {
    pub fn api_key(key: SecretString) -> Result<Self, StoreError> {
        validate_credential_part(key.expose_secret(), MAX_API_KEY_BYTES)?;
        if encoded_query_value_len(key.expose_secret()) >= MAX_API_KEY_BYTES {
            return Err(StoreError::InvalidState);
        }
        Ok(Self {
            kind: CredentialKind::ApiKey,
            username: None,
            secret: key,
        })
    }

    pub fn password(
        username: impl Into<String>,
        password: SecretString,
    ) -> Result<Self, StoreError> {
        let username = username.into();
        validate_credential_part(&username, MAX_USERNAME_BYTES)?;
        validate_credential_part(password.expose_secret(), MAX_PASSWORD_BYTES)?;
        Ok(Self {
            kind: CredentialKind::Password,
            username: Some(SecretString::from(username)),
            secret: password,
        })
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    pub(crate) fn username(&self) -> Option<&SecretString> {
        self.username.as_ref()
    }

    pub(crate) fn secret(&self) -> &SecretString {
        &self.secret
    }
}

/// Credential plus stable account binding. Deliberately lacks `Debug` and `Clone`.
pub struct OpenSubsonicPrivateState {
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    credential: ServerCredential,
}

impl OpenSubsonicPrivateState {
    pub fn new(
        backend_id: BackendId,
        account_scope_id: AccountScopeId,
        credential: ServerCredential,
    ) -> Self {
        Self {
            revision: 0,
            backend_id,
            account_scope_id,
            credential,
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

    pub fn credential_kind(&self) -> CredentialKind {
        self.credential.kind()
    }

    pub(crate) fn credential(&self) -> &ServerCredential {
        &self.credential
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiskCredentialKind {
    ApiKey,
    Password,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskPrivateState {
    kind: String,
    schema_version: u32,
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    credential_kind: DiskCredentialKind,
    username: Option<String>,
    secret: String,
}

impl Drop for DiskPrivateState {
    fn drop(&mut self) {
        if let Some(username) = &mut self.username {
            username.zeroize();
        }
        self.secret.zeroize();
    }
}

pub(crate) fn encode_private(
    state: &OpenSubsonicPrivateState,
) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let disk = DiskPrivateState {
        kind: PRIVATE_KIND.to_owned(),
        schema_version: PRIVATE_SCHEMA_VERSION,
        revision: state.revision,
        backend_id: state.backend_id.clone(),
        account_scope_id: state.account_scope_id.clone(),
        credential_kind: match state.credential.kind() {
            CredentialKind::ApiKey => DiskCredentialKind::ApiKey,
            CredentialKind::Password => DiskCredentialKind::Password,
        },
        username: state
            .credential
            .username()
            .map(|username| username.expose_secret().to_owned()),
        secret: state.credential.secret().expose_secret().to_owned(),
    };
    let bytes =
        Zeroizing::new(serde_json::to_vec(&disk).map_err(|_| StoreError::SerializationFailed)?);
    if bytes.len() as u64 > MAX_PRIVATE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_private(bytes: &[u8]) -> Result<OpenSubsonicPrivateState, StoreError> {
    if bytes.len() as u64 > MAX_PRIVATE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    let disk: DiskPrivateState =
        serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidState)?;
    if disk.kind != PRIVATE_KIND || disk.schema_version != PRIVATE_SCHEMA_VERSION {
        return Err(StoreError::InvalidState);
    }
    let credential = match disk.credential_kind {
        DiskCredentialKind::ApiKey if disk.username.is_none() => {
            ServerCredential::api_key(SecretString::from(disk.secret.clone()))?
        }
        DiskCredentialKind::Password => {
            let username = disk.username.clone().ok_or(StoreError::InvalidState)?;
            ServerCredential::password(username, SecretString::from(disk.secret.clone()))?
        }
        DiskCredentialKind::ApiKey => return Err(StoreError::InvalidState),
    };
    Ok(OpenSubsonicPrivateState {
        revision: disk.revision,
        backend_id: disk.backend_id.clone(),
        account_scope_id: disk.account_scope_id.clone(),
        credential,
    })
}

fn validate_credential_part(value: &str, max_bytes: usize) -> Result<(), StoreError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '\u{200b}'
                        | '\u{200c}'
                        | '\u{200d}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
    {
        return Err(StoreError::InvalidState);
    }
    Ok(())
}

fn encoded_query_value_len(value: &str) -> usize {
    value.bytes().fold(0_usize, |length, byte| {
        length.saturating_add(
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_') {
                1
            } else {
                3
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_round_trip_has_no_username() {
        let state = OpenSubsonicPrivateState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ServerCredential::api_key(SecretString::from("secret-key".to_owned())).unwrap(),
        );
        let bytes = encode_private(&state).unwrap();
        let decoded = decode_private(&bytes).unwrap();
        assert_eq!(decoded.credential_kind(), CredentialKind::ApiKey);
        assert!(decoded.credential().username().is_none());
        assert_eq!(decoded.credential().secret().expose_secret(), "secret-key");
    }

    #[test]
    fn password_round_trip_preserves_account_binding() {
        let state = OpenSubsonicPrivateState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ServerCredential::password("alice", SecretString::from("secret-password".to_owned()))
                .unwrap(),
        );
        let decoded = decode_private(&encode_private(&state).unwrap()).unwrap();
        assert_eq!(decoded.backend_id().as_str(), "backend");
        assert_eq!(decoded.account_scope_id().as_str(), "account");
        assert_eq!(
            decoded.credential().username().unwrap().expose_secret(),
            "alice"
        );
    }

    #[test]
    fn invalid_secret_shapes_fail_closed() {
        assert!(ServerCredential::password("", SecretString::from("x".to_owned())).is_err());
        assert!(ServerCredential::password("alice", SecretString::from(String::new())).is_err());
        assert!(ServerCredential::api_key(SecretString::from(String::new())).is_err());
        assert!(
            ServerCredential::api_key(SecretString::from("x".repeat(MAX_API_KEY_BYTES + 1)))
                .is_err()
        );
        assert!(
            ServerCredential::api_key(SecretString::from("가".repeat(400))).is_err(),
            "the URL-encoded key must remain below the protocol limit"
        );
        assert!(decode_private(br#"{"kind":"unknown"}"#).is_err());
    }
}
