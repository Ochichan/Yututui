use age::secrecy::ExposeSecret;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use data_encoding::HEXLOWER;
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

use super::*;

const TEST_SIGNATURE_DOMAIN: &[u8] = b"yututui/test-operation/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TestPayload {
    label: String,
    sequence: u64,
}

fn payload() -> TestPayload {
    TestPayload {
        label: "private state".to_owned(),
        sequence: 42,
    }
}

#[test]
fn device_material_round_trips_without_debug_leaks() {
    let material = DeviceSecretMaterial::generate_for("device-a").unwrap();
    let record = material.public_record();
    assert!(material.matches_public_record(&record));
    record.validate().unwrap();

    let age_secret = material.age_identity_secret();
    let signing_secret = material.signing_key_secret_b64();
    let rehydrated =
        DeviceSecretMaterial::from_encoded("device-a", age_secret, &signing_secret).unwrap();
    assert!(rehydrated.matches_public_record(&record));

    let rendered = format!("{material:?}");
    assert!(rendered.contains("[REDACTED]"));
    assert!(!rendered.contains(age_secret_for_assertion(&rehydrated).expose_secret()));
    assert!(!rendered.contains(signing_secret.expose_secret()));
}

#[test]
fn exact_public_key_matching_rejects_aliases_and_edits() {
    let material = DeviceSecretMaterial::generate_for("device-a").unwrap();
    let mut record = material.public_record();
    record.identity.age_recipient = record.identity.age_recipient.to_uppercase();
    assert!(!material.matches_public_record(&record));
    assert_eq!(record.validate(), Err(VaultError::InvalidAgeRecipient));
}

#[test]
fn multi_recipient_age_object_opens_for_each_recipient() {
    let first = DeviceSecretMaterial::generate_for("first").unwrap();
    let second = DeviceSecretMaterial::generate_for("second").unwrap();
    let recipients = vec![
        first.public_identity().age_recipient,
        second.public_identity().age_recipient,
    ];

    let object = encrypt_json_to_recipients(&payload(), &recipients).unwrap();
    assert_eq!(
        decrypt_json_with_identity::<TestPayload>(&object, first.age_identity()).unwrap(),
        payload()
    );
    assert_eq!(
        decrypt_json_with_identity::<TestPayload>(&object, second.age_identity()).unwrap(),
        payload()
    );
}

#[test]
fn wrong_recipient_cannot_open_age_object() {
    let intended = DeviceSecretMaterial::generate_for("intended").unwrap();
    let stranger = DeviceSecretMaterial::generate_for("stranger").unwrap();
    let object =
        encrypt_json_to_recipients(&payload(), &[intended.public_identity().age_recipient])
            .unwrap();

    assert_eq!(
        decrypt_json_with_identity::<TestPayload>(&object, stranger.age_identity()),
        Err(VaultError::DecryptionFailed)
    );
}

#[test]
fn age_header_and_final_tag_tampering_are_rejected() {
    let material = DeviceSecretMaterial::generate_for("device").unwrap();
    let object =
        encrypt_json_to_recipients(&payload(), &[material.public_identity().age_recipient])
            .unwrap();

    let mut header_tamper = object.clone().into_bytes();
    let stanza = header_tamper
        .windows(b"X25519".len())
        .position(|window| window == b"X25519")
        .unwrap();
    header_tamper[stanza] ^= 1;
    let header_tamper = EncryptedObject::from_bytes(header_tamper).unwrap();
    assert_eq!(
        decrypt_json_with_identity::<TestPayload>(&header_tamper, material.age_identity()),
        Err(VaultError::DecryptionFailed)
    );

    let mut tag_tamper = object.into_bytes();
    *tag_tamper.last_mut().unwrap() ^= 1;
    let tag_tamper = EncryptedObject::from_bytes(tag_tamper).unwrap();
    assert_eq!(
        decrypt_json_with_identity::<TestPayload>(&tag_tamper, material.age_identity()),
        Err(VaultError::DecryptionFailed)
    );
}

#[test]
fn c2sp_cctv_x25519_fixture_decrypts_and_matches_hash() {
    // Pinned 0BSD C2SP CCTV vector. Provenance is recorded in the fixture itself.
    let encoded: String = include_str!("fixtures/cctv_x25519.b64")
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect();
    let fixture = STANDARD.decode(encoded).unwrap();
    let separator = fixture
        .windows(2)
        .position(|window| window == b"\n\n")
        .unwrap();
    let header = std::str::from_utf8(&fixture[..separator]).unwrap();
    let encrypted = EncryptedObject::from_bytes(fixture[separator + 2..].to_vec()).unwrap();
    let identity: age::x25519::Identity = header
        .lines()
        .find_map(|line| line.strip_prefix("identity: "))
        .unwrap()
        .parse()
        .unwrap();
    let plaintext = decrypt_bytes_with_identity(&encrypted, &identity).unwrap();

    assert_eq!(
        sha256_domainless_hex(&plaintext),
        "013f54400c82da08037759ada907a8b864e97de81c088a182062c4b5622fd2ab"
    );
}

#[test]
fn signatures_are_domain_separated_and_strictly_verified() {
    let material = DeviceSecretMaterial::generate_for("signer").unwrap();
    let public_key = material.public_identity().ed25519_verifying_key;
    let signature =
        sign_serializable(TEST_SIGNATURE_DOMAIN, material.signing_key(), &payload()).unwrap();
    verify_serializable(TEST_SIGNATURE_DOMAIN, &public_key, &payload(), &signature).unwrap();

    let changed = TestPayload {
        sequence: 43,
        ..payload()
    };
    assert_eq!(
        verify_serializable(TEST_SIGNATURE_DOMAIN, &public_key, &changed, &signature),
        Err(VaultError::SignatureVerificationFailed)
    );
    assert_eq!(
        verify_serializable(
            b"yututui/other-domain/v1",
            &public_key,
            &payload(),
            &signature
        ),
        Err(VaultError::SignatureVerificationFailed)
    );

    let mut corrupted = base64url_decode(&signature).unwrap();
    corrupted[0] ^= 1;
    let corrupted = base64url_encode(&corrupted);
    assert_eq!(
        verify_serializable(TEST_SIGNATURE_DOMAIN, &public_key, &payload(), &corrupted),
        Err(VaultError::SignatureVerificationFailed)
    );
}

#[test]
fn canonical_json_signatures_ignore_object_insertion_order() {
    let material = DeviceSecretMaterial::generate_for("signer").unwrap();
    let mut first = serde_json::Map::new();
    first.insert("z".to_owned(), serde_json::json!(1));
    first.insert("a".to_owned(), serde_json::json!({"y": 2, "b": 3}));
    let mut second = serde_json::Map::new();
    second.insert("a".to_owned(), serde_json::json!({"b": 3, "y": 2}));
    second.insert("z".to_owned(), serde_json::json!(1));

    let first = sign_serializable(TEST_SIGNATURE_DOMAIN, material.signing_key(), &first).unwrap();
    let second = sign_serializable(TEST_SIGNATURE_DOMAIN, material.signing_key(), &second).unwrap();
    assert_eq!(first, second);
}

#[test]
fn rfc8032_ed25519_test_vector_one_matches() {
    let seed = decode_hex::<32>("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
    let expected_public =
        decode_hex::<32>("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    let expected_signature = decode_hex::<64>(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    );
    let key = SigningKey::from_bytes(&seed);
    assert_eq!(key.verifying_key().to_bytes(), expected_public);
    let signature = key.sign(b"");
    assert_eq!(signature.to_bytes(), expected_signature);
    key.verifying_key().verify_strict(b"", &signature).unwrap();
}

#[test]
fn pairing_code_proof_and_scrypt_seal_round_trip() {
    let code = generate_pairing_code().unwrap();
    assert_eq!(
        code.expose_secret()
            .bytes()
            .filter(|byte| *byte != b'-')
            .count(),
        26
    );
    let reparsed = PairingCode::parse(&code.expose_secret().to_ascii_lowercase()).unwrap();
    assert_eq!(reparsed.expose_secret(), code.expose_secret());
    assert!(!format!("{code:?}").contains(code.expose_secret()));

    let proof = pairing_proof_b64(&code, b"joiner public keys").unwrap();
    verify_pairing_proof(&code, b"joiner public keys", &proof).unwrap();
    assert_eq!(
        verify_pairing_proof(&code, b"different keys", &proof),
        Err(VaultError::PairingProofFailed)
    );

    let object = seal_pairing_json(&code, &payload()).unwrap();
    assert_eq!(
        open_pairing_json::<TestPayload>(&code, &object).unwrap(),
        payload()
    );
    let wrong_code = generate_pairing_code().unwrap();
    assert_eq!(
        open_pairing_json::<TestPayload>(&wrong_code, &object),
        Err(VaultError::DecryptionFailed)
    );
}

#[test]
fn recovery_signer_is_stable_and_scoped_to_dataset() {
    let recovery = age::x25519::Identity::generate();
    let first = derive_recovery_signing_key(&recovery, "dataset-a").unwrap();
    let repeated = derive_recovery_signing_key(&recovery, "dataset-a").unwrap();
    let other = derive_recovery_signing_key(&recovery, "dataset-b").unwrap();

    assert_eq!(
        first.verifying_key().to_bytes(),
        repeated.verifying_key().to_bytes()
    );
    assert_ne!(
        first.verifying_key().to_bytes(),
        other.verifying_key().to_bytes()
    );
}

#[test]
fn encoding_and_random_helpers_are_canonical() {
    let random = random_id_hex::<16>().unwrap();
    assert_eq!(random.len(), 32);
    assert!(random.bytes().all(|byte| byte.is_ascii_hexdigit()));

    let encoded = base64url_encode(b"canonical");
    assert_eq!(&*base64url_decode(&encoded).unwrap(), b"canonical");
    assert_eq!(
        base64url_decode(&(encoded + "=")),
        Err(VaultError::InvalidBase64)
    );

    let original = *b"canonical123";
    let encoded = base32_visual_encode(&original);
    let grouped = format!(
        "{}-{}-{}-{}",
        &encoded[0..5],
        &encoded[5..10],
        &encoded[10..15],
        &encoded[15..]
    )
    .to_ascii_lowercase();
    assert_eq!(&*base32_visual_decode(&grouped).unwrap(), &original);
}

fn age_secret_for_assertion(material: &DeviceSecretMaterial) -> SecretString {
    material.age_identity_secret()
}

fn sha256_domainless_hex(bytes: &[u8]) -> String {
    HEXLOWER.encode(&Sha256::digest(bytes))
}

fn decode_hex<const N: usize>(encoded: &str) -> [u8; N] {
    HEXLOWER
        .decode(encoded.replace(char::is_whitespace, "").as_bytes())
        .unwrap()
        .try_into()
        .unwrap()
}
