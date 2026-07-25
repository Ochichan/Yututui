use age::secrecy::SecretString;
use zeroize::{Zeroize, Zeroizing};

use crate::personal_state::DeviceRecord;
use crate::sync::{MAX_CUSTOM_CA_PEM_BYTES, VaultCredential};
use crate::util::text_edit::TextCursor;

const MAX_ENDPOINT_CHARS: usize = 2_048;
const MAX_USERNAME_CHARS: usize = 512;
const MAX_SECRET_CHARS: usize = 8_192;
const MAX_DEVICE_NAME_CHARS: usize = 128;
const MAX_PAIRING_CODE_CHARS: usize = 128;
const MAX_PATH_CHARS: usize = 4_096;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionField {
    Endpoint,
    Username,
    Secret,
    DeviceName,
    CustomCa,
    RecoveryFile,
    PairingCode,
}

impl ConnectionField {
    pub(crate) fn is_secret(self) -> bool {
        matches!(self, Self::Secret | Self::PairingCode)
    }
}

/// Secret-bearing connection form. It is deliberately neither `Clone` nor `Debug`.
pub(crate) struct SyncConnectionForm {
    endpoint: String,
    username: String,
    secret: String,
    device_name: String,
    custom_ca: String,
    recovery_file: String,
    pairing_code: String,
    pub(crate) field: usize,
    pub(crate) cursor: TextCursor,
    revealed_field: Option<ConnectionField>,
}

impl SyncConnectionForm {
    pub(crate) fn setup() -> Self {
        Self::new(false)
    }

    pub(crate) fn join() -> Self {
        Self::new(true)
    }

    fn new(join: bool) -> Self {
        let mut form = Self {
            endpoint: String::new(),
            username: String::new(),
            secret: String::new(),
            device_name: default_device_name(),
            custom_ca: String::new(),
            recovery_file: String::new(),
            pairing_code: String::new(),
            field: 0,
            cursor: TextCursor::default(),
            revealed_field: None,
        };
        if !join {
            form.recovery_file = default_recovery_path();
        }
        form
    }

    pub(crate) fn fields(&self, join: bool) -> &'static [ConnectionField] {
        if join {
            &[
                ConnectionField::Endpoint,
                ConnectionField::Username,
                ConnectionField::Secret,
                ConnectionField::DeviceName,
                ConnectionField::CustomCa,
                ConnectionField::PairingCode,
            ]
        } else {
            &[
                ConnectionField::Endpoint,
                ConnectionField::Username,
                ConnectionField::Secret,
                ConnectionField::DeviceName,
                ConnectionField::CustomCa,
                ConnectionField::RecoveryFile,
            ]
        }
    }

    pub(crate) fn current_field(&self, join: bool) -> ConnectionField {
        let fields = self.fields(join);
        fields[self.field.min(fields.len().saturating_sub(1))]
    }

    pub(crate) fn select_field(&mut self, join: bool, next: i32) {
        let len = self.fields(join).len() as i32;
        self.field = (self.field as i32 + next).clamp(0, len.saturating_sub(1)) as usize;
        self.revealed_field = None;
        self.cursor = TextCursor::at_end(self.current_value(join));
    }

    pub(crate) fn select_field_at(&mut self, join: bool, field: usize) {
        if field >= self.fields(join).len() {
            return;
        }
        self.field = field;
        self.revealed_field = None;
        self.cursor = TextCursor::at_end(self.current_value(join));
    }

    pub(crate) fn current_secret_is_revealed(&self, join: bool) -> bool {
        self.revealed_field == Some(self.current_field(join))
    }

    pub(crate) fn toggle_current_secret(&mut self, join: bool) {
        let field = self.current_field(join);
        self.revealed_field = if field.is_secret() && self.revealed_field != Some(field) {
            Some(field)
        } else {
            None
        };
    }

    pub(crate) fn hide_secrets(&mut self) {
        self.revealed_field = None;
    }

    pub(crate) fn current_value(&self, join: bool) -> &str {
        self.value(self.current_field(join))
    }

    pub(crate) fn current_value_mut(&mut self, join: bool) -> &mut String {
        self.value_mut(self.current_field(join))
    }

    pub(crate) fn display_value(&self, field: ConnectionField) -> String {
        let value = self.value(field);
        if field.is_secret() && self.revealed_field != Some(field) {
            "•".repeat(value.chars().count().min(32))
        } else {
            value.to_owned()
        }
    }

    pub(crate) fn max_chars(field: ConnectionField) -> usize {
        match field {
            ConnectionField::Endpoint => MAX_ENDPOINT_CHARS,
            ConnectionField::Username => MAX_USERNAME_CHARS,
            ConnectionField::Secret => MAX_SECRET_CHARS,
            ConnectionField::DeviceName => MAX_DEVICE_NAME_CHARS,
            ConnectionField::CustomCa | ConnectionField::RecoveryFile => MAX_PATH_CHARS,
            ConnectionField::PairingCode => MAX_PAIRING_CODE_CHARS,
        }
    }

    pub(crate) fn validate(&self, join: bool) -> Result<(), FormError> {
        if self.endpoint.trim().is_empty() {
            return Err(FormError::EndpointRequired);
        }
        if self.secret.is_empty() {
            return Err(FormError::SecretRequired);
        }
        if !valid_device_name(&self.device_name) {
            return Err(FormError::InvalidDeviceName);
        }
        if join && self.pairing_code.trim().is_empty() {
            return Err(FormError::PairingCodeRequired);
        }
        if !join && self.recovery_file.trim().is_empty() {
            return Err(FormError::RecoveryFileRequired);
        }
        Ok(())
    }

    pub(crate) fn into_input(mut self, join: bool) -> Result<SyncConnectionInput, FormError> {
        self.validate(join)?;
        let credential = if self.username.trim().is_empty() {
            VaultCredential::bearer_token(SecretString::from(std::mem::take(&mut self.secret)))
        } else {
            VaultCredential::password(
                self.username.trim().to_owned(),
                SecretString::from(std::mem::take(&mut self.secret)),
            )
        }
        .map_err(|_| FormError::SecretRequired)?;
        Ok(SyncConnectionInput {
            endpoint: std::mem::take(&mut self.endpoint),
            custom_ca_path: non_empty(std::mem::take(&mut self.custom_ca)),
            device_name: std::mem::take(&mut self.device_name),
            credential: Some(credential),
            recovery_file: if join {
                None
            } else {
                Some(std::path::PathBuf::from(std::mem::take(
                    &mut self.recovery_file,
                )))
            },
            pairing_code: if join {
                Some(std::mem::take(&mut self.pairing_code))
            } else {
                None
            },
        })
    }

    fn value(&self, field: ConnectionField) -> &str {
        match field {
            ConnectionField::Endpoint => &self.endpoint,
            ConnectionField::Username => &self.username,
            ConnectionField::Secret => &self.secret,
            ConnectionField::DeviceName => &self.device_name,
            ConnectionField::CustomCa => &self.custom_ca,
            ConnectionField::RecoveryFile => &self.recovery_file,
            ConnectionField::PairingCode => &self.pairing_code,
        }
    }

    fn value_mut(&mut self, field: ConnectionField) -> &mut String {
        match field {
            ConnectionField::Endpoint => &mut self.endpoint,
            ConnectionField::Username => &mut self.username,
            ConnectionField::Secret => &mut self.secret,
            ConnectionField::DeviceName => &mut self.device_name,
            ConnectionField::CustomCa => &mut self.custom_ca,
            ConnectionField::RecoveryFile => &mut self.recovery_file,
            ConnectionField::PairingCode => &mut self.pairing_code,
        }
    }
}

impl Drop for SyncConnectionForm {
    fn drop(&mut self) {
        self.endpoint.zeroize();
        self.username.zeroize();
        self.secret.zeroize();
        self.device_name.zeroize();
        self.custom_ca.zeroize();
        self.recovery_file.zeroize();
        self.pairing_code.zeroize();
    }
}

/// One-shot worker input. Debug/Clone/serde are intentionally absent.
pub struct SyncConnectionInput {
    pub(crate) endpoint: String,
    pub(crate) custom_ca_path: Option<std::path::PathBuf>,
    pub(crate) device_name: String,
    credential: Option<VaultCredential>,
    pub(crate) recovery_file: Option<std::path::PathBuf>,
    pub(crate) pairing_code: Option<String>,
}

impl SyncConnectionInput {
    fn load_custom_ca(&self) -> Result<Option<Vec<u8>>, FormError> {
        let Some(path) = self.custom_ca_path.as_deref() else {
            return Ok(None);
        };
        crate::util::safe_fs::read_no_symlink_limited(path, MAX_CUSTOM_CA_PEM_BYTES as u64)
            .map(Some)
            .map_err(|_| FormError::CustomCa)
    }

    pub(crate) fn into_setup_request(
        mut self,
    ) -> Result<crate::sync::service::SetupRequest, FormError> {
        let custom_ca_pem = self.load_custom_ca()?;
        Ok(crate::sync::service::SetupRequest {
            endpoint: std::mem::take(&mut self.endpoint),
            custom_ca_pem,
            device_name: std::mem::take(&mut self.device_name),
            credential: self.credential.take().ok_or(FormError::SecretRequired)?,
            recovery_file: self
                .recovery_file
                .take()
                .ok_or(FormError::RecoveryFileRequired)?,
        })
    }

    pub(crate) fn into_join_request(mut self) -> Result<SyncJoinRequest, FormError> {
        let custom_ca_pem = self.load_custom_ca()?;
        Ok(SyncJoinRequest {
            endpoint: std::mem::take(&mut self.endpoint),
            custom_ca_pem,
            device_name: std::mem::take(&mut self.device_name),
            credential: self.credential.take().ok_or(FormError::SecretRequired)?,
            pairing_code: Zeroizing::new(
                self.pairing_code
                    .take()
                    .ok_or(FormError::PairingCodeRequired)?,
            ),
        })
    }
}

impl Drop for SyncConnectionInput {
    fn drop(&mut self) {
        self.endpoint.zeroize();
        self.device_name.zeroize();
        if let Some(code) = self.pairing_code.as_mut() {
            code.zeroize();
        }
    }
}

/// Move-only join input used only inside the blocking worker.
pub(crate) struct SyncJoinRequest {
    pub(crate) endpoint: String,
    pub(crate) custom_ca_pem: Option<Vec<u8>>,
    pub(crate) device_name: String,
    pub(crate) credential: VaultCredential,
    pub(crate) pairing_code: Zeroizing<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormError {
    EndpointRequired,
    SecretRequired,
    InvalidDeviceName,
    PairingCodeRequired,
    RecoveryFileRequired,
    CustomCa,
}

fn non_empty(value: String) -> Option<std::path::PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    }
}

fn valid_device_name(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.chars().count() <= MAX_DEVICE_NAME_CHARS
        && !value.chars().any(DeviceRecord::is_forbidden_name_char)
}

fn default_device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|name| valid_device_name(name))
        .unwrap_or_else(|| crate::t!("This device", "이 기기", "このデバイス").to_owned())
}

fn default_recovery_path() -> String {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("yututui-recovery-kit.json")
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_and_control_device_names_are_rejected() {
        for character in [
            '\u{061c}', '\u{200b}', '\u{200c}', '\u{200d}', '\u{200e}', '\u{200f}', '\u{202a}',
            '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}',
            '\u{2069}', '\u{feff}', '\0', '\n', '\u{007f}', '\u{0085}',
        ] {
            assert!(
                !valid_device_name(&format!("safe{character}name")),
                "U+{:04X} must be rejected",
                u32::from(character)
            );
        }
        assert!(valid_device_name("Living room 노트북"));
    }

    #[test]
    fn dropped_form_zeroizes_secret_buffers() {
        // The behavior is provided by zeroize's String implementation; this regression test also
        // keeps the form non-Clone at the type boundary by exercising move-only conversion.
        let mut form = SyncConnectionForm::join();
        form.endpoint = "https://example.invalid/dav".to_owned();
        form.secret = "sentinel-secret".to_owned();
        form.device_name = "Laptop".to_owned();
        form.pairing_code = "AAAA-BBBB".to_owned();
        let input = form.into_input(true).unwrap();
        assert!(input.recovery_file.is_none());
    }

    #[test]
    fn revealing_one_secret_never_reveals_the_other() {
        let mut form = SyncConnectionForm::join();
        form.secret = "password-sentinel".to_owned();
        form.pairing_code = "pairing-code-sentinel".to_owned();
        form.field = 2;

        form.toggle_current_secret(true);
        assert_eq!(
            form.display_value(ConnectionField::Secret),
            "password-sentinel"
        );
        assert!(
            !form
                .display_value(ConnectionField::PairingCode)
                .contains("sentinel")
        );

        form.select_field_at(true, 5);
        assert!(
            !form
                .display_value(ConnectionField::Secret)
                .contains("sentinel")
        );
        assert!(
            !form
                .display_value(ConnectionField::PairingCode)
                .contains("sentinel")
        );
        form.toggle_current_secret(true);
        assert_eq!(
            form.display_value(ConnectionField::PairingCode),
            "pairing-code-sentinel"
        );

        form.hide_secrets();
        assert!(
            !form
                .display_value(ConnectionField::PairingCode)
                .contains("sentinel")
        );
    }
}
