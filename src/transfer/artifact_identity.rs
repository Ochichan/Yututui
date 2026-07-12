use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::util::safe_fs;

pub(crate) const ARTIFACT_AUDIO_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const ARTIFACT_SIDECAR_MAX_BYTES: u64 = 64 * 1024;

/// Durable admission token for one exact import-row selection.
///
/// The session instance and row revision make a reused job/row id distinguishable from its
/// predecessor, while `claim_id` distinguishes retries of the same selected video.  Every
/// asynchronous terminal path must present this complete token before it may mutate the row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ImportDownloadClaim {
    pub(crate) session_id: String,
    pub(crate) session_instance_id: String,
    pub(crate) row_id: String,
    pub(crate) source_order: u32,
    pub(crate) row_revision: u64,
    pub(crate) claim_id: String,
    pub(crate) expected_key: String,
}

#[cfg(test)]
pub(crate) fn test_import_claim(
    session_id: &str,
    row_id: &str,
    source_order: u32,
    expected_key: &str,
) -> ImportDownloadClaim {
    ImportDownloadClaim {
        session_id: session_id.to_owned(),
        session_instance_id: format!("test-instance-{session_id}"),
        row_id: row_id.to_owned(),
        source_order,
        row_revision: 1,
        claim_id: format!("test-claim-{source_order}-{expected_key}"),
        expected_key: expected_key.to_owned(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactFileIdentity {
    pub(crate) len: u64,
    pub(crate) sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ArtifactReceipt {
    pub(crate) audio: ArtifactFileIdentity,
    #[serde(default)]
    pub(crate) sidecar_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sidecar: Option<ArtifactFileIdentity>,
    /// The exact download admission which produced this artifact. Organize operations and
    /// receipts promoted from legacy records do not have an admission claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) claim: Option<ImportDownloadClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReceiptVerification {
    Verified,
    NeedsDownload,
    Conflict(String),
    LegacyUnverified,
}

pub(crate) fn verify_receipt(
    audio: &Path,
    sidecar: &Path,
    receipt: Option<&ArtifactReceipt>,
) -> ReceiptVerification {
    let Some(receipt) = receipt else {
        return ReceiptVerification::LegacyUnverified;
    };
    let actual_audio = match optional_file_identity(audio, ARTIFACT_AUDIO_MAX_BYTES) {
        Ok(Some(identity)) => identity,
        Ok(None) => return ReceiptVerification::NeedsDownload,
        Err(error) => return ReceiptVerification::Conflict(error.to_string()),
    };
    if actual_audio != receipt.audio {
        return ReceiptVerification::Conflict(format!(
            "audio receipt mismatch at {}",
            audio.display()
        ));
    }
    if receipt.sidecar_required && receipt.sidecar.is_none() {
        return ReceiptVerification::Conflict(format!(
            "required sidecar receipt is missing for {}",
            audio.display()
        ));
    }
    if let Some(expected_sidecar) = receipt.sidecar.as_ref() {
        let actual_sidecar = match optional_file_identity(sidecar, ARTIFACT_SIDECAR_MAX_BYTES) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                return ReceiptVerification::Conflict(format!(
                    "recorded sidecar is missing at {}",
                    sidecar.display()
                ));
            }
            Err(error) => return ReceiptVerification::Conflict(error.to_string()),
        };
        if actual_sidecar != *expected_sidecar {
            return ReceiptVerification::Conflict(format!(
                "sidecar receipt mismatch at {}",
                sidecar.display()
            ));
        }
    }
    ReceiptVerification::Verified
}

pub(crate) fn file_identity_limited(
    path: &Path,
    max_bytes: u64,
) -> io::Result<ArtifactFileIdentity> {
    let mut file = safe_fs::open_regular_no_symlink(path)?;
    file_identity_from_open(&mut file, path, max_bytes)
}

pub(crate) fn optional_file_identity(
    path: &Path,
    max_bytes: u64,
) -> io::Result<Option<ArtifactFileIdentity>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlink artifact {}", path.display()),
        )),
        Ok(_) => file_identity_limited(path, max_bytes).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(crate) fn file_identity_from_open(
    file: &mut File,
    path: &Path,
    max_bytes: u64,
) -> io::Result<ArtifactFileIdentity> {
    let initial = file.metadata()?;
    if initial.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "artifact exceeds the {max_bytes} byte identity limit: {}",
                path.display()
            ),
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *file);
    let identity = hash_exact_len(&mut reader, initial.len())?;
    drop(reader);
    let final_metadata = file.metadata()?;
    let modified_changed = match (initial.modified(), final_metadata.modified()) {
        (Ok(initial), Ok(final_metadata)) => initial != final_metadata,
        _ => false,
    };
    if final_metadata.len() != initial.len() || modified_changed {
        return Err(io::Error::other(format!(
            "artifact changed while it was identified: {}",
            path.display()
        )));
    }
    Ok(identity)
}

pub(crate) fn hash_exact_len<R: Read>(
    reader: &mut R,
    expected_len: u64,
) -> io::Result<ArtifactFileIdentity> {
    let limit = expected_len
        .checked_add(1)
        .ok_or_else(|| io::Error::other("artifact size cannot be bounded"))?;
    let mut bounded = reader.take(limit);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut len = 0_u64;
    loop {
        let read = bounded.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        len = len
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("artifact size overflow"))?;
        hasher.update(&buffer[..read]);
    }
    if len != expected_len {
        return Err(io::Error::other(format!(
            "artifact changed while it was identified (expected {expected_len} bytes, read {len})"
        )));
    }
    Ok(ArtifactFileIdentity {
        len,
        sha256: hex_digest(&hasher.finalize()),
    })
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
