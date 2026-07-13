//! Crash-recoverable ownership for moving one import audio file and its metadata sidecar.

use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::artifact_identity::{
    ARTIFACT_AUDIO_MAX_BYTES, ARTIFACT_SIDECAR_MAX_BYTES, ArtifactFileIdentity as ArtifactIdentity,
    ArtifactReceipt, ImportDownloadClaim, ReceiptVerification, file_identity_from_open,
    file_identity_limited, hash_exact_len, optional_file_identity, verify_receipt,
};
use super::session::{ImportRecordGuard, ImportSession};
use crate::util::safe_fs;

mod recovery;
use recovery::committed_session_path;
mod permissions;
use permissions::{artifact_publish_modes, ensure_scoped_directory, validate_publish_mode};
mod reconcile;
use reconcile::{
    ReconcileFilePair, ReconcileFilePolicy, ReconcileOptionalFilePair, pin_transaction_scopes,
    reconcile_file_pair, reconcile_optional_file_pair,
};

const TXN_SCHEMA_VERSION: u32 = 3;
const TXN_MAX_BYTES: u64 = 64 * 1024;
const TXN_MAX_FILES: usize = 1024;
const TXN_SCAN_MAX_BYTES: u64 = 16 * 1024 * 1024;
const ID_COMPONENT_MAX: usize = 128;
const OPERATION_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTRY_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactMoveKind {
    ImportDownload,
    Organize,
}

#[derive(Debug, Clone)]
pub(crate) struct ArtifactMoveRequest {
    kind: ArtifactMoveKind,
    session_id: String,
    row_id: String,
    source_order: u32,
    claim: Option<ImportDownloadClaim>,
    audio_from: PathBuf,
    audio_to: PathBuf,
    destination_root: PathBuf,
}

impl ArtifactMoveRequest {
    pub(crate) fn import_download(
        session_id: String,
        row_id: String,
        source_order: u32,
        claim: ImportDownloadClaim,
        audio_from: PathBuf,
        audio_to: PathBuf,
        destination_root: PathBuf,
    ) -> Self {
        Self {
            kind: ArtifactMoveKind::ImportDownload,
            session_id,
            row_id,
            source_order,
            claim: Some(claim),
            audio_from,
            audio_to,
            destination_root,
        }
    }

    pub(crate) fn organize(
        session_id: String,
        row_id: String,
        source_order: u32,
        audio_from: PathBuf,
        audio_to: PathBuf,
        destination_root: PathBuf,
    ) -> Self {
        Self {
            kind: ArtifactMoveKind::Organize,
            session_id,
            row_id,
            source_order,
            claim: None,
            audio_from,
            audio_to,
            destination_root,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactMovePhase {
    Prepared,
    AudioMoved,
    SidecarMoved,
    SessionSaved,
    SourcesReclaimed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactMoveTxn {
    schema_version: u32,
    kind: ArtifactMoveKind,
    session_id: String,
    row_id: String,
    source_order: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim: Option<ImportDownloadClaim>,
    session_source_path: PathBuf,
    session_destination_path: PathBuf,
    audio_from: PathBuf,
    audio_to: PathBuf,
    sidecar_from: PathBuf,
    sidecar_to: PathBuf,
    source_parent: PathBuf,
    source_parent_id: safe_fs::FileObjectId,
    source_private_root: Option<PathBuf>,
    source_private_root_id: Option<safe_fs::FileObjectId>,
    destination_root: PathBuf,
    destination_root_id: safe_fs::FileObjectId,
    destination_private_root: Option<PathBuf>,
    destination_private_root_id: Option<safe_fs::FileObjectId>,
    destination_parent: PathBuf,
    destination_parent_id: safe_fs::FileObjectId,
    audio_stage_name: String,
    sidecar_stage_name: String,
    audio_stage_object_id: Option<safe_fs::FileObjectId>,
    sidecar_stage_object_id: Option<safe_fs::FileObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio_publish_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidecar_publish_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "is_false")]
    audio_source_private_stage: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    sidecar_source_private_stage: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    verify_legacy_existing_modes: bool,
    audio_object_id: safe_fs::FileObjectId,
    audio_identity: ArtifactIdentity,
    sidecar_object_id: Option<safe_fs::FileObjectId>,
    sidecar_identity: Option<ArtifactIdentity>,
    phase: ArtifactMovePhase,
}

#[derive(Debug, Default)]
pub(crate) struct ArtifactMoveRecoveryReport {
    pub(crate) recovered: usize,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactMoveFaultPoint {
    BeforeAudioPublish,
    AfterAudioPublishBeforeSourceUnlink,
    BeforeAudioSourceUnlinkValidation,
    BeforeSidecarPublish,
    AfterSidecarPublishBeforeSourceUnlink,
    BeforeSidecarSourceUnlinkValidation,
    AfterAudioMove,
    AfterSidecarMove,
    BeforeSessionSave,
    AfterSessionSave,
}

struct ArtifactTxnLock {
    _lock: safe_fs::AdvisoryFileLock,
}

pub(crate) fn commit(request: ArtifactMoveRequest) -> anyhow::Result<PathBuf> {
    validate_identity_components(&request.session_id, &request.row_id, request.source_order)?;
    let _record_guard = acquire_record_guard(&request.session_id)?;
    commit_locked(request)
}

/// Commit while the caller owns the session's [`ImportRecordGuard`].
pub(crate) fn commit_locked(request: ArtifactMoveRequest) -> anyhow::Result<PathBuf> {
    commit_locked_with_hook(request, None)
}

#[cfg(test)]
pub(crate) fn commit_with_fault(
    request: ArtifactMoveRequest,
    fault: ArtifactMoveFaultPoint,
) -> anyhow::Result<PathBuf> {
    validate_identity_components(&request.session_id, &request.row_id, request.source_order)?;
    let _record_guard = acquire_record_guard(&request.session_id)?;
    let hook = |point| {
        if point == fault {
            Err(std::io::Error::other(format!(
                "fault injection after {point:?}"
            )))
        } else {
            Ok(())
        }
    };
    commit_locked_with_hook(request, Some(&hook))
}

fn commit_locked_with_hook(
    request: ArtifactMoveRequest,
    fault: Option<&dyn Fn(ArtifactMoveFaultPoint) -> std::io::Result<()>>,
) -> anyhow::Result<PathBuf> {
    validate_request(&request)?;
    let key = transaction_key(&request.session_id, &request.row_id, request.source_order);
    let _txn_lock = acquire_txn_lock(&key)?;
    let path = transaction_path(&key)?;

    if path.exists() {
        let mut txn = load_transaction(&path)?;
        validate_transaction_key(&txn, &path, &key)?;
        return resume_transaction(&path, &mut txn, fault);
    }

    if let Some(done) = already_committed_path(&request)? {
        sync_absent_transaction_parent(&path)?;
        return Ok(done);
    }

    let mut txn = prepare_transaction(request)?;
    persist_new_transaction(&path, &txn)?;
    resume_transaction(&path, &mut txn, fault)
}

/// Reconcile one row before retrying a download. A durable completed row is also success even
/// after its transaction was already removed and its UI notification was lost during shutdown.
#[cfg(test)]
pub(crate) fn reconcile_row(
    session_id: &str,
    row_id: &str,
    source_order: u32,
) -> anyhow::Result<Option<PathBuf>> {
    validate_identity_components(session_id, row_id, source_order)?;
    let _record_guard = acquire_record_guard(session_id)?;
    let key = transaction_key(session_id, row_id, source_order);
    let _txn_lock = acquire_txn_lock(&key)?;
    let path = transaction_path(&key)?;
    if path.exists() {
        let mut txn = load_transaction(&path)?;
        validate_transaction_key(&txn, &path, &key)?;
        return resume_transaction(&path, &mut txn, None).map(Some);
    }
    let committed = committed_session_path(session_id, row_id, source_order)?;
    if committed.is_some() {
        sync_absent_transaction_parent(&path)?;
    }
    Ok(committed)
}

/// Reconcile a retry for the exact durable admission which originally started it.
pub(crate) fn reconcile_claim(claim: &ImportDownloadClaim) -> anyhow::Result<Option<PathBuf>> {
    validate_identity_components(&claim.session_id, &claim.row_id, claim.source_order)?;
    let _record_guard = acquire_record_guard(&claim.session_id)?;
    super::session::validate_import_download_claim_unlocked(claim, true)?;
    let key = transaction_key(&claim.session_id, &claim.row_id, claim.source_order);
    let _txn_lock = acquire_txn_lock(&key)?;
    let path = transaction_path(&key)?;
    if path.exists() {
        let mut txn = load_transaction(&path)?;
        validate_transaction_key(&txn, &path, &key)?;
        if txn.claim.as_ref() != Some(claim) {
            bail!("artifact move transaction belongs to a different download claim");
        }
        return resume_transaction(&path, &mut txn, None).map(Some);
    }
    let committed = committed_session_path(&claim.session_id, &claim.row_id, claim.source_order)?;
    if committed.is_some() {
        sync_absent_transaction_parent(&path)?;
    }
    Ok(committed)
}

pub(crate) fn has_pending_claim(claim: &ImportDownloadClaim) -> anyhow::Result<bool> {
    validate_identity_components(&claim.session_id, &claim.row_id, claim.source_order)?;
    let key = transaction_key(&claim.session_id, &claim.row_id, claim.source_order);
    let _txn_lock = acquire_txn_lock(&key)?;
    let path = transaction_path(&key)?;
    if !path.exists() {
        return Ok(false);
    }
    let txn = load_transaction(&path)?;
    validate_transaction_key(&txn, &path, &key)?;
    Ok(txn.claim.as_ref() == Some(claim))
}

/// Reconcile every transaction for a session while its import-record lock is already held.
pub(crate) fn reconcile_session_locked(session_id: &str) -> anyhow::Result<usize> {
    validate_component("session id", session_id)?;
    let paths = inventory_transactions()?;
    reconcile_session_paths_locked(session_id, paths)
}

fn reconcile_session_paths_locked(session_id: &str, paths: Vec<PathBuf>) -> anyhow::Result<usize> {
    let mut recovered = 0;
    for path in paths {
        // Inventory is only a snapshot. A commit for another session may remove its transaction
        // before we inspect it, so serialize with that transaction first and then re-check the
        // path. Treat absence under the owning lock as successful completion, not corruption of
        // the session currently being reconciled.
        let key = transaction_key_from_path(&path)?;
        let _txn_lock = acquire_txn_lock(key)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        let mut txn = load_transaction(&path)?;
        if txn.session_id != session_id {
            continue;
        }
        validate_transaction_key(&txn, &path, key)?;
        resume_transaction(&path, &mut txn, None)?;
        recovered += 1;
    }
    Ok(recovered)
}

/// Startup recovery. Inventory limits are checked before any transaction is mutated.
pub(crate) fn reconcile_all_pending() -> anyhow::Result<ArtifactMoveRecoveryReport> {
    let paths = inventory_transactions()?;
    let mut report = ArtifactMoveRecoveryReport::default();
    for path in paths {
        let mut txn = match load_transaction(&path) {
            Ok(txn) => txn,
            Err(error) => {
                report.warnings.push(format!(
                    "could not load artifact move {}: {error:#}",
                    path.display()
                ));
                continue;
            }
        };
        let key = transaction_key(&txn.session_id, &txn.row_id, txn.source_order);
        if let Err(error) = validate_transaction_key(&txn, &path, &key) {
            report.warnings.push(error.to_string());
            continue;
        }
        let _record_guard = match ImportRecordGuard::try_acquire(&txn.session_id) {
            Ok(guard) => guard,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                report.warnings.push(format!(
                    "artifact move {} remains pending while its import session is active",
                    path.display()
                ));
                continue;
            }
            Err(error) => {
                report.warnings.push(format!(
                    "could not lock artifact move {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let _txn_lock = match acquire_txn_lock(&key) {
            Ok(lock) => lock,
            Err(error) => {
                report.warnings.push(format!(
                    "could not own artifact move {}: {error:#}",
                    path.display()
                ));
                continue;
            }
        };
        match resume_transaction(&path, &mut txn, None) {
            Ok(_) => report.recovered += 1,
            Err(error) => report.warnings.push(format!(
                "artifact move {} remains pending: {error:#}",
                path.display()
            )),
        }
    }
    Ok(report)
}

fn prepare_transaction(request: ArtifactMoveRequest) -> anyhow::Result<ArtifactMoveTxn> {
    let operation_key = transaction_key(&request.session_id, &request.row_id, request.source_order);
    let stage_tag = operation_key
        .get(..16)
        .context("artifact operation key is too short")?;
    reject_parent_components(&request.audio_from)?;
    reject_parent_components(&request.audio_to)?;
    reject_parent_components(&request.destination_root)?;
    let relative_destination = request
        .audio_to
        .strip_prefix(&request.destination_root)
        .with_context(|| {
            format!(
                "artifact destination escaped root: {}",
                request.audio_to.display()
            )
        })?;
    let file_name = relative_destination
        .file_name()
        .context("artifact destination has no file name")?;
    let relative_parent = relative_destination
        .parent()
        .unwrap_or_else(|| Path::new(""));
    safe_fs::ensure_dir_durable(&request.destination_root)?;
    reject_symlink_or_non_directory(&request.destination_root)?;
    let destination_root = fs::canonicalize(&request.destination_root).with_context(|| {
        format!(
            "canonicalize artifact destination root {}",
            request.destination_root.display()
        )
    })?;
    let publish_modes = artifact_publish_modes(request.kind, &destination_root)?;
    let requested_parent =
        ensure_scoped_directory(&destination_root, relative_parent, publish_modes.directory)?;
    let destination_parent = destination_root.join(relative_parent);
    let (
        destination_root_pin,
        destination_parent_pin,
        destination_private_root,
        destination_private_root_id,
    ) = match import_private_root(&destination_root) {
        Some(private_root) => {
            let root_relative = destination_root
                .strip_prefix(&private_root)
                .context("private destination root escaped its import root")?;
            let parent_relative = destination_parent
                .strip_prefix(&private_root)
                .context("private destination parent escaped its import root")?;
            let private_root_pin =
                safe_fs::PinnedDir::open_private_existing(&private_root, Path::new(""))
                    .with_context(|| {
                        format!("pin private destination root {}", private_root.display())
                    })?;
            let destination_root_pin =
                safe_fs::PinnedDir::open_private_existing(&private_root, root_relative)
                    .with_context(|| {
                        format!(
                            "pin private destination scope {}",
                            destination_root.display()
                        )
                    })?;
            let destination_parent_pin =
                safe_fs::PinnedDir::open_private_existing(&private_root, parent_relative)
                    .with_context(|| {
                        format!(
                            "pin private destination parent {}",
                            requested_parent.display()
                        )
                    })?;
            let private_root_id = private_root_pin.identity();
            (
                destination_root_pin,
                destination_parent_pin,
                Some(private_root),
                Some(private_root_id),
            )
        }
        None => (
            safe_fs::PinnedDir::open_existing(&destination_root, Path::new(""))
                .with_context(|| format!("pin destination root {}", destination_root.display()))?,
            safe_fs::PinnedDir::open_existing(&destination_root, relative_parent).with_context(
                || format!("pin destination parent {}", requested_parent.display()),
            )?,
            None,
            None,
        ),
    };
    let audio_to = destination_parent.join(file_name);

    let session_source_path = request.audio_from.clone();
    let session_destination_path = request.audio_to.clone();
    let requested_source_parent = request
        .audio_from
        .parent()
        .context("source audio has no parent")?
        .to_path_buf();
    let source_parent = fs::canonicalize(&requested_source_parent).with_context(|| {
        format!(
            "canonicalize source audio parent {}",
            requested_source_parent.display()
        )
    })?;
    let (source_parent_pin, source_private_root, source_private_root_id) =
        match import_private_root(&source_parent) {
            Some(private_root) => {
                let relative = source_parent
                    .strip_prefix(&private_root)
                    .context("private source parent escaped its import root")?;
                let private_root_pin =
                    safe_fs::PinnedDir::open_private_existing(&private_root, Path::new(""))
                        .with_context(|| {
                            format!("pin private import root {}", private_root.display())
                        })?;
                let source_parent_pin =
                    safe_fs::PinnedDir::open_private_existing(&private_root, relative)
                        .with_context(|| {
                            format!("pin private source parent {}", source_parent.display())
                        })?;
                let private_root_id = private_root_pin.identity();
                (source_parent_pin, Some(private_root), Some(private_root_id))
            }
            None => (
                safe_fs::PinnedDir::open_existing(&source_parent, Path::new("")).with_context(
                    || format!("pin source audio parent {}", source_parent.display()),
                )?,
                None,
                None,
            ),
        };
    let source_name = request
        .audio_from
        .file_name()
        .context("source audio has no file name")?;
    let audio_from = source_parent.join(source_name);
    let source_audio = source_parent_pin
        .open_child_readonly(source_name)
        .with_context(|| format!("open source audio {}", audio_from.display()))?;
    let mut source_audio_file = source_audio.file()?.try_clone()?;
    let audio_identity = file_identity_from_open(
        &mut source_audio_file,
        &audio_from,
        ARTIFACT_AUDIO_MAX_BYTES,
    )
    .with_context(|| format!("identify source audio {}", audio_from.display()))?;
    if audio_from == audio_to {
        bail!("artifact source and destination are identical");
    }

    let sidecar_from = crate::downloads::sidecar_path(&audio_from);
    let sidecar_to = crate::downloads::sidecar_path(&audio_to);
    let sidecar_name = sidecar_from
        .file_name()
        .context("source sidecar has no file name")?;
    let source_sidecar = match source_parent_pin.open_child_readonly(sidecar_name) {
        Ok(sidecar) => Some(sidecar),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let sidecar_identity = source_sidecar
        .as_ref()
        .map(|sidecar| {
            let mut file = sidecar.file()?.try_clone()?;
            file_identity_from_open(&mut file, &sidecar_from, ARTIFACT_SIDECAR_MAX_BYTES)
        })
        .transpose()?;
    if matches!(request.kind, ArtifactMoveKind::ImportDownload) && sidecar_identity.is_none() {
        bail!(
            "import download sidecar is required before artifact commit: {}",
            sidecar_from.display()
        );
    }
    // Linux can publish an anonymous O_TMPFILE created on the destination filesystem, so keep
    // staging there. Besides avoiding a recoverable public stage, this also lets an inbox and
    // library live on different mounts. Other Unix platforms need the app-owned source namespace
    // for their named no-replace staging fallback.
    let source_private_stage = cfg!(all(unix, not(target_os = "linux")))
        && matches!(request.kind, ArtifactMoveKind::Organize)
        && source_private_root.is_some()
        && destination_private_root.is_none();
    Ok(ArtifactMoveTxn {
        schema_version: TXN_SCHEMA_VERSION,
        kind: request.kind,
        session_id: request.session_id,
        row_id: request.row_id,
        source_order: request.source_order,
        claim: request.claim,
        session_source_path,
        session_destination_path,
        audio_from,
        audio_to,
        sidecar_from,
        sidecar_to,
        source_parent,
        source_parent_id: source_parent_pin.identity(),
        source_private_root,
        source_private_root_id,
        destination_root,
        destination_root_id: destination_root_pin.identity(),
        destination_private_root,
        destination_private_root_id,
        destination_parent,
        destination_parent_id: destination_parent_pin.identity(),
        audio_stage_name: format!(".ytt-stage-{stage_tag}-audio"),
        sidecar_stage_name: format!(".ytt-stage-{stage_tag}-sidecar"),
        audio_stage_object_id: None,
        sidecar_stage_object_id: None,
        audio_publish_mode: publish_modes.audio,
        sidecar_publish_mode: publish_modes.sidecar,
        audio_source_private_stage: source_private_stage,
        sidecar_source_private_stage: source_private_stage,
        verify_legacy_existing_modes: false,
        audio_object_id: source_audio.identity(),
        audio_identity,
        sidecar_object_id: source_sidecar.as_ref().map(|sidecar| sidecar.identity()),
        sidecar_identity,
        phase: ArtifactMovePhase::Prepared,
    })
}

fn resume_transaction(
    path: &Path,
    txn: &mut ArtifactMoveTxn,
    fault: Option<&dyn Fn(ArtifactMoveFaultPoint) -> std::io::Result<()>>,
) -> anyhow::Result<PathBuf> {
    validate_transaction(txn)?;
    backfill_publication_policy(path, txn)?;
    if let Some(claim) = txn.claim.as_ref() {
        super::session::validate_import_download_claim_unlocked(claim, true)?;
    }
    {
        let scopes = pin_transaction_scopes(txn)?;
        let source_name = txn
            .audio_from
            .file_name()
            .context("source audio has no file name")?
            .to_owned();
        let destination_name = txn
            .audio_to
            .file_name()
            .context("destination audio has no file name")?
            .to_owned();
        let stage_name = txn.audio_stage_name.clone();
        let expected = txn.audio_identity.clone();
        let expected_source = txn.audio_object_id;
        let expected_stage = txn.audio_stage_object_id;
        let publish_mode = txn.audio_publish_mode;
        let verify_legacy_existing_mode = txn.verify_legacy_existing_modes;
        let stage_parent = if txn.audio_source_private_stage {
            &scopes.source_parent
        } else {
            &scopes.destination_parent
        };
        let mut record_stage = |identity| {
            txn.audio_stage_object_id = Some(identity);
            persist_transaction(path, txn)
        };
        reconcile_file_pair(
            ReconcileFilePair {
                source_parent: &scopes.source_parent,
                source_name: &source_name,
                expected_source_object: expected_source,
                stage_parent,
                destination_parent: &scopes.destination_parent,
                destination_stage_name: std::ffi::OsStr::new(&stage_name),
                expected_stage_object: expected_stage,
                destination_name: &destination_name,
                expected: &expected,
            },
            &mut record_stage,
            ReconcileFilePolicy {
                fault,
                before_publish: ArtifactMoveFaultPoint::BeforeAudioPublish,
                after_publish: ArtifactMoveFaultPoint::AfterAudioPublishBeforeSourceUnlink,
                retained_source_boundary: ArtifactMoveFaultPoint::BeforeAudioSourceUnlinkValidation,
                max_bytes: ARTIFACT_AUDIO_MAX_BYTES,
                publish_mode,
                verify_legacy_existing_mode,
            },
        )
        .context("reconcile import audio move")?;
    }
    inject(fault, ArtifactMoveFaultPoint::AfterAudioMove)?;
    advance_phase(path, txn, ArtifactMovePhase::AudioMoved)?;

    {
        let scopes = pin_transaction_scopes(txn)?;
        let source_name = txn
            .sidecar_from
            .file_name()
            .context("source sidecar has no file name")?
            .to_owned();
        let destination_name = txn
            .sidecar_to
            .file_name()
            .context("destination sidecar has no file name")?
            .to_owned();
        let stage_name = txn.sidecar_stage_name.clone();
        let expected = txn.sidecar_identity.clone();
        let expected_source = txn.sidecar_object_id;
        let expected_stage = txn.sidecar_stage_object_id;
        let publish_mode = txn.sidecar_publish_mode;
        let verify_legacy_existing_mode = txn.verify_legacy_existing_modes;
        let stage_parent = if txn.sidecar_source_private_stage {
            &scopes.source_parent
        } else {
            &scopes.destination_parent
        };
        let mut record_stage = |identity| {
            txn.sidecar_stage_object_id = Some(identity);
            persist_transaction(path, txn)
        };
        reconcile_optional_file_pair(
            ReconcileOptionalFilePair {
                source_parent: &scopes.source_parent,
                source_name: &source_name,
                expected_source_object: expected_source,
                stage_parent,
                destination_parent: &scopes.destination_parent,
                destination_stage_name: std::ffi::OsStr::new(&stage_name),
                expected_stage_object: expected_stage,
                destination_name: &destination_name,
                expected: expected.as_ref(),
            },
            &mut record_stage,
            fault,
            publish_mode,
            verify_legacy_existing_mode,
        )
        .context("reconcile import sidecar move")?;
    }
    inject(fault, ArtifactMoveFaultPoint::AfterSidecarMove)?;
    advance_phase(path, txn, ArtifactMovePhase::SidecarMoved)?;

    pin_transaction_scopes(txn)?;
    inject(fault, ArtifactMoveFaultPoint::BeforeSessionSave)?;
    super::session::record_artifact_move_done_unlocked(
        &txn.session_id,
        &txn.row_id,
        txn.source_order,
        (&txn.session_source_path, &txn.session_destination_path),
        matches!(txn.kind, ArtifactMoveKind::ImportDownload),
        txn.claim.as_ref(),
        ArtifactReceipt {
            audio: txn.audio_identity.clone(),
            sidecar_required: matches!(txn.kind, ArtifactMoveKind::ImportDownload),
            sidecar: txn.sidecar_identity.clone(),
            claim: txn.claim.clone(),
        },
    )?;
    inject(fault, ArtifactMoveFaultPoint::AfterSessionSave)?;
    advance_phase(path, txn, ArtifactMovePhase::SessionSaved)?;

    let scopes = pin_transaction_scopes(txn)?;
    reclaim_private_sources(txn, &scopes.source_parent)?;
    advance_phase(path, txn, ArtifactMovePhase::SourcesReclaimed)?;

    pin_transaction_scopes(txn)?;
    remove_transaction(path)?;
    Ok(txn.session_destination_path.clone())
}

fn identity_conflict(
    label: &str,
    path: &Path,
    expected: &ArtifactIdentity,
    actual: &ArtifactIdentity,
) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "{label} artifact conflict at {} (expected {} bytes/{}, found {} bytes/{})",
            path.display(),
            expected.len,
            expected.sha256,
            actual.len,
            actual.sha256
        ),
    )
}

fn import_private_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == ".yututui-inbox")
        })
        .map(Path::to_path_buf)
}

fn backfill_publication_policy(path: &Path, txn: &mut ArtifactMoveTxn) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        if txn.audio_publish_mode.is_none() && txn.sidecar_publish_mode.is_none() {
            let modes = artifact_publish_modes(txn.kind, &txn.destination_root)?;
            if modes.audio.is_some() || modes.sidecar.is_some() {
                txn.audio_publish_mode = modes.audio;
                txn.sidecar_publish_mode = modes.sidecar;
                txn.verify_legacy_existing_modes = matches!(txn.kind, ArtifactMoveKind::Organize);
                if cfg!(all(unix, not(target_os = "linux")))
                    && matches!(txn.kind, ArtifactMoveKind::Organize)
                    && txn.source_private_root.is_some()
                    && txn.destination_private_root.is_none()
                {
                    txn.audio_source_private_stage = txn.audio_stage_object_id.is_none();
                    txn.sidecar_source_private_stage = txn.sidecar_stage_object_id.is_none();
                }
                persist_transaction(path, txn)?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (path, txn);
    Ok(())
}

fn reclaim_private_sources(
    txn: &ArtifactMoveTxn,
    source_parent: &safe_fs::PinnedDir,
) -> anyhow::Result<()> {
    if txn.source_private_root.is_none() {
        return Ok(());
    }
    let audio_name = txn
        .audio_from
        .file_name()
        .context("private source audio has no basename")?;
    let audio = match source_parent.open_existing_child_readonly(audio_name, txn.audio_object_id) {
        Ok(audio) => Some(audio),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("preflight exact private source audio cleanup"),
    };
    let sidecar = match txn.sidecar_object_id {
        Some(expected) => {
            let name = txn
                .sidecar_from
                .file_name()
                .context("private source sidecar has no basename")?;
            match source_parent.open_existing_child_readonly(name, expected) {
                Ok(sidecar) => Some(sidecar),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(error).context("preflight exact private source sidecar cleanup");
                }
            }
        }
        None => None,
    };

    // Both identities are preflighted before either name is removed. POSIX cleanup remains
    // conditional on the private 0700 namespace's sole-writer lease; the safe_fs API refuses
    // user-selected parents and performs one last exact-ID reopen immediately before unlink.
    if let Some(sidecar) = sidecar {
        sidecar
            .remove_from_private_parent()
            .context("remove exact private source sidecar")?;
    }
    if let Some(audio) = audio {
        audio
            .remove_from_private_parent()
            .context("remove exact private source audio")?;
    }
    Ok(())
}

fn advance_phase(
    path: &Path,
    txn: &mut ArtifactMoveTxn,
    next: ArtifactMovePhase,
) -> std::io::Result<()> {
    if txn.phase < next {
        txn.phase = next;
        persist_transaction(path, txn)?;
    }
    Ok(())
}

fn persist_new_transaction(path: &Path, txn: &ArtifactMoveTxn) -> anyhow::Result<()> {
    let _registry = acquire_registry_lock()?;
    let parent = path
        .parent()
        .context("artifact move transaction has no parent")?;
    safe_fs::ensure_private_dir_durable(parent)?;
    let inventory = inventory_transactions_locked()?;
    if inventory.len() >= TXN_MAX_FILES {
        bail!("artifact move registry is full ({TXN_MAX_FILES} transactions)");
    }
    let bytes = transaction_bytes(txn)?;
    let total = inventory.iter().try_fold(0_u64, |total, entry| {
        let len = fs::symlink_metadata(entry)?.len();
        total
            .checked_add(len)
            .ok_or_else(|| std::io::Error::other("artifact move registry size overflow"))
    })?;
    if total.saturating_add(bytes.len() as u64) > TXN_SCAN_MAX_BYTES {
        bail!(
            "artifact move registry exceeds its {} byte scan cap",
            TXN_SCAN_MAX_BYTES
        );
    }
    if path.exists() {
        bail!(
            "artifact move transaction already exists: {}",
            path.display()
        );
    }
    safe_fs::write_private_atomic(path, &bytes)?;
    Ok(())
}

fn persist_transaction(path: &Path, txn: &ArtifactMoveTxn) -> std::io::Result<()> {
    let _registry = acquire_registry_lock().map_err(std::io::Error::other)?;
    let bytes = transaction_bytes(txn).map_err(std::io::Error::other)?;
    safe_fs::write_private_atomic(path, &bytes)
}

fn transaction_bytes(txn: &ArtifactMoveTxn) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(txn)?;
    if bytes.len() as u64 > TXN_MAX_BYTES {
        bail!("artifact move transaction exceeds {TXN_MAX_BYTES} bytes");
    }
    Ok(bytes)
}

fn load_transaction(path: &Path) -> anyhow::Result<ArtifactMoveTxn> {
    let bytes = safe_fs::read_no_symlink_limited(path, TXN_MAX_BYTES)
        .with_context(|| format!("read artifact move transaction {}", path.display()))?;
    let schema = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| value.get("schema_version").and_then(|value| value.as_u64()));
    if schema != Some(u64::from(TXN_SCHEMA_VERSION)) {
        bail!(
            "unsupported artifact move schema {} (parent object identities are required)",
            schema
                .map(|schema| schema.to_string())
                .unwrap_or_else(|| "missing".to_owned())
        );
    }
    let txn: ArtifactMoveTxn = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode artifact move transaction {}", path.display()))?;
    validate_transaction(&txn)?;
    Ok(txn)
}

fn remove_transaction(path: &Path) -> std::io::Result<()> {
    let _registry = acquire_registry_lock().map_err(std::io::Error::other)?;
    safe_fs::remove_private_file_durable(path)
}

fn sync_absent_transaction_parent(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    match fs::symlink_metadata(parent) {
        Ok(_) => {
            reject_symlink_or_non_directory(parent)?;
            safe_fs::sync_parent_dir(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_transaction(txn: &ArtifactMoveTxn) -> anyhow::Result<()> {
    if txn.schema_version != TXN_SCHEMA_VERSION {
        bail!("unsupported artifact move schema {}", txn.schema_version);
    }
    validate_identity_components(&txn.session_id, &txn.row_id, txn.source_order)?;
    let operation_key = transaction_key(&txn.session_id, &txn.row_id, txn.source_order);
    let stage_tag = operation_key
        .get(..16)
        .context("artifact operation key is too short")?;
    if txn.audio_stage_name != format!(".ytt-stage-{stage_tag}-audio")
        || txn.sidecar_stage_name != format!(".ytt-stage-{stage_tag}-sidecar")
    {
        bail!("artifact move contains invalid destination stage names");
    }
    match (txn.kind, txn.claim.as_ref()) {
        (ArtifactMoveKind::ImportDownload, Some(claim))
            if claim.session_id == txn.session_id
                && claim.row_id == txn.row_id
                && claim.source_order == txn.source_order => {}
        (ArtifactMoveKind::ImportDownload, _) => {
            bail!("import artifact move is missing its exact download claim")
        }
        (ArtifactMoveKind::Organize, None) => {}
        (ArtifactMoveKind::Organize, Some(_)) => {
            bail!("organize artifact move must not carry a download claim")
        }
    }
    if txn.sidecar_from != crate::downloads::sidecar_path(&txn.audio_from)
        || txn.sidecar_to != crate::downloads::sidecar_path(&txn.audio_to)
    {
        bail!("artifact move sidecar paths do not match their audio paths");
    }
    validate_digest(&txn.audio_identity)?;
    if txn.audio_identity.len > ARTIFACT_AUDIO_MAX_BYTES {
        bail!("artifact move audio identity exceeds its size limit");
    }
    if let Some(identity) = &txn.sidecar_identity {
        validate_digest(identity)?;
        if identity.len > ARTIFACT_SIDECAR_MAX_BYTES {
            bail!("artifact move sidecar identity exceeds its size limit");
        }
    }
    if txn.sidecar_identity.is_some() != txn.sidecar_object_id.is_some() {
        bail!("artifact move sidecar content/object identities are inconsistent");
    }
    validate_publish_mode(txn.audio_publish_mode)?;
    validate_publish_mode(txn.sidecar_publish_mode)?;
    match (txn.source_private_root.as_ref(), txn.source_private_root_id) {
        (Some(root), Some(_))
            if root
                .file_name()
                .is_some_and(|name| name == ".yututui-inbox")
                && txn.source_parent.starts_with(root) => {}
        (None, None) => {}
        _ => bail!("artifact move private source root is inconsistent"),
    }
    match (
        txn.destination_private_root.as_ref(),
        txn.destination_private_root_id,
    ) {
        (Some(root), Some(_))
            if root
                .file_name()
                .is_some_and(|name| name == ".yututui-inbox")
                && txn.destination_root.starts_with(root)
                && txn.destination_parent.starts_with(root) => {}
        (None, None) => {}
        _ => bail!("artifact move private destination root is inconsistent"),
    }
    if (txn.audio_source_private_stage || txn.sidecar_source_private_stage)
        && (!matches!(txn.kind, ArtifactMoveKind::Organize)
            || txn.source_private_root.is_none()
            || txn.destination_private_root.is_some())
    {
        bail!("artifact move private stage policy is inconsistent");
    }
    reject_parent_components(&txn.audio_from)?;
    reject_parent_components(&txn.audio_to)?;
    reject_parent_components(&txn.session_source_path)?;
    reject_parent_components(&txn.session_destination_path)?;
    if txn.audio_from.parent() != Some(txn.source_parent.as_path())
        || txn.sidecar_from.parent() != Some(txn.source_parent.as_path())
        || txn.audio_to.parent() != Some(txn.destination_parent.as_path())
        || txn.sidecar_to.parent() != Some(txn.destination_parent.as_path())
        || !txn.destination_parent.starts_with(&txn.destination_root)
    {
        bail!("artifact move path escaped its persisted parent scope");
    }
    Ok(())
}

fn validate_transaction_key(txn: &ArtifactMoveTxn, path: &Path, key: &str) -> anyhow::Result<()> {
    validate_transaction(txn)?;
    let expected = transaction_path(key)?;
    if path != expected {
        bail!(
            "artifact move filename does not match its stable identity: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_digest(identity: &ArtifactIdentity) -> anyhow::Result<()> {
    if identity.sha256.len() != 64 || !identity.sha256.bytes().all(is_lower_hex_byte) {
        bail!("artifact move contains an invalid SHA-256 identity");
    }
    Ok(())
}

fn validate_identity_components(
    session_id: &str,
    row_id: &str,
    source_order: u32,
) -> anyhow::Result<()> {
    validate_component("session id", session_id)?;
    validate_component("row id", row_id)?;
    if source_order == 0 {
        bail!("artifact move source order must be positive");
    }
    Ok(())
}

fn validate_request(request: &ArtifactMoveRequest) -> anyhow::Result<()> {
    validate_identity_components(&request.session_id, &request.row_id, request.source_order)?;
    match (request.kind, request.claim.as_ref()) {
        (ArtifactMoveKind::ImportDownload, Some(claim))
            if claim.session_id == request.session_id
                && claim.row_id == request.row_id
                && claim.source_order == request.source_order =>
        {
            super::session::validate_import_download_claim_unlocked(claim, true)?;
        }
        (ArtifactMoveKind::ImportDownload, _) => {
            bail!("import artifact move is missing its exact download claim")
        }
        (ArtifactMoveKind::Organize, None) => {}
        (ArtifactMoveKind::Organize, Some(_)) => {
            bail!("organize artifact move must not carry a download claim")
        }
    }
    reject_parent_components(&request.audio_from)?;
    reject_parent_components(&request.audio_to)?;
    reject_parent_components(&request.destination_root)?;
    let relative = request
        .audio_to
        .strip_prefix(&request.destination_root)
        .with_context(|| {
            format!(
                "artifact destination escaped root: {}",
                request.audio_to.display()
            )
        })?;
    if relative.file_name().is_none() {
        bail!("artifact destination has no file name");
    }
    Ok(())
}

fn validate_component(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > ID_COMPONENT_MAX
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid artifact move {label}: {value:?}");
    }
    Ok(())
}

fn reject_parent_components(path: &Path) -> anyhow::Result<()> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("artifact path must not contain `..`: {}", path.display());
    }
    Ok(())
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn reject_symlink_or_non_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing non-directory artifact scope {}", path.display()),
        ));
    }
    Ok(())
}

fn already_committed_path(request: &ArtifactMoveRequest) -> anyhow::Result<Option<PathBuf>> {
    let session = ImportSession::load(&request.session_id)?;
    let Some(row) = session
        .rows
        .iter()
        .find(|row| row.source_order == request.source_order && row.row_id == request.row_id)
    else {
        bail!(
            "import session row identity changed for {} row #{}",
            request.session_id,
            request.source_order
        );
    };
    if !row.written || row.local_path.as_deref() != Some(request.audio_to.as_path()) {
        return Ok(None);
    }
    let Some(path) =
        committed_session_path(&request.session_id, &request.row_id, request.source_order)?
    else {
        return Ok(None);
    };
    let Some(parent) = request.audio_to.parent() else {
        return Ok(None);
    };
    if !parent.exists() {
        return Ok(None);
    }
    let requested_parent = fs::canonicalize(parent)?;
    let requested = requested_parent.join(
        request
            .audio_to
            .file_name()
            .context("artifact destination has no file name")?,
    );
    let committed = fs::canonicalize(&path)?;
    if committed != requested {
        return Ok(None);
    }
    let Some(reverified) =
        committed_session_path(&request.session_id, &request.row_id, request.source_order)?
    else {
        return Ok(None);
    };
    if fs::canonicalize(&reverified)? == requested {
        Ok(Some(reverified))
    } else {
        Ok(None)
    }
}

fn transaction_key(session_id: &str, row_id: &str, source_order: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update([0]);
    hasher.update(row_id.as_bytes());
    hasher.update([0]);
    hasher.update(source_order.to_le_bytes());
    let digest = hasher.finalize();
    hex_digest(&digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn transaction_dir() -> anyhow::Result<PathBuf> {
    let root = transfers_dir()?;
    Ok(root.join("artifact-moves"))
}

fn transaction_path(key: &str) -> anyhow::Result<PathBuf> {
    if key.len() != 64 || !key.bytes().all(is_lower_hex_byte) {
        bail!("invalid opaque artifact move key");
    }
    Ok(transaction_dir()?.join(format!("{key}.json")))
}

fn transaction_key_from_path(path: &Path) -> anyhow::Result<&str> {
    let key = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".json"))
        .context("invalid artifact move transaction path")?;
    if key.len() != 64 || !key.bytes().all(is_lower_hex_byte) {
        bail!("invalid opaque artifact move transaction path");
    }
    Ok(key)
}

fn txn_lock_path(key: &str) -> anyhow::Result<PathBuf> {
    if key.len() != 64 || !key.bytes().all(is_lower_hex_byte) {
        bail!("invalid opaque artifact move lock key");
    }
    let stripe = key.get(..2).context("invalid artifact move lock stripe")?;
    Ok(transfers_dir()?
        .join("artifact-move-locks")
        .join(format!("{stripe}.lock")))
}

fn transfers_dir() -> anyhow::Result<PathBuf> {
    crate::paths::data_dir()
        .map(|root| root.join("transfers"))
        .ok_or_else(|| anyhow!("no data directory for artifact move recovery"))
}

fn acquire_record_guard(session_id: &str) -> anyhow::Result<ImportRecordGuard> {
    let deadline = Instant::now() + OPERATION_LOCK_WAIT_TIMEOUT;
    loop {
        match ImportRecordGuard::try_acquire(session_id) {
            Ok(guard) => return Ok(guard),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn acquire_txn_lock(key: &str) -> anyhow::Result<ArtifactTxnLock> {
    let lock_path = txn_lock_path(key)?;
    if let Some(parent) = lock_path.parent() {
        safe_fs::ensure_private_dir_durable(parent)?;
    }
    let deadline = Instant::now() + OPERATION_LOCK_WAIT_TIMEOUT;
    loop {
        match safe_fs::try_lock_private_file(&lock_path)? {
            Some(lock) => return Ok(ArtifactTxnLock { _lock: lock }),
            None if Instant::now() < deadline => std::thread::sleep(LOCK_RETRY_DELAY),
            None => bail!(
                "artifact move {key} remained owned for {} seconds",
                OPERATION_LOCK_WAIT_TIMEOUT.as_secs()
            ),
        }
    }
}

fn acquire_registry_lock() -> anyhow::Result<safe_fs::AdvisoryFileLock> {
    let transfers = transfers_dir()?;
    safe_fs::ensure_private_dir_durable(&transfers)?;
    let path = transfers.join("artifact-move-registry.lock");
    let deadline = Instant::now() + REGISTRY_LOCK_WAIT_TIMEOUT;
    loop {
        match safe_fs::try_lock_private_file(&path)? {
            Some(lock) => return Ok(lock),
            None if Instant::now() < deadline => std::thread::sleep(LOCK_RETRY_DELAY),
            None => bail!(
                "artifact move registry remained busy for {} seconds",
                REGISTRY_LOCK_WAIT_TIMEOUT.as_secs()
            ),
        }
    }
}

fn inventory_transactions() -> anyhow::Result<Vec<PathBuf>> {
    let _registry = acquire_registry_lock()?;
    inventory_transactions_locked()
}

fn inventory_transactions_locked() -> anyhow::Result<Vec<PathBuf>> {
    let dir = transaction_dir()?;
    let inventory = inventory_transaction_entries(&dir, TXN_MAX_FILES, TXN_SCAN_MAX_BYTES)?;
    for stale in inventory.stale_temps {
        safe_fs::remove_private_file_durable(&stale).with_context(|| {
            format!(
                "remove stale artifact move transaction temp {}",
                stale.display()
            )
        })?;
    }
    Ok(inventory.transactions)
}

#[cfg(test)]
fn inventory_transactions_in(
    dir: &Path,
    max_files: usize,
    max_total_bytes: u64,
) -> anyhow::Result<Vec<PathBuf>> {
    Ok(inventory_transaction_entries(dir, max_files, max_total_bytes)?.transactions)
}

struct TransactionInventory {
    transactions: Vec<PathBuf>,
    stale_temps: Vec<PathBuf>,
}

fn inventory_transaction_entries(
    dir: &Path,
    max_files: usize,
    max_total_bytes: u64,
) -> anyhow::Result<TransactionInventory> {
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TransactionInventory {
                transactions: Vec::new(),
                stale_temps: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("refusing invalid artifact move registry {}", dir.display())
        }
        Ok(_) => {}
    }
    let mut transactions = Vec::new();
    let mut stale_temps = Vec::new();
    let mut entries = 0_usize;
    let mut total = 0_u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("artifact move registry contains a non-UTF-8 filename"))?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("artifact move registry contains non-file entry {name:?}");
        }
        if metadata.len() > TXN_MAX_BYTES {
            bail!("artifact move transaction {name:?} exceeds {TXN_MAX_BYTES} bytes");
        }
        entries += 1;
        if entries > max_files {
            bail!("artifact move registry exceeds its {max_files} file cap");
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| anyhow!("artifact move registry size overflow"))?;
        if total > max_total_bytes {
            bail!(
                "artifact move registry exceeds its {} byte scan cap",
                max_total_bytes
            );
        }
        if is_transaction_name(name) {
            transactions.push(path);
        } else if is_transaction_temp_name(name) {
            stale_temps.push(path);
        } else {
            bail!("artifact move registry contains invalid entry {name:?}");
        }
    }
    transactions.sort();
    stale_temps.sort();
    Ok(TransactionInventory {
        transactions,
        stale_temps,
    })
}

fn is_transaction_name(name: &str) -> bool {
    name.strip_suffix(".json")
        .is_some_and(|key| key.len() == 64 && key.bytes().all(is_lower_hex_byte))
}

fn is_transaction_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let Some((target, suffix)) = rest.split_once(".tmp.") else {
        return false;
    };
    if !is_transaction_name(target) {
        return false;
    }
    let mut suffix = suffix.split('.');
    let Some(pid) = suffix.next() else {
        return false;
    };
    let Some(token) = suffix.next() else {
        return false;
    };
    suffix.next().is_none()
        && !pid.is_empty()
        && pid.len() <= 20
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && token.len() == 16
        && token.bytes().all(is_lower_hex_byte)
}

fn is_lower_hex_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn inject(
    fault: Option<&dyn Fn(ArtifactMoveFaultPoint) -> std::io::Result<()>>,
    point: ArtifactMoveFaultPoint,
) -> std::io::Result<()> {
    match fault {
        Some(fault) => fault(point),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
