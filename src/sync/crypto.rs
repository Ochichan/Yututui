use std::collections::BTreeSet;
use std::fmt;
use std::io::{Read, Write};
use std::str::FromStr;

use age::secrecy::{ExposeSecret, SecretString};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use data_encoding::{BASE32_NOPAD_VISUAL, HEXLOWER};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

pub(crate) use crate::personal_state::DevicePublicIdentity;
use crate::personal_state::DeviceRecord;

use super::error::VaultError;

/// Maximum authenticated plaintext accepted from an encrypted state object.
pub const MAX_PROTECTED_PAYLOAD_BYTES: usize = 192 * 1024 * 1024;

/// Maximum binary age object accepted before parsing its header.
pub const MAX_ENCRYPTED_OBJECT_BYTES: usize = MAX_PROTECTED_PAYLOAD_BYTES + 4 * 1024 * 1024;

/// At most 256 active devices plus the offline recovery recipient.
pub const MAX_AGE_RECIPIENTS: usize = 257;

const MAX_DEVICE_ID_BYTES: usize = 128;
const MAX_DATASET_ID_BYTES: usize = 128;
const MAX_RANDOM_BYTES: usize = 64;
const INITIAL_READ_CAPACITY: usize = 1024 * 1024;
const PAIRING_CODE_BYTES: usize = 16;
const PAIRING_CODE_CHARS: usize = 26;
const PAIRING_CODE_GROUP_CHARS: usize = 5;
const PAIRING_SCRYPT_LOG_N: u8 = 15;
const SIGNED_OBJECT_PREFIX: &[u8] = b"yututui/signed-object/v1\0";
const HASH_PREFIX: &[u8] = b"yututui/domain-hash/v1\0";
const DEVICE_FINGERPRINT_DOMAIN: &[u8] = b"yututui/device-fingerprint/v1";
const RECOVERY_KDF_SALT_DOMAIN: &[u8] = b"yututui/recovery-signing-salt/v1";
const RECOVERY_KDF_INFO: &[u8] = b"yututui/recovery-signing-key/v1";
const PAIRING_PROOF_DOMAIN: &[u8] = b"yututui/pairing-proof/v1";

type HmacSha256 = Hmac<Sha256>;

/// A device identifier bound to its exact public keys and display fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePublicRecord {
    pub device_id: String,
    pub identity: DevicePublicIdentity,
    pub fingerprint: String,
}

impl DevicePublicRecord {
    pub fn validate(&self) -> Result<(), VaultError> {
        validate_device_id(&self.device_id)?;
        validate_personal_identity(&self.identity)?;
        if self.fingerprint != device_identity_fingerprint(&self.identity) {
            return Err(VaultError::InvalidSigningKey);
        }
        Ok(())
    }
}

/// Owner-only encryption and signing material for one device.
///
/// Secret fields intentionally implement neither `Serialize` nor `Clone`, and its
/// custom `Debug` representation never renders key material.
pub struct DeviceSecretMaterial {
    device_id: String,
    age_identity: age::x25519::Identity,
    signing_key: SigningKey,
}

impl fmt::Debug for DeviceSecretMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceSecretMaterial")
            .field("device_id", &self.device_id)
            .field("age_identity", &"[REDACTED]")
            .field("signing_key", &"[REDACTED]")
            .finish()
    }
}

impl DeviceSecretMaterial {
    /// Generates independent age and Ed25519 keys for a canonical device identifier.
    pub fn generate_for(device_id: impl Into<String>) -> Result<Self, VaultError> {
        let device_id = device_id.into();
        validate_device_id(&device_id)?;

        let age_identity = age::x25519::Identity::generate();
        let seed = secure_random_bytes::<32>()?;
        let signing_key = SigningKey::from_bytes(&seed);
        if signing_key.verifying_key().is_weak() {
            return Err(VaultError::WeakSigningKey);
        }

        Ok(Self {
            device_id,
            age_identity,
            signing_key,
        })
    }

    /// Rehydrates owner-only key material while enforcing canonical encodings.
    pub fn from_encoded(
        device_id: impl Into<String>,
        age_identity: SecretString,
        signing_key_b64: &SecretString,
    ) -> Result<Self, VaultError> {
        let device_id = device_id.into();
        validate_device_id(&device_id)?;

        let parsed_identity = age::x25519::Identity::from_str(age_identity.expose_secret())
            .map_err(|_| VaultError::InvalidAgeIdentity)?;
        let canonical_identity = parsed_identity.to_string();
        if canonical_identity.expose_secret() != age_identity.expose_secret() {
            return Err(VaultError::InvalidAgeIdentity);
        }

        let mut seed = decode_fixed_base64::<32>(
            signing_key_b64.expose_secret(),
            VaultError::InvalidSigningKey,
        )?;
        let signing_key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        if signing_key.verifying_key().is_weak() {
            return Err(VaultError::WeakSigningKey);
        }

        Ok(Self {
            device_id,
            age_identity: parsed_identity,
            signing_key,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn public_identity(&self) -> DevicePublicIdentity {
        DevicePublicIdentity {
            age_recipient: self.age_identity.to_public().to_string(),
            ed25519_verifying_key: verifying_key_to_base64(&self.signing_key.verifying_key()),
        }
    }

    pub fn public_record(&self) -> DevicePublicRecord {
        let identity = self.public_identity();
        let fingerprint = device_identity_fingerprint(&identity);
        DevicePublicRecord {
            device_id: self.device_id.clone(),
            identity,
            fingerprint,
        }
    }

    /// Uses exact canonical strings so aliases cannot silently replace approved keys.
    pub fn matches_public_record(&self, record: &DevicePublicRecord) -> bool {
        self.public_record() == *record
    }

    /// Matches canonical ledger keys exactly without considering the display name.
    pub fn matches_record(&self, record: &DeviceRecord) -> bool {
        record.device_id.as_str() == self.device_id
            && !record.revoked
            && record.public_identity.as_ref() == Some(&self.public_identity())
    }

    pub fn matches_personal_record(&self, record: &DeviceRecord) -> bool {
        self.matches_record(record)
    }

    pub fn fingerprint(&self) -> String {
        device_identity_fingerprint(&self.public_identity())
    }

    pub fn age_identity_secret(&self) -> SecretString {
        self.age_identity.to_string()
    }

    pub fn signing_key_secret_b64(&self) -> SecretString {
        let mut seed = self.signing_key.to_bytes();
        let encoded = base64url_encode(&seed);
        seed.zeroize();
        SecretString::from(encoded)
    }

    pub fn age_identity(&self) -> &age::x25519::Identity {
        &self.age_identity
    }

    pub(crate) fn signing_key(&self) -> &SigningKey {
        &self.signing_key
    }
}

/// A 128-bit pairing secret that stays out of serde and ordinary formatting.
#[derive(Clone)]
pub struct PairingCode(SecretString);

impl PairingCode {
    pub fn generate() -> Result<Self, VaultError> {
        generate_pairing_code()
    }

    pub fn parse(encoded: &str) -> Result<Self, VaultError> {
        let secret = SecretString::from(encoded);
        let decoded = decode_pairing_code_secret(&secret)?;
        Ok(Self(group_pairing_code(
            &BASE32_NOPAD_VISUAL.encode(&decoded),
        )))
    }

    /// Explicit access for a user-facing copy or reveal action.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PairingCode").field(&"[REDACTED]").finish()
    }
}

/// Opaque binary age ciphertext.
///
/// It is deliberately not serde-serializable, avoiding accidental JSON byte arrays or
/// inclusion in status snapshots. WebDAV and file transports use the raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CiphertextProvenance {
    ProducedLocally,
    UntrustedInput,
}

#[derive(Clone)]
pub struct EncryptedObject {
    bytes: Vec<u8>,
    provenance: CiphertextProvenance,
}

impl EncryptedObject {
    /// Parse size-bounded, age-shaped bytes received from storage.
    ///
    /// The result remains untrusted and cannot be uploaded by a vault transport. Only a full
    /// decrypt-and-verify operation authenticates its payload.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, VaultError> {
        Self::from_bytes_with_provenance(bytes, CiphertextProvenance::UntrustedInput)
    }

    fn produced_locally(bytes: Vec<u8>) -> Result<Self, VaultError> {
        Self::from_bytes_with_provenance(bytes, CiphertextProvenance::ProducedLocally)
    }

    fn from_bytes_with_provenance(
        bytes: Vec<u8>,
        provenance: CiphertextProvenance,
    ) -> Result<Self, VaultError> {
        if bytes.is_empty() || bytes.len() > MAX_ENCRYPTED_OBJECT_BYTES {
            return Err(VaultError::InvalidEncryptedObject);
        }
        // This is structural admission only. The header and payload are authenticated later,
        // when a recipient identity decrypts the complete stream through EOF.
        age::Decryptor::new_buffered(bytes.as_slice())
            .map_err(|_| VaultError::InvalidEncryptedObject)?;
        Ok(Self { bytes, provenance })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn is_locally_produced(&self) -> bool {
        self.provenance == CiphertextProvenance::ProducedLocally
    }
}

impl PartialEq for EncryptedObject {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for EncryptedObject {}

impl AsRef<[u8]> for EncryptedObject {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for EncryptedObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedObject")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Returns a bounded, zeroizing buffer filled directly by the operating system RNG.
pub fn secure_random_bytes<const N: usize>() -> Result<Zeroizing<[u8; N]>, VaultError> {
    if N == 0 || N > MAX_RANDOM_BYTES {
        return Err(VaultError::InvalidRandomLength);
    }
    let mut bytes = Zeroizing::new([0_u8; N]);
    getrandom::fill(&mut *bytes).map_err(|_| VaultError::RandomnessUnavailable)?;
    Ok(bytes)
}

/// Generates a lowercase hexadecimal identifier from OS randomness.
pub fn random_id_hex<const N: usize>() -> Result<String, VaultError> {
    let bytes = secure_random_bytes::<N>()?;
    Ok(HEXLOWER.encode(bytes.as_ref()))
}

/// URL-safe, unpadded Base64 used for public keys, signatures, and proofs.
pub fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes only canonical URL-safe, unpadded Base64.
pub fn base64url_decode(encoded: &str) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let max_encoded_len = MAX_PROTECTED_PAYLOAD_BYTES
        .saturating_add(2)
        .saturating_div(3)
        .saturating_mul(4);
    if encoded.len() > max_encoded_len {
        return Err(VaultError::PayloadTooLarge);
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| VaultError::InvalidBase64)?;
    if URL_SAFE_NO_PAD.encode(&decoded) != encoded {
        return Err(VaultError::InvalidBase64);
    }
    Ok(Zeroizing::new(decoded))
}

/// Human-readable, no-padding Base32 with visual-error correction on decode.
pub fn base32_visual_encode(bytes: &[u8]) -> String {
    BASE32_NOPAD_VISUAL.encode(bytes)
}

/// Decodes a visual Base32 value after removing ASCII whitespace and hyphens.
pub fn base32_visual_decode(encoded: &str) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let normalized = normalize_visual_base32(encoded)?;
    let decoded = BASE32_NOPAD_VISUAL
        .decode(normalized.as_bytes())
        .map_err(|_| VaultError::InvalidBase32)?;
    Ok(Zeroizing::new(decoded))
}

/// Generates a 128-bit, grouped pairing code. Its `Debug` output is redacted.
pub fn generate_pairing_code() -> Result<PairingCode, VaultError> {
    let bytes = secure_random_bytes::<PAIRING_CODE_BYTES>()?;
    let encoded = base32_visual_encode(bytes.as_ref());
    Ok(PairingCode(group_pairing_code(&encoded)))
}

fn group_pairing_code(encoded: &str) -> SecretString {
    let mut grouped = Zeroizing::new(String::with_capacity(
        encoded.len() + encoded.len() / PAIRING_CODE_GROUP_CHARS,
    ));
    for (index, character) in encoded.chars().enumerate() {
        if index > 0 && index % PAIRING_CODE_GROUP_CHARS == 0 {
            grouped.push('-');
        }
        grouped.push(character);
    }
    SecretString::from(std::mem::take(&mut *grouped))
}

/// Creates a constant-time-verifiable proof that the joining device knows the code.
pub fn pairing_proof_b64(
    pairing_code: &PairingCode,
    challenge: &[u8],
) -> Result<String, VaultError> {
    if challenge.len() > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    let code = decode_pairing_code(pairing_code)?;
    let mut mac =
        HmacSha256::new_from_slice(code.as_ref()).map_err(|_| VaultError::PairingProofFailed)?;
    update_framed(&mut mac, PAIRING_PROOF_DOMAIN, challenge);
    let mut proof = mac.finalize().into_bytes();
    let encoded = base64url_encode(&proof);
    proof.zeroize();
    Ok(encoded)
}

pub fn verify_pairing_proof(
    pairing_code: &PairingCode,
    challenge: &[u8],
    proof_b64: &str,
) -> Result<(), VaultError> {
    if challenge.len() > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    let code = decode_pairing_code(pairing_code)?;
    let proof = decode_fixed_base64::<32>(proof_b64, VaultError::PairingProofFailed)?;
    let mut mac =
        HmacSha256::new_from_slice(code.as_ref()).map_err(|_| VaultError::PairingProofFailed)?;
    update_framed(&mut mac, PAIRING_PROOF_DOMAIN, challenge);
    mac.verify_slice(proof.as_ref())
        .map_err(|_| VaultError::PairingProofFailed)
}

/// Hashes unambiguous length-delimited parts below a protocol-specific domain.
pub fn sha256_domain_hex(domain: &[u8], parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    Digest::update(&mut digest, HASH_PREFIX);
    digest_part(&mut digest, domain);
    for part in parts {
        digest_part(&mut digest, part);
    }
    HEXLOWER.encode(&digest.finalize())
}

/// Signs canonical JSON below an explicit protocol domain.
pub fn sign_serializable<T: Serialize + ?Sized>(
    domain: &'static [u8],
    key: &SigningKey,
    value: &T,
) -> Result<String, VaultError> {
    let payload = canonical_json_bytes(value)?;
    let message = signature_message(domain, &payload);
    let signature = key.sign(&message);
    Ok(base64url_encode(&signature.to_bytes()))
}

/// Verifies canonical JSON with Ed25519 strict verification and canonical encodings.
pub fn verify_serializable<T: Serialize + ?Sized>(
    domain: &'static [u8],
    public_key_b64: &str,
    value: &T,
    signature_b64: &str,
) -> Result<(), VaultError> {
    let key = decode_verifying_key(public_key_b64)?;
    let signature_bytes = decode_fixed_base64::<64>(signature_b64, VaultError::InvalidSignature)?;
    let signature = Signature::from_slice(signature_bytes.as_ref())
        .map_err(|_| VaultError::InvalidSignature)?;
    let payload = canonical_json_bytes(value)?;
    let message = signature_message(domain, &payload);
    key.verify_strict(&message, &signature)
        .map_err(|_| VaultError::SignatureVerificationFailed)
}

/// Encrypts canonical JSON to every active device and recovery age recipient.
pub fn encrypt_json_to_recipients<T: Serialize + ?Sized>(
    value: &T,
    recipient_strings: &[String],
) -> Result<EncryptedObject, VaultError> {
    let plaintext = canonical_json_bytes(value)?;
    let recipients = parse_recipients(recipient_strings)?;
    encrypt_bytes_to_recipients(&plaintext, &recipients)
}

/// Authenticates the complete age stream before attempting to parse its JSON payload.
pub fn decrypt_json_with_identity<T: DeserializeOwned>(
    object: &EncryptedObject,
    identity: &age::x25519::Identity,
) -> Result<T, VaultError> {
    let plaintext = decrypt_bytes_with_identity(object, identity)?;
    serde_json::from_slice(&plaintext).map_err(|_| VaultError::SerializationFailed)
}

/// Seals a short-lived pairing payload with age's standard scrypt recipient format.
pub fn seal_pairing_json<T: Serialize + ?Sized>(
    pairing_code: &PairingCode,
    value: &T,
) -> Result<EncryptedObject, VaultError> {
    let plaintext = canonical_json_bytes(value)?;
    let passphrase = canonical_pairing_passphrase(pairing_code)?;
    let mut recipient = age::scrypt::Recipient::new(passphrase);
    recipient.set_work_factor(PAIRING_SCRYPT_LOG_N);
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|_| VaultError::EncryptionFailed)?;
    encrypt_with_encryptor(encryptor, &plaintext)
}

/// Opens a pairing payload while refusing attacker-selected expensive scrypt work.
pub fn open_pairing_json<T: DeserializeOwned>(
    pairing_code: &PairingCode,
    object: &EncryptedObject,
) -> Result<T, VaultError> {
    let passphrase = canonical_pairing_passphrase(pairing_code)?;
    let mut identity = age::scrypt::Identity::new(passphrase);
    identity.set_max_work_factor(PAIRING_SCRYPT_LOG_N);
    let plaintext = decrypt_bytes_with_dyn_identity(object, &identity)?;
    serde_json::from_slice(&plaintext).map_err(|_| VaultError::SerializationFailed)
}

/// Deterministically derives the recovery signer from the canonical recovery age key
/// and dataset identity. Temporary secret material is zeroized on every return path.
pub fn derive_recovery_signing_key(
    recovery_identity: &age::x25519::Identity,
    dataset_id: &str,
) -> Result<SigningKey, VaultError> {
    validate_dataset_id(dataset_id)?;
    let recovery_secret = recovery_identity.to_string();
    let salt_hex = sha256_domain_hex(RECOVERY_KDF_SALT_DOMAIN, &[dataset_id.as_bytes()]);
    let hkdf = Hkdf::<Sha256>::new(
        Some(salt_hex.as_bytes()),
        recovery_secret.expose_secret().as_bytes(),
    );
    let mut seed = Zeroizing::new([0_u8; 32]);
    hkdf.expand(RECOVERY_KDF_INFO, &mut *seed)
        .map_err(|_| VaultError::KeyDerivationFailed)?;
    let signing_key = SigningKey::from_bytes(&seed);
    if signing_key.verifying_key().is_weak() {
        return Err(VaultError::WeakSigningKey);
    }
    Ok(signing_key)
}

pub(crate) fn validate_device_id(device_id: &str) -> Result<(), VaultError> {
    if !valid_vault_identifier(device_id, MAX_DEVICE_ID_BYTES) {
        return Err(VaultError::InvalidDeviceId);
    }
    Ok(())
}

pub(crate) fn validate_dataset_id(dataset_id: &str) -> Result<(), VaultError> {
    if !valid_vault_identifier(dataset_id, MAX_DATASET_ID_BYTES) {
        return Err(VaultError::InvalidDatasetId);
    }
    Ok(())
}

fn valid_vault_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn parse_canonical_age_recipient(encoded: &str) -> Result<age::x25519::Recipient, VaultError> {
    let recipient =
        age::x25519::Recipient::from_str(encoded).map_err(|_| VaultError::InvalidAgeRecipient)?;
    if recipient.to_string() != encoded {
        return Err(VaultError::InvalidAgeRecipient);
    }
    Ok(recipient)
}

pub(crate) fn validate_age_recipient(encoded: &str) -> Result<(), VaultError> {
    parse_canonical_age_recipient(encoded).map(|_| ())
}

fn decode_verifying_key(encoded: &str) -> Result<VerifyingKey, VaultError> {
    let bytes = decode_fixed_base64::<32>(encoded, VaultError::InvalidSigningKey)?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| VaultError::InvalidSigningKey)?;
    if key.is_weak() {
        return Err(VaultError::WeakSigningKey);
    }
    Ok(key)
}

pub(crate) fn verifying_key_from_base64(encoded: &str) -> Result<VerifyingKey, VaultError> {
    decode_verifying_key(encoded)
}

pub(crate) fn verifying_key_to_base64(key: &VerifyingKey) -> String {
    base64url_encode(key.as_bytes())
}

fn validate_personal_identity(identity: &DevicePublicIdentity) -> Result<(), VaultError> {
    validate_age_recipient(&identity.age_recipient)?;
    verifying_key_from_base64(&identity.ed25519_verifying_key)?;
    Ok(())
}

fn device_identity_fingerprint(identity: &DevicePublicIdentity) -> String {
    sha256_domain_hex(
        DEVICE_FINGERPRINT_DOMAIN,
        &[
            identity.age_recipient.as_bytes(),
            identity.ed25519_verifying_key.as_bytes(),
        ],
    )
}

fn decode_fixed_base64<const N: usize>(
    encoded: &str,
    error: VaultError,
) -> Result<Zeroizing<[u8; N]>, VaultError> {
    let decoded = base64url_decode(encoded).map_err(|_| error)?;
    let mut bytes = Zeroizing::new([0_u8; N]);
    if decoded.len() != N {
        return Err(error);
    }
    bytes.copy_from_slice(&decoded);
    Ok(bytes)
}

fn normalize_visual_base32(encoded: &str) -> Result<Zeroizing<String>, VaultError> {
    if encoded.len() > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    let mut normalized = Zeroizing::new(String::with_capacity(encoded.len()));
    for character in encoded.chars() {
        if character == '-' || character.is_ascii_whitespace() {
            continue;
        }
        if !character.is_ascii_alphanumeric() {
            return Err(VaultError::InvalidBase32);
        }
        normalized.push(character.to_ascii_uppercase());
    }
    if normalized.is_empty() {
        return Err(VaultError::InvalidBase32);
    }
    Ok(normalized)
}

fn decode_pairing_code(pairing_code: &PairingCode) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    decode_pairing_code_secret(&pairing_code.0)
}

fn decode_pairing_code_secret(
    pairing_code: &SecretString,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if pairing_code.expose_secret().len() > 64 {
        return Err(VaultError::InvalidPairingCode);
    }
    let normalized = normalize_visual_base32(pairing_code.expose_secret())
        .map_err(|_| VaultError::InvalidPairingCode)?;
    if normalized.len() != PAIRING_CODE_CHARS {
        return Err(VaultError::InvalidPairingCode);
    }
    let decoded = BASE32_NOPAD_VISUAL
        .decode(normalized.as_bytes())
        .map_err(|_| VaultError::InvalidPairingCode)?;
    if decoded.len() != PAIRING_CODE_BYTES {
        return Err(VaultError::InvalidPairingCode);
    }
    Ok(Zeroizing::new(decoded))
}

fn canonical_pairing_passphrase(pairing_code: &PairingCode) -> Result<SecretString, VaultError> {
    let decoded = decode_pairing_code(pairing_code)?;
    Ok(SecretString::from(BASE32_NOPAD_VISUAL.encode(&decoded)))
}

fn parse_recipients(
    recipient_strings: &[String],
) -> Result<Vec<age::x25519::Recipient>, VaultError> {
    if recipient_strings.is_empty() {
        return Err(VaultError::MissingRecipients);
    }
    if recipient_strings.len() > MAX_AGE_RECIPIENTS {
        return Err(VaultError::TooManyRecipients);
    }

    let mut canonical = BTreeSet::new();
    for encoded in recipient_strings {
        let recipient = parse_canonical_age_recipient(encoded)?;
        canonical.insert(recipient.to_string());
    }
    canonical
        .iter()
        .map(|encoded| parse_canonical_age_recipient(encoded))
        .collect()
}

fn canonical_json_bytes<T: Serialize + ?Sized>(
    value: &T,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let mut json = serde_json::to_value(value).map_err(|_| VaultError::SerializationFailed)?;
    canonicalize_json(&mut json);
    let encoded = serde_json::to_vec(&json).map_err(|_| VaultError::SerializationFailed)?;
    if encoded.len() > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    Ok(Zeroizing::new(encoded))
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        serde_json::Value::Object(object) => {
            let mut entries: Vec<_> = std::mem::take(object).into_iter().collect();
            for (_, value) in &mut entries {
                canonicalize_json(value);
            }
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            object.extend(entries);
        }
        _ => {}
    }
}

fn signature_message(domain: &[u8], payload: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut message = Zeroizing::new(Vec::with_capacity(
        SIGNED_OBJECT_PREFIX.len() + domain.len() + payload.len() + 16,
    ));
    message.extend_from_slice(SIGNED_OBJECT_PREFIX);
    append_len_and_bytes(&mut message, domain);
    append_len_and_bytes(&mut message, payload);
    message
}

fn append_len_and_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

fn digest_part(digest: &mut Sha256, part: &[u8]) {
    Digest::update(digest, (part.len() as u64).to_be_bytes());
    Digest::update(digest, part);
}

fn update_framed(mac: &mut HmacSha256, domain: &[u8], payload: &[u8]) {
    Mac::update(mac, &(domain.len() as u64).to_be_bytes());
    Mac::update(mac, domain);
    Mac::update(mac, &(payload.len() as u64).to_be_bytes());
    Mac::update(mac, payload);
}

fn encrypt_bytes_to_recipients(
    plaintext: &[u8],
    recipients: &[age::x25519::Recipient],
) -> Result<EncryptedObject, VaultError> {
    let encryptor = age::Encryptor::with_recipients(
        recipients
            .iter()
            .map(|recipient| recipient as &dyn age::Recipient),
    )
    .map_err(|_| VaultError::EncryptionFailed)?;
    encrypt_with_encryptor(encryptor, plaintext)
}

fn encrypt_with_encryptor(
    encryptor: age::Encryptor,
    plaintext: &[u8],
) -> Result<EncryptedObject, VaultError> {
    if plaintext.len() > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    let mut encrypted = Vec::with_capacity(
        plaintext
            .len()
            .saturating_add(64 * 1024)
            .min(MAX_ENCRYPTED_OBJECT_BYTES),
    );
    {
        let mut writer = encryptor
            .wrap_output(&mut encrypted)
            .map_err(|_| VaultError::EncryptionFailed)?;
        writer
            .write_all(plaintext)
            .map_err(|_| VaultError::EncryptionFailed)?;
        writer.finish().map_err(|_| VaultError::EncryptionFailed)?;
    }
    EncryptedObject::produced_locally(encrypted)
}

fn decrypt_bytes_with_identity(
    object: &EncryptedObject,
    identity: &age::x25519::Identity,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    decrypt_bytes_with_dyn_identity(object, identity)
}

fn decrypt_bytes_with_dyn_identity(
    object: &EncryptedObject,
    identity: &dyn age::Identity,
) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    if object.as_bytes().len() > MAX_ENCRYPTED_OBJECT_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    let decryptor = age::Decryptor::new_buffered(object.as_bytes())
        .map_err(|_| VaultError::DecryptionFailed)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity))
        .map_err(|_| VaultError::DecryptionFailed)?;
    let initial_capacity = object.as_bytes().len().min(INITIAL_READ_CAPACITY);
    let mut plaintext = Zeroizing::new(Vec::with_capacity(initial_capacity));
    (&mut reader)
        .take((MAX_PROTECTED_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut plaintext)
        .map_err(|_| VaultError::DecryptionFailed)?;
    if plaintext.len() > MAX_PROTECTED_PAYLOAD_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    // `read_to_end` reached the stream EOF here, so age has verified the final chunk tag.
    Ok(plaintext)
}

#[cfg(test)]
#[path = "crypto/tests.rs"]
mod tests;
