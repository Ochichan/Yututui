use super::*;

#[cfg(unix)]
#[test]
fn inaccessible_existing_organize_directory_is_rejected_without_chmod() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let mut fixture = MoveFixture::new("existing-private-dir", ArtifactMoveKind::Organize);
    let library_root = fixture.request.destination_root.clone();
    std::fs::set_permissions(&library_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let artist = library_root.join("Artist");
    std::fs::create_dir(&artist).unwrap();
    std::fs::set_permissions(&artist, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.destination = artist.join("Album/Track [abc123def45].m4a");
    fixture.request.audio_to = fixture.destination.clone();

    let error = commit(fixture.request.clone())
        .expect_err("inaccessible existing directory must fail publication");

    assert!(format!("{error:#}").contains("library publication requires at least 0755"));
    assert_eq!(std::fs::metadata(&artist).unwrap().mode() & 0o777, 0o700);
    assert!(fixture.source.exists());
    assert!(!fixture.transaction_path().exists());
    fixture.cleanup();
}

#[test]
fn legacy_transaction_schema_is_refused_without_mutating_artifacts() {
    let data_root = std::env::temp_dir().join(format!(
        "yututui-artifact-legacy-data-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let data_root_value = data_root.to_string_lossy().into_owned();

    // This test intentionally leaves a legacy transaction visible long enough to reject it.
    // Give that corrupt fixture its own registry so parallel session reconciliation cannot
    // observe it as an unrelated startup failure.
    crate::test_util::env::with_var("YTM_DATA_DIR", Some(&data_root_value), || {
        let fixture = MoveFixture::new("legacy-schema", ArtifactMoveKind::ImportDownload);
        // Keep startup reconciliation from observing the transaction as current-schema in the
        // interval between its creation and the deliberate legacy-schema rewrite below.
        let record_guard = ImportRecordGuard::try_acquire(&fixture.session_id)
            .expect("own legacy fixture session");
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
    });

    let _ = std::fs::remove_dir_all(data_root);
}

#[cfg(target_os = "linux")]
#[test]
fn organize_publishes_across_filesystems_with_destination_anonymous_staging() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let mut fixture = MoveFixture::new("cross-filesystem", ArtifactMoveKind::Organize);
    let shared_memory = Path::new("/dev/shm");
    let Ok(shared_memory_metadata) = std::fs::metadata(shared_memory) else {
        fixture.cleanup();
        return;
    };
    let source_metadata = std::fs::metadata(fixture.source.parent().unwrap()).unwrap();
    if source_metadata.dev() == shared_memory_metadata.dev() {
        fixture.cleanup();
        return;
    }

    let destination_root = shared_memory.join(format!(
        "yututui-cross-filesystem-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&destination_root).unwrap();
    std::fs::set_permissions(&destination_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let destination = destination_root.join(fixture.destination.file_name().unwrap());
    fixture.destination = destination.clone();
    fixture.request.audio_to = destination.clone();
    fixture.request.destination_root = destination_root.clone();

    commit(fixture.request.clone()).expect("cross-filesystem organize must publish");

    assert_eq!(std::fs::read(&destination).unwrap(), b"fixture audio");
    assert_eq!(
        std::fs::metadata(&destination).unwrap().mode() & 0o777,
        0o644
    );
    let sidecar = crate::downloads::sidecar_path(&destination);
    assert_eq!(std::fs::read(&sidecar).unwrap(), b"fixture sidecar");
    assert_eq!(std::fs::metadata(&sidecar).unwrap().mode() & 0o777, 0o600);
    assert!(!fixture.source.exists());
    assert!(!fixture.transaction_path().exists());

    let _ = std::fs::remove_dir_all(destination_root);
    fixture.cleanup();
}
