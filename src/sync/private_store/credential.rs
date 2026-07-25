//! Move-only WebDAV credentials owned by the local private store.

use age::secrecy::{ExposeSecret, SecretString};

use super::{MAX_CREDENTIAL_BYTES, MAX_USERNAME_BYTES, VaultError, validate_credential_part};

/// Authentication form used by the state-vault WebDAV endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultCredentialKind {
    Password,
    BearerToken,
}

/// A WebDAV credential whose secret values have no `Debug`, `Clone`, or serde implementation.
pub struct VaultCredential {
    kind: VaultCredentialKind,
    username: Option<SecretString>,
    secret: SecretString,
}

impl VaultCredential {
    pub fn password(
        username: impl Into<String>,
        password: SecretString,
    ) -> Result<Self, VaultError> {
        let username = username.into();
        validate_credential_part(&username, MAX_USERNAME_BYTES, false)?;
        validate_credential_part(password.expose_secret(), MAX_CREDENTIAL_BYTES, true)?;
        Ok(Self {
            kind: VaultCredentialKind::Password,
            username: Some(SecretString::from(username)),
            secret: password,
        })
    }

    pub fn bearer_token(token: SecretString) -> Result<Self, VaultError> {
        validate_credential_part(token.expose_secret(), MAX_CREDENTIAL_BYTES, true)?;
        Ok(Self {
            kind: VaultCredentialKind::BearerToken,
            username: None,
            secret: token,
        })
    }

    pub fn kind(&self) -> VaultCredentialKind {
        self.kind
    }

    pub fn username(&self) -> Option<&SecretString> {
        self.username.as_ref()
    }

    pub fn secret(&self) -> &SecretString {
        &self.secret
    }
}
