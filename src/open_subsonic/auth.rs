//! Per-request OpenSubsonic authentication material.

use age::secrecy::ExposeSecret as _;
use data_encoding::HEXLOWER;
use md5::{Digest as _, Md5};
use zeroize::{Zeroize, Zeroizing};

use super::private_store::{CredentialKind, ServerCredential};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    RandomFailed,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("request authentication could not be prepared")
    }
}

impl std::error::Error for AuthError {}

/// Authentication query/form fields. Deliberately lacks `Debug` and `Clone`.
pub struct AuthParameters {
    fields: Vec<(String, String)>,
}

impl AuthParameters {
    pub fn fresh(credential: &ServerCredential) -> Result<Self, AuthError> {
        match credential.kind() {
            CredentialKind::ApiKey => Ok(Self {
                fields: vec![(
                    "apiKey".to_owned(),
                    credential.secret().expose_secret().to_owned(),
                )],
            }),
            CredentialKind::Password => {
                let mut salt_bytes = [0_u8; 16];
                getrandom::fill(&mut salt_bytes).map_err(|_| AuthError::RandomFailed)?;
                let salt = HEXLOWER.encode(&salt_bytes);
                let mut token_input = Zeroizing::new(Vec::with_capacity(
                    credential.secret().expose_secret().len() + salt.len(),
                ));
                token_input.extend_from_slice(credential.secret().expose_secret().as_bytes());
                token_input.extend_from_slice(salt.as_bytes());
                let token = HEXLOWER.encode(&Md5::digest(&token_input));
                Ok(Self {
                    fields: vec![
                        (
                            "u".to_owned(),
                            credential
                                .username()
                                .expect("password credentials always have a username")
                                .expose_secret()
                                .to_owned(),
                        ),
                        ("t".to_owned(), token),
                        ("s".to_owned(), salt),
                    ],
                })
            }
        }
    }

    pub(crate) fn fields(&self) -> &[(String, String)] {
        &self.fields
    }
}

impl Drop for AuthParameters {
    fn drop(&mut self) {
        for (name, value) in &mut self.fields {
            name.zeroize();
            value.zeroize();
        }
    }
}

pub(crate) fn common_parameters() -> [(&'static str, &'static str); 3] {
    [("v", "1.16.1"), ("c", "yututui"), ("f", "json")]
}

#[cfg(test)]
mod tests {
    use age::secrecy::SecretString;
    use data_encoding::HEXLOWER_PERMISSIVE;

    use super::*;

    #[test]
    fn api_key_auth_excludes_every_legacy_parameter() {
        let mut credential =
            ServerCredential::api_key(SecretString::from("api-secret".to_owned())).unwrap();
        credential.bind_api_key_username("alice").unwrap();
        let auth = AuthParameters::fresh(&credential).unwrap();
        assert_eq!(
            auth.fields(),
            &[("apiKey".to_owned(), "api-secret".to_owned())]
        );
        for forbidden in ["u", "p", "t", "s"] {
            assert!(!auth.fields().iter().any(|(name, _)| name == forbidden));
        }
    }

    #[test]
    fn password_auth_uses_fresh_salted_md5_without_cleartext_password() {
        fn field<'a>(auth: &'a AuthParameters, name: &str) -> &'a str {
            auth.fields()
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
                .unwrap()
        }

        let credential =
            ServerCredential::password("alice", SecretString::from("password-secret".to_owned()))
                .unwrap();
        let first = AuthParameters::fresh(&credential).unwrap();
        let second = AuthParameters::fresh(&credential).unwrap();
        assert_ne!(field(&first, "s"), field(&second, "s"));
        assert!(!first.fields().iter().any(|(name, _)| name == "p"));
        let salt = field(&first, "s");
        assert_eq!(
            HEXLOWER_PERMISSIVE.decode(salt.as_bytes()).unwrap().len(),
            16
        );
        let expected = HEXLOWER.encode(&Md5::digest(format!("password-secret{salt}").as_bytes()));
        assert_eq!(field(&first, "t"), expected);
    }
}
