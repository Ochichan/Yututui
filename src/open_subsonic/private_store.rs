//! Owner-only OpenSubsonic credentials.

use age::secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::model::{AccountScopeId, BackendId};
use super::native_history::NativeHistoryCredential;
use super::profile::StoreError;

pub(crate) const PRIVATE_KIND: &str = "yututui_open_subsonic_private";
pub(crate) const PRIVATE_SCHEMA_VERSION: u32 = 3;
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

    /// Attach the account identity proven by the authenticated `tokenInfo` endpoint.
    ///
    /// The username is private ownership evidence only. API-key requests continue to omit the
    /// legacy `u` authentication parameter as required by the OpenSubsonic extension.
    pub(crate) fn bind_api_key_username(
        &mut self,
        username: impl Into<String>,
    ) -> Result<(), StoreError> {
        if self.kind != CredentialKind::ApiKey {
            return Err(StoreError::InvalidState);
        }
        let username = username.into();
        validate_credential_part(&username, MAX_USERNAME_BYTES)?;
        self.username = Some(SecretString::from(username));
        Ok(())
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
    native_history: NativeHistorySetting,
}

enum NativeHistorySetting {
    Off,
    ReuseServerPassword,
    DedicatedPassword {
        username: SecretString,
        password: SecretString,
    },
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
            native_history: NativeHistorySetting::Off,
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

    pub fn native_history_enabled(&self) -> bool {
        !matches!(self.native_history, NativeHistorySetting::Off)
    }

    pub(crate) fn bind_api_key_username(
        &mut self,
        username: impl Into<String>,
    ) -> Result<(), StoreError> {
        self.credential.bind_api_key_username(username)
    }

    pub fn enable_native_history_reusing_server_password(&mut self) -> Result<(), StoreError> {
        if self.credential.kind() != CredentialKind::Password {
            return Err(StoreError::InvalidState);
        }
        self.native_history = NativeHistorySetting::ReuseServerPassword;
        Ok(())
    }

    pub fn enable_native_history_with_password(
        &mut self,
        username: impl Into<String>,
        password: SecretString,
    ) -> Result<(), StoreError> {
        if self.credential.kind() != CredentialKind::ApiKey {
            return Err(StoreError::InvalidState);
        }
        let username = username.into();
        validate_credential_part(&username, MAX_USERNAME_BYTES)?;
        validate_credential_part(password.expose_secret(), MAX_PASSWORD_BYTES)?;
        self.native_history = NativeHistorySetting::DedicatedPassword {
            username: SecretString::from(username),
            password,
        };
        Ok(())
    }

    pub fn disable_native_history(&mut self) {
        self.native_history = NativeHistorySetting::Off;
    }

    pub(crate) fn preserve_native_history_from(
        &mut self,
        previous: &Self,
    ) -> Result<(), StoreError> {
        if self.backend_id != previous.backend_id
            || self.account_scope_id != previous.account_scope_id
        {
            return Err(StoreError::InvalidState);
        }
        match (self.credential.kind(), &previous.native_history) {
            (_, NativeHistorySetting::Off) => {
                self.disable_native_history();
                Ok(())
            }
            (CredentialKind::Password, _) => self.enable_native_history_reusing_server_password(),
            (CredentialKind::ApiKey, NativeHistorySetting::ReuseServerPassword) => self
                .enable_native_history_with_password(
                    previous
                        .credential
                        .username()
                        .ok_or(StoreError::InvalidState)?
                        .expose_secret()
                        .to_owned(),
                    SecretString::from(previous.credential.secret().expose_secret().to_owned()),
                ),
            (
                CredentialKind::ApiKey,
                NativeHistorySetting::DedicatedPassword { username, password },
            ) => self.enable_native_history_with_password(
                username.expose_secret().to_owned(),
                SecretString::from(password.expose_secret().to_owned()),
            ),
        }
    }

    /// Copies the selected password into an ephemeral, non-serializable login value.
    pub fn native_history_credential(&self) -> Result<Option<NativeHistoryCredential>, StoreError> {
        let (username, password) = match &self.native_history {
            NativeHistorySetting::Off => return Ok(None),
            NativeHistorySetting::ReuseServerPassword => (
                self.credential.username().ok_or(StoreError::InvalidState)?,
                self.credential.secret(),
            ),
            NativeHistorySetting::DedicatedPassword { username, password } => (username, password),
        };
        NativeHistoryCredential::new(
            username.expose_secret().to_owned(),
            SecretString::from(password.expose_secret().to_owned()),
        )
        .map(Some)
        .map_err(|_| StoreError::InvalidState)
    }

    pub(crate) fn credential(&self) -> &ServerCredential {
        &self.credential
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(default)]
    native_history_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_history_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_history_password: Option<String>,
}

impl Drop for DiskPrivateState {
    fn drop(&mut self) {
        if let Some(username) = &mut self.username {
            username.zeroize();
        }
        self.secret.zeroize();
        if let Some(username) = &mut self.native_history_username {
            username.zeroize();
        }
        if let Some(password) = &mut self.native_history_password {
            password.zeroize();
        }
    }
}

pub(crate) fn encode_private(
    state: &OpenSubsonicPrivateState,
) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let (native_history_enabled, native_history_username, native_history_password) =
        match &state.native_history {
            NativeHistorySetting::Off => (false, None, None),
            NativeHistorySetting::ReuseServerPassword => (true, None, None),
            NativeHistorySetting::DedicatedPassword { username, password } => (
                true,
                Some(username.expose_secret().to_owned()),
                Some(password.expose_secret().to_owned()),
            ),
        };
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
        native_history_enabled,
        native_history_username,
        native_history_password,
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
    if disk.kind != PRIVATE_KIND || !matches!(disk.schema_version, 1 | 2 | PRIVATE_SCHEMA_VERSION) {
        return Err(StoreError::InvalidState);
    }
    if disk.schema_version == 1
        && (disk.native_history_enabled
            || disk.native_history_username.is_some()
            || disk.native_history_password.is_some())
    {
        return Err(StoreError::InvalidState);
    }
    let credential = match disk.credential_kind {
        DiskCredentialKind::ApiKey
            if disk.schema_version < PRIVATE_SCHEMA_VERSION && disk.username.is_some() =>
        {
            return Err(StoreError::InvalidState);
        }
        DiskCredentialKind::ApiKey => {
            let mut credential =
                ServerCredential::api_key(SecretString::from(disk.secret.clone()))?;
            if let Some(username) = disk.username.clone() {
                credential.bind_api_key_username(username)?;
            }
            credential
        }
        DiskCredentialKind::Password => {
            let username = disk.username.clone().ok_or(StoreError::InvalidState)?;
            ServerCredential::password(username, SecretString::from(disk.secret.clone()))?
        }
    };
    let native_history = match (
        disk.credential_kind,
        disk.native_history_enabled,
        disk.native_history_username.as_deref(),
        disk.native_history_password.as_deref(),
    ) {
        (_, false, None, None) => NativeHistorySetting::Off,
        (DiskCredentialKind::Password, true, None, None) => {
            NativeHistorySetting::ReuseServerPassword
        }
        (DiskCredentialKind::ApiKey, true, Some(username), Some(password)) => {
            validate_credential_part(username, MAX_USERNAME_BYTES)?;
            validate_credential_part(password, MAX_PASSWORD_BYTES)?;
            NativeHistorySetting::DedicatedPassword {
                username: SecretString::from(username.to_owned()),
                password: SecretString::from(password.to_owned()),
            }
        }
        _ => return Err(StoreError::InvalidState),
    };
    Ok(OpenSubsonicPrivateState {
        revision: disk.revision,
        backend_id: disk.backend_id.clone(),
        account_scope_id: disk.account_scope_id.clone(),
        credential,
        native_history,
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
    fn token_info_username_round_trips_as_private_api_key_owner_evidence() {
        let mut credential =
            ServerCredential::api_key(SecretString::from("secret-key".to_owned())).unwrap();
        credential.bind_api_key_username("alice").unwrap();
        let state = OpenSubsonicPrivateState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            credential,
        );

        let bytes = encode_private(&state).unwrap();
        let disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(disk["schema_version"], PRIVATE_SCHEMA_VERSION);
        assert_eq!(disk["username"], "alice");
        let decoded = decode_private(&bytes).unwrap();
        assert_eq!(decoded.credential_kind(), CredentialKind::ApiKey);
        assert_eq!(
            decoded.credential().username().unwrap().expose_secret(),
            "alice"
        );
    }

    #[test]
    fn password_round_trip_preserves_account_binding() {
        let mut state = OpenSubsonicPrivateState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ServerCredential::password("alice", SecretString::from("secret-password".to_owned()))
                .unwrap(),
        );
        state
            .enable_native_history_reusing_server_password()
            .unwrap();
        let decoded = decode_private(&encode_private(&state).unwrap()).unwrap();
        assert_eq!(decoded.backend_id().as_str(), "backend");
        assert_eq!(decoded.account_scope_id().as_str(), "account");
        assert_eq!(
            decoded.credential().username().unwrap().expose_secret(),
            "alice"
        );
        assert!(decoded.native_history_enabled());
        assert!(decoded.native_history_credential().unwrap().is_some());
        let disk: serde_json::Value = serde_json::from_slice(&encode_private(&state).unwrap())
            .expect("private state should be valid JSON");
        assert_eq!(disk["schema_version"], PRIVATE_SCHEMA_VERSION);
        assert_eq!(disk["native_history_enabled"], true);
        assert!(disk.get("native_history_password").is_none());
    }

    #[test]
    fn api_key_history_password_round_trips_and_disable_clears_it() {
        let mut state = OpenSubsonicPrivateState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ServerCredential::api_key(SecretString::from("api-secret".to_owned())).unwrap(),
        );
        assert!(
            state
                .enable_native_history_reusing_server_password()
                .is_err()
        );
        state
            .enable_native_history_with_password(
                "alice",
                SecretString::from("native-password".to_owned()),
            )
            .unwrap();
        let decoded = decode_private(&encode_private(&state).unwrap()).unwrap();
        assert!(decoded.native_history_enabled());
        assert!(decoded.native_history_credential().unwrap().is_some());

        state.disable_native_history();
        let bytes = encode_private(&state).unwrap();
        let disk: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(disk["native_history_enabled"], false);
        assert!(disk.get("native_history_username").is_none());
        assert!(disk.get("native_history_password").is_none());
        assert!(!decode_private(&bytes).unwrap().native_history_enabled());
    }

    #[test]
    fn schema_one_migrates_to_current_schema_with_history_disabled() {
        let legacy = br#"{
            "kind":"yututui_open_subsonic_private",
            "schema_version":1,
            "revision":7,
            "backend_id":"backend",
            "account_scope_id":"account",
            "credential_kind":"password",
            "username":"alice",
            "secret":"legacy-password"
        }"#;
        let state = decode_private(legacy).unwrap();
        assert!(!state.native_history_enabled());
        let encoded = encode_private(&state).unwrap();
        let disk: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(disk["schema_version"], PRIVATE_SCHEMA_VERSION);
        assert_eq!(disk["native_history_enabled"], false);
    }

    #[test]
    fn schema_two_api_key_without_owner_evidence_remains_readable() {
        let legacy = br#"{
            "kind":"yututui_open_subsonic_private",
            "schema_version":2,
            "revision":7,
            "backend_id":"backend",
            "account_scope_id":"account",
            "credential_kind":"api_key",
            "username":null,
            "secret":"legacy-api-key",
            "native_history_enabled":false
        }"#;

        let state = decode_private(legacy).unwrap();
        assert_eq!(state.credential_kind(), CredentialKind::ApiKey);
        assert!(state.credential().username().is_none());
    }

    #[test]
    fn malformed_native_history_combinations_fail_closed() {
        let state = OpenSubsonicPrivateState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ServerCredential::api_key(SecretString::from("api-secret".to_owned())).unwrap(),
        );
        let mut disk: serde_json::Value =
            serde_json::from_slice(&encode_private(&state).unwrap()).unwrap();
        disk["native_history_enabled"] = true.into();
        assert!(decode_private(&serde_json::to_vec(&disk).unwrap()).is_err());

        disk["schema_version"] = 1.into();
        disk["native_history_username"] = "alice".into();
        disk["native_history_password"] = "native-password".into();
        assert!(decode_private(&serde_json::to_vec(&disk).unwrap()).is_err());
    }

    #[test]
    fn same_account_credential_changes_preserve_history_without_reusing_api_keys() {
        let backend = BackendId::new("backend").unwrap();
        let account = AccountScopeId::new("account").unwrap();
        let mut password_state = OpenSubsonicPrivateState::new(
            backend.clone(),
            account.clone(),
            ServerCredential::password(
                "alice",
                SecretString::from("native-capable-password".to_owned()),
            )
            .unwrap(),
        );
        password_state
            .enable_native_history_reusing_server_password()
            .unwrap();

        let mut api_key_state = OpenSubsonicPrivateState::new(
            backend.clone(),
            account.clone(),
            ServerCredential::api_key(SecretString::from("api-key".to_owned())).unwrap(),
        );
        api_key_state
            .preserve_native_history_from(&password_state)
            .unwrap();
        assert!(api_key_state.native_history_enabled());
        assert!(api_key_state.native_history_credential().unwrap().is_some());

        let mut replacement_password = OpenSubsonicPrivateState::new(
            backend,
            account,
            ServerCredential::password(
                "alice",
                SecretString::from("replacement-password".to_owned()),
            )
            .unwrap(),
        );
        replacement_password
            .preserve_native_history_from(&api_key_state)
            .unwrap();
        let disk: serde_json::Value =
            serde_json::from_slice(&encode_private(&replacement_password).unwrap()).unwrap();
        assert_eq!(disk["native_history_enabled"], true);
        assert!(disk.get("native_history_password").is_none());
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
