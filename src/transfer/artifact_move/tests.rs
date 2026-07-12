use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::transfer::session::{ImportSessionRow, ImportSessionRowStatus};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct MoveFixture {
    root: PathBuf,
    session_id: String,
    row_id: String,
    source_order: u32,
    source: PathBuf,
    destination: PathBuf,
    request: ArtifactMoveRequest,
}

impl MoveFixture {
    fn new(label: &str, kind: ArtifactMoveKind) -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("sp2yt-artifact-{label}-{}-{sequence}", std::process::id());
        let row_id = "row-00001".to_owned();
        let source_order = 1;
        let root = std::env::temp_dir().join(format!(
            "yututui-artifact-{label}-{}-{}-{sequence}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let source_dir = root.join("incoming");
        let destination_root = root.join("complete");
        std::fs::create_dir_all(&source_dir).expect("create source directory");
        std::fs::create_dir_all(&destination_root).expect("create destination directory");
        let source = source_dir.join("Track [abc123def45].m4a");
        let destination = destination_root.join("Track [abc123def45].m4a");
        std::fs::write(&source, b"fixture audio").expect("write source audio");
        std::fs::write(crate::downloads::sidecar_path(&source), b"fixture sidecar")
            .expect("write source sidecar");

        let organize = matches!(kind, ArtifactMoveKind::Organize);
        let claim = crate::transfer::artifact_identity::test_import_claim(
            &session_id,
            &row_id,
            source_order,
            "abc123def45",
        );
        let session = ImportSession {
            session_id: session_id.clone(),
            session_instance_id: claim.session_instance_id.clone(),
            rows: vec![ImportSessionRow {
                row_id: row_id.clone(),
                source_order,
                status: ImportSessionRowStatus::Matched,
                title: "Track".to_owned(),
                source_key: "spotify:track:fixture".to_owned(),
                selected_key: Some("abc123def45".to_owned()),
                download_claim: (!organize).then(|| claim.clone()),
                written: organize,
                local_path: organize.then(|| source.clone()),
                ..ImportSessionRow::default()
            }],
            ..ImportSession::default()
        };
        session.save().expect("save artifact fixture session");

        let request = match kind {
            ArtifactMoveKind::ImportDownload => ArtifactMoveRequest::import_download(
                session_id.clone(),
                row_id.clone(),
                source_order,
                claim,
                source.clone(),
                destination.clone(),
                destination_root,
            ),
            ArtifactMoveKind::Organize => ArtifactMoveRequest::organize(
                session_id.clone(),
                row_id.clone(),
                source_order,
                source.clone(),
                destination.clone(),
                destination_root,
            ),
        };
        Self {
            root,
            session_id,
            row_id,
            source_order,
            source,
            destination,
            request,
        }
    }

    fn transaction_path(&self) -> PathBuf {
        let key = transaction_key(&self.session_id, &self.row_id, self.source_order);
        transaction_path(&key).expect("transaction path")
    }

    fn into_private_inbox(mut self) -> Self {
        let private_root = self.root.join(".yututui-inbox");
        let session = private_root.join(&self.session_id);
        let incoming = session.join("incoming");
        let complete = session.join("complete");
        for directory in [&private_root, &session, &incoming, &complete] {
            crate::util::safe_fs::ensure_private_dir(directory).unwrap();
        }
        let source = incoming.join(self.source.file_name().unwrap());
        let destination = complete.join(self.destination.file_name().unwrap());
        std::fs::rename(&self.source, &source).unwrap();
        std::fs::rename(
            crate::downloads::sidecar_path(&self.source),
            crate::downloads::sidecar_path(&source),
        )
        .unwrap();
        self.source = source.clone();
        self.destination = destination.clone();
        self.request.audio_from = source;
        self.request.audio_to = destination;
        self.request.destination_root = complete;
        self
    }

    fn assert_committed_once(&self) {
        assert!(self.source.is_file(), "retained source audio is missing");
        assert!(self.destination.is_file(), "destination audio is missing");
        assert_eq!(
            std::fs::read(&self.destination).expect("read committed audio"),
            b"fixture audio"
        );
        let source_sidecar = crate::downloads::sidecar_path(&self.source);
        let destination_sidecar = crate::downloads::sidecar_path(&self.destination);
        assert!(
            source_sidecar.is_file(),
            "retained source sidecar is missing"
        );
        assert!(destination_sidecar.is_file(), "destination sidecar missing");
        assert_eq!(
            std::fs::read(destination_sidecar).expect("read committed sidecar"),
            b"fixture sidecar"
        );
        assert_eq!(regular_file_count(self.source.parent().unwrap()), 2);
        assert_eq!(regular_file_count(self.destination.parent().unwrap()), 2);
        let source_file = safe_fs::open_regular_no_symlink(&self.source).unwrap();
        let destination_file = safe_fs::open_regular_no_symlink(&self.destination).unwrap();
        assert_ne!(
            safe_fs::file_object_id(&source_file).unwrap(),
            safe_fs::file_object_id(&destination_file).unwrap(),
            "destination must not alias the mutable retained source"
        );

        let saved = ImportSession::load(&self.session_id).expect("load committed session");
        let row = &saved.rows[0];
        assert!(row.written);
        assert_eq!(
            std::fs::canonicalize(row.local_path.as_ref().expect("committed row path"))
                .expect("canonical committed row path"),
            std::fs::canonicalize(&self.destination).expect("canonical destination")
        );
        assert!(!self.transaction_path().exists());
    }

    fn cleanup(&self) {
        let transaction = self.transaction_path();
        if transaction.exists() {
            let _ = remove_transaction(&transaction);
        }
        let _ = ImportSession::delete_record(&self.session_id);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn regular_file_count(directory: &Path) -> usize {
    std::fs::read_dir(directory)
        .expect("read fixture directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .count()
}

fn private_reconcile_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "yututui-reconcile-{label}-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    for directory in [&root, &root.join("source"), &root.join("destination")] {
        safe_fs::ensure_private_dir(directory).unwrap();
    }
    root
}

fn artifact_identity(path: &Path, max_bytes: u64) -> ArtifactIdentity {
    let mut file = safe_fs::open_regular_no_symlink(path).unwrap();
    file_identity_from_open(&mut file, path, max_bytes).unwrap()
}

#[test]
fn session_reconcile_ignores_a_transaction_removed_after_inventory() {
    let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let session_id = format!(
        "sp2yt-artifact-stale-snapshot-{}-{sequence}",
        std::process::id()
    );
    let key = transaction_key(&session_id, "row-00001", 1);
    let stale_snapshot_path = transaction_path(&key).unwrap();
    assert!(!stale_snapshot_path.exists());

    // This path represents an inventory entry whose owner completed and removed it before this
    // session reached the entry. The session record lock is the caller contract of the helper.
    let _record_guard = ImportRecordGuard::try_acquire(&session_id).unwrap();
    let recovered = reconcile_session_paths_locked(&session_id, vec![stale_snapshot_path]).unwrap();

    assert_eq!(recovered, 0);
}

#[test]
fn every_durable_phase_rolls_forward_to_exactly_one_pair_and_row() {
    let cases = [
        (
            "after-audio",
            ArtifactMoveKind::ImportDownload,
            ArtifactMoveFaultPoint::AfterAudioMove,
        ),
        (
            "after-sidecar",
            ArtifactMoveKind::ImportDownload,
            ArtifactMoveFaultPoint::AfterSidecarMove,
        ),
        (
            "before-session",
            ArtifactMoveKind::Organize,
            ArtifactMoveFaultPoint::BeforeSessionSave,
        ),
        (
            "after-session",
            ArtifactMoveKind::Organize,
            ArtifactMoveFaultPoint::AfterSessionSave,
        ),
        (
            "after-import-session",
            ArtifactMoveKind::ImportDownload,
            ArtifactMoveFaultPoint::AfterSessionSave,
        ),
    ];

    for (label, kind, fault) in cases {
        let fixture = MoveFixture::new(label, kind);
        let error = commit_with_fault(fixture.request.clone(), fault)
            .expect_err("fault injection must stop the first commit");
        assert!(
            format!("{error:#}").contains("fault injection"),
            "unexpected injected commit error: {error:#}"
        );
        let transaction = fixture.transaction_path();
        assert!(transaction.is_file(), "fault must retain durable intent");
        let name = transaction
            .file_name()
            .and_then(|name| name.to_str())
            .expect("opaque transaction filename");
        assert_eq!(name.len(), 69);
        assert!(!name.contains(&fixture.session_id));
        assert!(!name.contains(&fixture.row_id));

        let report = reconcile_all_pending().expect("startup reconciliation");
        assert!(
            report
                .warnings
                .iter()
                .all(|warning| !warning.contains(name)),
            "fixture transaction was not recovered: {:?}",
            report.warnings
        );
        fixture.assert_committed_once();

        let retry = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
            .expect("retry reconciliation")
            .expect("durably committed row must be retry success");
        assert_eq!(
            std::fs::canonicalize(retry).expect("canonical retry path"),
            std::fs::canonicalize(&fixture.destination).expect("canonical retry destination")
        );
        fixture.assert_committed_once();
        fixture.cleanup();
    }
}

#[test]
fn legacy_transaction_schema_is_refused_without_mutating_artifacts() {
    let fixture = MoveFixture::new("legacy-schema", ArtifactMoveKind::ImportDownload);
    // Keep startup reconciliation from observing the transaction as current-schema in the
    // interval between its creation and the deliberate legacy-schema rewrite below.
    let record_guard =
        ImportRecordGuard::try_acquire(&fixture.session_id).expect("own legacy fixture session");
    let fault = |point| {
        if point == ArtifactMoveFaultPoint::BeforeAudioPublish {
            Err(std::io::Error::other("legacy fixture fault injection"))
        } else {
            Ok(())
        }
    };
    commit_locked_with_hook(fixture.request.clone(), Some(&fault))
        .expect_err("fault must retain a prepared transaction");
    let transaction = fixture.transaction_path();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&transaction).unwrap()).unwrap();
    value["schema_version"] = serde_json::Value::from(TXN_SCHEMA_VERSION - 1);
    {
        // Production transaction rewrites hold this lock so inventory cannot mistake their
        // live atomic-write temp for a stale crash remnant. Mirror that contract here.
        let _registry = acquire_registry_lock().expect("own transaction registry");
        safe_fs::write_private_atomic_json(&transaction, &value).unwrap();
    }
    drop(record_guard);

    let error = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect_err("legacy transaction must not infer missing object identities");

    assert!(format!("{error:#}").contains("unsupported artifact move schema"));
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"fixture audio");
    assert!(!fixture.destination.exists());
    assert!(transaction.exists());
    fixture.cleanup();
}

#[test]
fn replaced_source_parent_is_rejected_before_opening_a_foreign_source() {
    let fixture = MoveFixture::new("source-parent-swap", ArtifactMoveKind::ImportDownload);
    commit_with_fault(
        fixture.request.clone(),
        ArtifactMoveFaultPoint::BeforeAudioPublish,
    )
    .expect_err("fault must retain transaction scope identities");
    let source_parent = fixture.source.parent().unwrap().to_path_buf();
    let displaced = fixture.root.join("displaced-incoming");
    std::fs::rename(&source_parent, &displaced).unwrap();
    std::fs::create_dir(&source_parent).unwrap();
    std::fs::write(&fixture.source, b"foreign source").unwrap();

    let error = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect_err("replacement source parent must fail its persisted object check");

    assert!(format!("{error:#}").contains("source parent object changed"));
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"foreign source");
    assert_eq!(
        std::fs::read(displaced.join(fixture.source.file_name().unwrap())).unwrap(),
        b"fixture audio"
    );
    assert!(!fixture.destination.exists());
    assert!(fixture.transaction_path().exists());
    fixture.cleanup();
}

#[test]
fn replaced_destination_scope_never_receives_a_published_generation() {
    let fixture = MoveFixture::new("destination-parent-swap", ArtifactMoveKind::ImportDownload);
    commit_with_fault(
        fixture.request.clone(),
        ArtifactMoveFaultPoint::BeforeAudioPublish,
    )
    .expect_err("fault must retain transaction scope identities");
    let destination_parent = fixture.destination.parent().unwrap().to_path_buf();
    let displaced = fixture.root.join("displaced-complete");
    std::fs::rename(&destination_parent, &displaced).unwrap();
    std::fs::create_dir(&destination_parent).unwrap();

    let error = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect_err("replacement destination scope must fail its persisted object check");

    assert!(format!("{error:#}").contains("destination root object changed"));
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"fixture audio");
    assert_eq!(regular_file_count(&destination_parent), 0);
    #[cfg(not(windows))]
    assert_eq!(regular_file_count(&displaced), 0);
    #[cfg(windows)]
    assert_eq!(regular_file_count(&displaced), 1);
    assert!(fixture.transaction_path().exists());
    fixture.cleanup();
}

#[test]
fn destination_created_at_publish_race_is_never_overwritten() {
    let fixture = MoveFixture::new("publish-race", ArtifactMoveKind::ImportDownload);
    let destination = fixture.destination.clone();
    let injected = AtomicBool::new(false);
    let hook = |point| {
        if point == ArtifactMoveFaultPoint::BeforeAudioPublish
            && !injected.swap(true, Ordering::SeqCst)
        {
            std::fs::write(&destination, b"foreign destination")?;
        }
        Ok(())
    };
    let _guard = ImportRecordGuard::try_acquire(&fixture.session_id).expect("own fixture session");
    let error = commit_locked_with_hook(fixture.request.clone(), Some(&hook))
        .expect_err("no-replace publish must preserve the racing destination");
    assert!(injected.load(Ordering::SeqCst));
    assert!(format!("{error:#}").contains("reconcile import audio move"));
    assert_eq!(
        std::fs::read(&fixture.destination).expect("read racing destination"),
        b"foreign destination"
    );
    assert_eq!(
        std::fs::read(&fixture.source).expect("read preserved source"),
        b"fixture audio"
    );
    assert!(crate::downloads::sidecar_path(&fixture.source).is_file());
    assert!(fixture.transaction_path().is_file());
    drop(_guard);

    std::fs::remove_file(&fixture.destination).expect("remove external collision");
    reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect("retry after collision removal")
        .expect("retry must finish the transaction");
    fixture.assert_committed_once();
    fixture.cleanup();
}

#[test]
fn matching_existing_destination_with_a_different_file_id_is_idempotent() {
    let fixture = MoveFixture::new("matching-destination", ArtifactMoveKind::ImportDownload);
    std::fs::copy(&fixture.source, &fixture.destination).unwrap();
    std::fs::copy(
        crate::downloads::sidecar_path(&fixture.source),
        crate::downloads::sidecar_path(&fixture.destination),
    )
    .unwrap();

    let committed = commit(fixture.request.clone())
        .expect("exact no-replace destination content is idempotent");

    assert_eq!(committed, fixture.destination);
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"fixture audio");
    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"fixture audio"
    );
    assert!(!fixture.transaction_path().exists());
    fixture.cleanup();
}

#[test]
fn matching_final_with_a_journal_owned_named_stage_is_not_forgotten() {
    let root = private_reconcile_root("matching-final-retained-stage");
    let source_dir = root.join("source");
    let destination_dir = root.join("destination");
    let source_path = source_dir.join("source.audio");
    let stage_path = destination_dir.join("owned.stage");
    let final_path = destination_dir.join("final.audio");
    std::fs::write(&source_path, b"identical payload").unwrap();
    std::fs::write(&stage_path, b"identical payload").unwrap();
    std::fs::write(&final_path, b"identical payload").unwrap();
    let source_parent = safe_fs::PinnedDir::open_private_existing(&root, Path::new("source"))
        .expect("pin private source");
    let destination_parent =
        safe_fs::PinnedDir::open_private_existing(&root, Path::new("destination"))
            .expect("pin private destination");
    let source = source_parent
        .open_child_readonly(std::ffi::OsStr::new("source.audio"))
        .unwrap();
    let stage = destination_parent
        .open_child_readonly(std::ffi::OsStr::new("owned.stage"))
        .unwrap();
    let expected = artifact_identity(&source_path, ARTIFACT_AUDIO_MAX_BYTES);
    let mut record_stage = |_| panic!("an existing journal stage must not be re-recorded");

    let error = reconcile_file_pair(
        ReconcileFilePair {
            source_parent: &source_parent,
            source_name: std::ffi::OsStr::new("source.audio"),
            expected_source_object: source.identity(),
            destination_parent: &destination_parent,
            destination_stage_name: std::ffi::OsStr::new("owned.stage"),
            expected_stage_object: Some(stage.identity()),
            destination_name: std::ffi::OsStr::new("final.audio"),
            expected: &expected,
        },
        &mut record_stage,
        ReconcileFilePolicy {
            fault: None,
            before_publish: ArtifactMoveFaultPoint::BeforeAudioPublish,
            after_publish: ArtifactMoveFaultPoint::AfterAudioPublishBeforeSourceUnlink,
            retained_source_boundary: ArtifactMoveFaultPoint::BeforeAudioSourceUnlinkValidation,
            max_bytes: ARTIFACT_AUDIO_MAX_BYTES,
        },
    )
    .expect_err("a retained named stage requires explicit recovery");

    assert!(
        error
            .to_string()
            .contains("journal-owned named stage is retained")
    );
    assert_eq!(std::fs::read(source_path).unwrap(), b"identical payload");
    assert_eq!(std::fs::read(stage_path).unwrap(), b"identical payload");
    assert_eq!(std::fs::read(final_path).unwrap(), b"identical payload");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn journal_owned_partial_stage_is_repaired_and_promoted() {
    let root = private_reconcile_root("repair-partial-stage");
    let source_dir = root.join("source");
    let destination_dir = root.join("destination");
    let source_path = source_dir.join("source.audio");
    let stage_path = destination_dir.join("owned.stage");
    let final_path = destination_dir.join("final.audio");
    std::fs::write(&source_path, b"complete source payload").unwrap();
    std::fs::write(&stage_path, b"partial").unwrap();
    let source_parent = safe_fs::PinnedDir::open_private_existing(&root, Path::new("source"))
        .expect("pin private source");
    let destination_parent =
        safe_fs::PinnedDir::open_private_existing(&root, Path::new("destination"))
            .expect("pin private destination");
    let source = source_parent
        .open_child_readonly(std::ffi::OsStr::new("source.audio"))
        .unwrap();
    let stage = destination_parent
        .open_child_readonly(std::ffi::OsStr::new("owned.stage"))
        .unwrap();
    let expected = artifact_identity(&source_path, ARTIFACT_AUDIO_MAX_BYTES);
    let mut record_stage = |_| panic!("an existing journal stage must not be re-recorded");

    reconcile_file_pair(
        ReconcileFilePair {
            source_parent: &source_parent,
            source_name: std::ffi::OsStr::new("source.audio"),
            expected_source_object: source.identity(),
            destination_parent: &destination_parent,
            destination_stage_name: std::ffi::OsStr::new("owned.stage"),
            expected_stage_object: Some(stage.identity()),
            destination_name: std::ffi::OsStr::new("final.audio"),
            expected: &expected,
        },
        &mut record_stage,
        ReconcileFilePolicy {
            fault: None,
            before_publish: ArtifactMoveFaultPoint::BeforeAudioPublish,
            after_publish: ArtifactMoveFaultPoint::AfterAudioPublishBeforeSourceUnlink,
            retained_source_boundary: ArtifactMoveFaultPoint::BeforeAudioSourceUnlinkValidation,
            max_bytes: ARTIFACT_AUDIO_MAX_BYTES,
        },
    )
    .expect("repair and promote journal-owned stage");

    assert!(!stage_path.exists());
    assert_eq!(
        std::fs::read(final_path).unwrap(),
        b"complete source payload"
    );
    assert_eq!(
        std::fs::read(source_path).unwrap(),
        b"complete source payload"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn matching_existing_destination_hardlink_is_idempotent_cleanup() {
    let fixture = MoveFixture::new("matching-hardlink", ArtifactMoveKind::ImportDownload);
    std::fs::hard_link(&fixture.source, &fixture.destination).unwrap();
    std::fs::hard_link(
        crate::downloads::sidecar_path(&fixture.source),
        crate::downloads::sidecar_path(&fixture.destination),
    )
    .unwrap();

    let error = commit(fixture.request.clone())
        .expect_err("final destination must never alias its retained source");
    assert!(format!("{error:#}").contains("aliases the retained source"));
    assert!(fixture.transaction_path().is_file());
    fixture.cleanup();
}

#[test]
fn retained_source_mutation_cannot_change_the_committed_destination() {
    let fixture = MoveFixture::new("source-independence", ArtifactMoveKind::ImportDownload);
    commit(fixture.request.clone()).expect("commit independent destination generation");

    std::fs::write(&fixture.source, b"mutated retained source").unwrap();
    std::fs::write(
        crate::downloads::sidecar_path(&fixture.source),
        b"mutated retained sidecar",
    )
    .unwrap();

    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"fixture audio"
    );
    assert_eq!(
        std::fs::read(crate::downloads::sidecar_path(&fixture.destination)).unwrap(),
        b"fixture sidecar"
    );
    fixture.cleanup();
}

#[test]
fn successful_private_inbox_commit_reclaims_sources_and_leaves_no_stage() {
    let fixture =
        MoveFixture::new("private-reclaim", ArtifactMoveKind::ImportDownload).into_private_inbox();

    commit(fixture.request.clone()).expect("commit private inbox generation");

    assert!(!fixture.source.exists());
    assert!(!crate::downloads::sidecar_path(&fixture.source).exists());
    assert_eq!(regular_file_count(fixture.source.parent().unwrap()), 0);
    assert_eq!(regular_file_count(fixture.destination.parent().unwrap()), 2);
    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"fixture audio"
    );
    assert!(!fixture.transaction_path().exists());
    fixture.cleanup();
}

#[test]
fn private_source_swap_at_cleanup_boundary_preserves_the_foreign_file() {
    let fixture = MoveFixture::new("private-source-swap", ArtifactMoveKind::ImportDownload)
        .into_private_inbox();
    let source = fixture.source.clone();
    let moved = source.with_file_name("moved-owned-audio");
    let injected = AtomicBool::new(false);
    let hook = |point| {
        if point == ArtifactMoveFaultPoint::BeforeAudioSourceUnlinkValidation
            && !injected.swap(true, Ordering::SeqCst)
        {
            std::fs::rename(&source, &moved)?;
            std::fs::write(&source, b"foreign replacement")?;
        }
        Ok(())
    };
    let _guard = ImportRecordGuard::try_acquire(&fixture.session_id).unwrap();

    let error = commit_locked_with_hook(fixture.request.clone(), Some(&hook))
        .expect_err("foreign private source replacement must stop exact cleanup");

    assert!(injected.load(Ordering::SeqCst));
    assert!(format!("{error:#}").contains("private source audio cleanup"));
    assert_eq!(std::fs::read(&source).unwrap(), b"foreign replacement");
    assert_eq!(std::fs::read(&moved).unwrap(), b"fixture audio");
    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"fixture audio"
    );
    assert!(fixture.transaction_path().exists());
    drop(_guard);
    fixture.cleanup();
}

#[cfg(unix)]
#[test]
fn read_only_sources_commit_through_read_only_handles() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = MoveFixture::new("read-only-source", ArtifactMoveKind::ImportDownload);
    std::fs::set_permissions(&fixture.source, std::fs::Permissions::from_mode(0o444)).unwrap();
    std::fs::set_permissions(
        crate::downloads::sidecar_path(&fixture.source),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();

    commit(fixture.request.clone()).expect("read-only source commit");

    fixture.assert_committed_once();
    fixture.cleanup();
}

#[test]
fn destination_replaced_after_publish_never_authorizes_source_unlink() {
    let fixture = MoveFixture::new("replace-after-publish", ArtifactMoveKind::ImportDownload);
    let destination = fixture.destination.clone();
    let injected = AtomicBool::new(false);
    let hook = |point| {
        if point == ArtifactMoveFaultPoint::AfterAudioPublishBeforeSourceUnlink
            && !injected.swap(true, Ordering::SeqCst)
        {
            std::fs::remove_file(&destination)?;
            std::fs::write(&destination, b"foreign destination")?;
        }
        Ok(())
    };
    let _guard = ImportRecordGuard::try_acquire(&fixture.session_id).expect("own fixture session");
    let error = commit_locked_with_hook(fixture.request.clone(), Some(&hook))
        .expect_err("a replaced destination must retain the source and transaction");
    assert!(injected.load(Ordering::SeqCst));
    assert!(format!("{error:#}").contains("reconcile import audio move"));
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"fixture audio");
    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"foreign destination"
    );
    assert!(fixture.transaction_path().is_file());
    drop(_guard);

    std::fs::remove_file(&fixture.destination).unwrap();
    reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect("retry after removing foreign destination")
        .expect("pending move should complete");
    fixture.assert_committed_once();
    fixture.cleanup();
}

#[test]
fn destination_removed_at_final_unlink_boundary_preserves_source() {
    let fixture = MoveFixture::new("final-unlink-race", ArtifactMoveKind::ImportDownload);
    let destination = fixture.destination.clone();
    let injected = AtomicBool::new(false);
    let hook = |point| {
        if point == ArtifactMoveFaultPoint::BeforeAudioSourceUnlinkValidation
            && !injected.swap(true, Ordering::SeqCst)
        {
            std::fs::remove_file(&destination)?;
        }
        Ok(())
    };
    let _guard = ImportRecordGuard::try_acquire(&fixture.session_id).expect("own fixture session");
    let error = commit_locked_with_hook(fixture.request.clone(), Some(&hook))
        .expect_err("the final destination re-open must fail before source unlink");
    assert!(injected.load(Ordering::SeqCst));
    assert!(format!("{error:#}").contains("reconcile import audio move"));
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"fixture audio");
    assert!(!fixture.destination.exists());
    assert!(fixture.transaction_path().is_file());
    drop(_guard);

    reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect("retry after final-boundary removal")
        .expect("pending move should complete");
    fixture.assert_committed_once();
    fixture.cleanup();
}

#[test]
fn identity_hash_reads_at_most_initial_length_plus_one() {
    struct InfiniteReader {
        reads: u64,
    }

    impl std::io::Read for InfiniteReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += buffer.len() as u64;
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    let mut reader = InfiniteReader { reads: 0 };
    let error = hash_exact_len(&mut reader, 1_024)
        .expect_err("an appended byte must make the fixed snapshot invalid");
    assert!(error.to_string().contains("expected 1024 bytes, read 1025"));
    assert_eq!(reader.reads, 1_025, "hashing must not chase a growing EOF");
}

#[test]
fn oversized_sparse_audio_is_rejected_before_transaction_creation() {
    let fixture = MoveFixture::new("oversized-audio", ArtifactMoveKind::ImportDownload);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&fixture.source)
        .unwrap()
        .set_len(ARTIFACT_AUDIO_MAX_BYTES + 1)
        .unwrap();

    let error = commit(fixture.request.clone()).expect_err("oversized audio must not be hashed");

    assert!(format!("{error:#}").contains("identity limit"));
    assert!(!fixture.transaction_path().exists());
    assert_eq!(
        std::fs::metadata(&fixture.source).unwrap().len(),
        ARTIFACT_AUDIO_MAX_BYTES + 1
    );
    fixture.cleanup();
}

#[test]
fn removed_transaction_never_reports_done_for_replaced_destination() {
    let fixture = MoveFixture::new("receipt-mismatch", ArtifactMoveKind::ImportDownload);
    commit(fixture.request.clone()).expect("commit receipt fixture");
    std::fs::write(&fixture.destination, b"foreign replacement").unwrap();

    let error = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect_err("a receipt mismatch must not become false Done");

    assert!(format!("{error:#}").contains("receipt conflict"));
    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"foreign replacement"
    );
    let session = ImportSession::load(&fixture.session_id).unwrap();
    assert!(session.rows[0].written);
    assert!(session.rows[0].artifact_receipt.is_some());
    fixture.cleanup();
}

#[test]
fn missing_receipted_audio_clears_committed_row_for_redownload() {
    let fixture = MoveFixture::new("receipt-missing", ArtifactMoveKind::ImportDownload);
    commit(fixture.request.clone()).expect("commit missing receipt fixture");
    std::fs::remove_file(&fixture.destination).unwrap();

    let outcome = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect("missing audio should become an explicit redownload outcome");

    assert!(outcome.is_none());
    let session = ImportSession::load(&fixture.session_id).unwrap();
    assert!(!session.rows[0].written);
    assert!(session.rows[0].local_path.is_none());
    assert!(session.rows[0].artifact_receipt.is_none());
    fixture.cleanup();
}

#[test]
fn legacy_written_row_without_receipt_is_verified_and_promoted() {
    let fixture = MoveFixture::new("receipt-legacy", ArtifactMoveKind::ImportDownload);
    commit(fixture.request.clone()).expect("commit legacy receipt fixture");
    let mut session = ImportSession::load(&fixture.session_id).unwrap();
    session.rows[0].artifact_receipt = None;
    let song = super::super::session::import_song_for_row(&fixture.session_id, &session.rows[0])
        .expect("rebuild legacy sidecar metadata");
    std::fs::remove_file(crate::downloads::sidecar_path(&fixture.destination)).unwrap();
    crate::downloads::write_sidecar(&song, &fixture.destination)
        .expect("replace synthetic fixture with a valid legacy sidecar");
    session.save().unwrap();

    let reconciled = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .expect("bounded legacy verification")
        .expect("verified legacy row is done");

    assert_eq!(reconciled, fixture.destination);
    let promoted = ImportSession::load(&fixture.session_id).unwrap();
    assert!(promoted.rows[0].artifact_receipt.is_some());
    assert_eq!(
        std::fs::read(&fixture.destination).unwrap(),
        b"fixture audio"
    );
    fixture.cleanup();
}

#[test]
fn old_shape_resave_is_boundedly_verified_and_promoted() {
    let fixture = MoveFixture::new("receipt-old-shape", ArtifactMoveKind::ImportDownload);
    let session = ImportSession::load(&fixture.session_id).unwrap();
    let song = super::super::session::import_song_for_row(&fixture.session_id, &session.rows[0])
        .expect("build deterministic sidecar");
    std::fs::remove_file(crate::downloads::sidecar_path(&fixture.source)).unwrap();
    crate::downloads::write_sidecar(&song, &fixture.source).unwrap();
    commit(fixture.request.clone()).unwrap();

    let current = ImportSession::load(&fixture.session_id).unwrap();
    let mut old_shape = serde_json::to_value(&current).unwrap();
    let object = old_shape.as_object_mut().unwrap();
    object.remove("session_instance_id");
    let row = object
        .get_mut("rows")
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|rows| rows.first_mut())
        .and_then(serde_json::Value::as_object_mut)
        .unwrap();
    row.remove("revision");
    row.remove("download_claim");
    row.remove("artifact_receipt");
    std::fs::write(
        super::super::session::session_path(&fixture.session_id).unwrap(),
        serde_json::to_vec_pretty(&old_shape).unwrap(),
    )
    .unwrap();

    let reconciled = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .unwrap()
        .expect("old-shape row promoted");
    assert_eq!(reconciled, fixture.destination);
    let promoted = ImportSession::load(&fixture.session_id).unwrap();
    assert_eq!(promoted.rows[0].revision, 1);
    assert!(promoted.rows[0].artifact_receipt.is_some());
    fixture.cleanup();
}

#[test]
fn missing_audio_then_redownload_commits_with_retained_identical_sidecar() {
    let fixture = MoveFixture::new("receipt-redownload-cycle", ArtifactMoveKind::ImportDownload);
    commit(fixture.request.clone()).unwrap();
    let destination_sidecar = crate::downloads::sidecar_path(&fixture.destination);
    std::fs::remove_file(&fixture.destination).unwrap();
    assert!(
        reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
            .unwrap()
            .is_none()
    );

    let claim = super::super::session::claim_import_download(
        &fixture.session_id,
        fixture.source_order,
        "abc123def45",
    )
    .unwrap();
    std::fs::write(&fixture.source, b"fixture audio").unwrap();
    let source_sidecar = crate::downloads::sidecar_path(&fixture.source);
    std::fs::copy(&destination_sidecar, &source_sidecar).unwrap();
    let request = ArtifactMoveRequest::import_download(
        fixture.session_id.clone(),
        fixture.row_id.clone(),
        fixture.source_order,
        claim.clone(),
        fixture.source.clone(),
        fixture.destination.clone(),
        fixture.destination.parent().unwrap().to_path_buf(),
    );
    commit(request).expect("redownload cycle commits without deleting an unowned sidecar");

    assert!(fixture.destination.is_file());
    assert!(destination_sidecar.is_file());
    assert!(
        source_sidecar.is_file(),
        "distinct equal sidecar is retained"
    );
    let saved = ImportSession::load(&fixture.session_id).unwrap();
    assert!(saved.rows[0].written);
    assert_eq!(
        saved.rows[0]
            .artifact_receipt
            .as_ref()
            .and_then(|receipt| receipt.claim.as_ref()),
        Some(&claim)
    );
    fixture.cleanup();
}

#[test]
fn missing_receipted_sidecar_is_rebuilt_before_done() {
    let fixture = MoveFixture::new("receipt-sidecar-repair", ArtifactMoveKind::ImportDownload);
    let session = ImportSession::load(&fixture.session_id).unwrap();
    let song = super::super::session::import_song_for_row(&fixture.session_id, &session.rows[0])
        .expect("build deterministic sidecar");
    std::fs::remove_file(crate::downloads::sidecar_path(&fixture.source)).unwrap();
    crate::downloads::write_sidecar(&song, &fixture.source).unwrap();
    commit(fixture.request.clone()).unwrap();
    let sidecar = crate::downloads::sidecar_path(&fixture.destination);
    let expected = crate::downloads::sidecar_bytes(&song).unwrap();
    std::fs::remove_file(&sidecar).unwrap();

    let reconciled = reconcile_row(&fixture.session_id, &fixture.row_id, fixture.source_order)
        .unwrap()
        .expect("missing deterministic sidecar is repaired");

    assert_eq!(reconciled, fixture.destination);
    assert_eq!(std::fs::read(sidecar).unwrap(), expected);
    assert!(ImportSession::load(&fixture.session_id).unwrap().rows[0].written);
    fixture.cleanup();
}

#[cfg(unix)]
#[test]
fn destination_symlink_escape_is_rejected_before_external_directory_creation() {
    use std::os::unix::fs::symlink;

    let mut fixture = MoveFixture::new("symlink-escape", ArtifactMoveKind::ImportDownload);
    let destination_root = fixture.root.join("library");
    let outside = fixture.root.join("outside");
    std::fs::create_dir_all(&destination_root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, destination_root.join("linked")).unwrap();
    fixture.destination = destination_root
        .join("linked")
        .join("created-outside")
        .join("Track.m4a");
    fixture.request.audio_to = fixture.destination.clone();
    fixture.request.destination_root = destination_root;

    let error = commit(fixture.request.clone()).expect_err("symlinked scope must be rejected");
    assert!(format!("{error:#}").contains("refusing non-directory artifact scope"));
    assert!(!outside.join("created-outside").exists());
    assert_eq!(std::fs::read(&fixture.source).unwrap(), b"fixture audio");
    assert!(!fixture.transaction_path().exists());
    fixture.cleanup();
}

#[test]
fn hostile_and_over_cap_inventory_is_rejected_without_mutation() {
    let root = std::env::temp_dir().join(format!(
        "yututui-artifact-inventory-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let hostile = root.join("session-row-identity.json");
    std::fs::write(&hostile, b"hostile").unwrap();
    let error = inventory_transactions_in(&root, 8, 1024).unwrap_err();
    assert!(error.to_string().contains("invalid entry"));
    assert_eq!(std::fs::read(&hostile).unwrap(), b"hostile");
    std::fs::remove_file(&hostile).unwrap();

    let first = root.join(format!("{}.json", "a".repeat(64)));
    let second = root.join(format!("{}.json", "b".repeat(64)));
    std::fs::write(&first, b"a").unwrap();
    std::fs::write(&second, b"b").unwrap();
    let error = inventory_transactions_in(&root, 1, 1024).unwrap_err();
    assert!(error.to_string().contains("1 file cap"));
    let error = inventory_transactions_in(&root, 2, 1).unwrap_err();
    assert!(error.to_string().contains("1 byte scan cap"));
    assert_eq!(std::fs::read(&first).unwrap(), b"a");
    assert_eq!(std::fs::read(&second).unwrap(), b"b");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn validated_atomic_write_temp_is_cleaned_only_after_bounded_inventory() {
    let _registry = acquire_registry_lock().expect("own transaction registry");
    let dir = transaction_dir().unwrap();
    safe_fs::ensure_private_dir_durable(&dir).unwrap();
    let key = transaction_key(
        "sp2yt-artifact-stale-temp",
        "row-00001",
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed) as u32 + 1,
    );
    let stale = dir.join(format!(
        ".{key}.json.tmp.{}.{}",
        std::process::id(),
        "a".repeat(16)
    ));
    std::fs::write(&stale, b"partial atomic transaction").unwrap();

    let inventory = inventory_transactions_locked().expect("inventory with stale atomic temp");

    assert!(!stale.exists());
    assert!(!inventory.contains(&stale));
}

#[test]
fn concurrent_rows_serialize_the_shared_session_update_without_loss() {
    let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let session_id = format!(
        "sp2yt-artifact-concurrent-{}-{sequence}",
        std::process::id()
    );
    let root = std::env::temp_dir().join(format!(
        "yututui-artifact-concurrent-{}-{sequence}",
        std::process::id()
    ));
    let incoming = root.join("incoming");
    let complete = root.join("complete");
    std::fs::create_dir_all(&incoming).unwrap();
    std::fs::create_dir_all(&complete).unwrap();

    let mut rows = Vec::new();
    let mut requests = Vec::new();
    for order in 1..=2 {
        let row_id = format!("row-{order:05}");
        let claim = crate::transfer::artifact_identity::test_import_claim(
            &session_id,
            &row_id,
            order,
            &format!("video-{order}"),
        );
        let source = incoming.join(format!("Track {order}.m4a"));
        let destination = complete.join(format!("Track {order}.m4a"));
        std::fs::write(&source, format!("audio {order}")).unwrap();
        std::fs::write(
            crate::downloads::sidecar_path(&source),
            format!("sidecar {order}"),
        )
        .unwrap();
        rows.push(ImportSessionRow {
            row_id: row_id.clone(),
            source_order: order,
            status: ImportSessionRowStatus::Matched,
            title: format!("Track {order}"),
            source_key: format!("spotify:track:{order}"),
            selected_key: Some(format!("video-{order}")),
            download_claim: Some(claim.clone()),
            ..ImportSessionRow::default()
        });
        requests.push(ArtifactMoveRequest::import_download(
            session_id.clone(),
            row_id,
            order,
            claim,
            source,
            destination,
            complete.clone(),
        ));
    }
    ImportSession {
        session_id: session_id.clone(),
        session_instance_id: format!("test-instance-{session_id}"),
        rows,
        ..ImportSession::default()
    }
    .save()
    .unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(requests.len()));
    let workers: Vec<_> = requests
        .into_iter()
        .map(|request| {
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                commit(request)
            })
        })
        .collect();
    for worker in workers {
        worker
            .join()
            .expect("artifact commit worker panicked")
            .expect("concurrent artifact commit failed");
    }

    let saved = ImportSession::load(&session_id).unwrap();
    assert_eq!(saved.counts.written, 2);
    assert!(saved.rows.iter().all(|row| row.written));
    for order in 1..=2 {
        let destination = complete.join(format!("Track {order}.m4a"));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            format!("audio {order}").as_bytes()
        );
        assert_eq!(
            std::fs::read(crate::downloads::sidecar_path(&destination)).unwrap(),
            format!("sidecar {order}").as_bytes()
        );
    }

    let _ = ImportSession::delete_record(&session_id);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn transaction_locks_use_a_bounded_stripe_set_and_serialize_collisions() {
    let mut paths = std::collections::HashSet::new();
    for stripe in 0_u16..=255 {
        let key = format!("{stripe:02x}{}", "0".repeat(62));
        paths.insert(txn_lock_path(&key).unwrap());
    }
    assert_eq!(paths.len(), 256, "lock files must have a fixed upper bound");

    let first_key = format!("aa{}", "1".repeat(62));
    let second_key = format!("aa{}", "2".repeat(62));
    assert_ne!(first_key, second_key);
    assert_eq!(
        txn_lock_path(&first_key).unwrap(),
        txn_lock_path(&second_key).unwrap()
    );
    let first = acquire_txn_lock(&first_key).expect("acquire first stripe owner");
    let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _second = acquire_txn_lock(&second_key).expect("acquire colliding stripe owner");
        acquired_tx.send(()).unwrap();
    });
    assert!(
        acquired_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "a colliding key acquired the stripe concurrently"
    );
    drop(first);
    acquired_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("colliding key did not acquire the released stripe");
    worker.join().unwrap();
}
