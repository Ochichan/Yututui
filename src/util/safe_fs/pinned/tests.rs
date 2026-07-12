use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::PinnedDir;
use crate::util::safe_fs::{
    clear_process_mutation_revoke_for_test, ensure_private_dir, revoke_process_mutations,
};

struct MutationRevokeReset;

impl MutationRevokeReset {
    fn clear() -> Self {
        clear_process_mutation_revoke_for_test();
        Self
    }
}

impl Drop for MutationRevokeReset {
    fn drop(&mut self) {
        clear_process_mutation_revoke_for_test();
    }
}

#[test]
fn creates_and_durably_syncs_owned_generation() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("owned-generation");
    let destination = root.join("destination");
    fs::create_dir_all(&destination).unwrap();

    let pinned = PinnedDir::open_existing(&root, PathBuf::from("destination").as_path())
        .expect("pin destination");
    let mut generation = pinned
        .create_new(OsStr::new("artifact.part"))
        .expect("create owned generation");
    assert_eq!(generation.basename(), OsStr::new("artifact.part"));
    assert_eq!(pinned.identity(), generation.parent.identity());
    generation
        .file_mut()
        .unwrap()
        .write_all(b"payload")
        .unwrap();
    generation.sync_durable().expect("sync generation");

    assert_eq!(
        fs::read(destination.join("artifact.part")).unwrap(),
        b"payload"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(windows)]
#[test]
fn pinned_directory_handle_can_flush_metadata() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("directory-flush");
    fs::create_dir_all(&root).unwrap();

    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();
    pinned
        .sync_directory()
        .expect("write-capable directory handle must authorize metadata flush");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn collision_preserves_existing_sentinel() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("collision");
    let destination = root.join("destination");
    fs::create_dir_all(&destination).unwrap();
    let sentinel = destination.join("artifact.part");
    fs::write(&sentinel, b"sentinel").unwrap();

    let pinned = PinnedDir::open_existing(&root, PathBuf::from("destination").as_path())
        .expect("pin destination");
    let error = pinned
        .create_new(OsStr::new("artifact.part"))
        .expect_err("collision must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&sentinel).unwrap(), b"sentinel");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn read_only_child_opens_without_write_or_delete_access() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("read-only-child");
    fs::create_dir_all(&root).unwrap();
    let source = root.join("source.audio");
    fs::write(&source, b"read-only payload").unwrap();
    let mut permissions = fs::metadata(&source).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&source, permissions).unwrap();

    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();
    let source = pinned
        .open_child_readonly(OsStr::new("source.audio"))
        .expect("read-only source handle");
    let mut file = source.file().unwrap().try_clone().unwrap();
    let mut payload = Vec::new();
    file.read_to_end(&mut payload).unwrap();
    assert_eq!(payload, b"read-only payload");

    #[cfg(windows)]
    {
        let mut permissions = fs::metadata(root.join("source.audio"))
            .unwrap()
            .permissions();
        // This branch only clears the Windows read-only file attribute so the test fixture can
        // be removed. The Unix world-writable hazard behind this lint is excluded by the cfg.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(root.join("source.audio"), permissions).unwrap();
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn private_generation_cleanup_removes_only_the_exact_owned_name() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("private-remove");
    ensure_private_dir(&root).unwrap();
    let pinned = PinnedDir::open_private_existing(&root, PathBuf::new().as_path()).unwrap();
    let mut generation = pinned.create_new(OsStr::new("owned.audio")).unwrap();
    generation.file_mut().unwrap().write_all(b"owned").unwrap();
    generation.sync_durable().unwrap();

    generation.remove_from_private_parent().unwrap();

    assert!(!root.join("owned.audio").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn private_named_stage_promotes_without_a_residual_stage() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("private-promote");
    ensure_private_dir(&root).unwrap();
    let pinned = PinnedDir::open_private_existing(&root, PathBuf::new().as_path()).unwrap();
    let mut stage = pinned.create_new(OsStr::new("owned.stage")).unwrap();
    stage.file_mut().unwrap().write_all(b"published").unwrap();
    stage.sync_durable().unwrap();

    let published = stage
        .promote_noreplace(&pinned, OsStr::new("final.audio"))
        .unwrap();

    assert_eq!(published.file().unwrap().metadata().unwrap().len(), 9);
    assert!(!root.join("owned.stage").exists());
    assert_eq!(fs::read(root.join("final.audio")).unwrap(), b"published");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn private_named_stage_collision_remains_owned_for_recovery() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("private-promote-collision");
    ensure_private_dir(&root).unwrap();
    fs::write(root.join("final.audio"), b"foreign").unwrap();
    let pinned = PinnedDir::open_private_existing(&root, PathBuf::new().as_path()).unwrap();
    let mut stage = pinned.create_new(OsStr::new("owned.stage")).unwrap();
    stage.file_mut().unwrap().write_all(b"published").unwrap();
    stage.sync_durable().unwrap();

    let error = stage
        .promote_noreplace(&pinned, OsStr::new("final.audio"))
        .expect_err("no-replace promotion must preserve both generations");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(root.join("owned.stage")).unwrap(), b"published");
    assert_eq!(fs::read(root.join("final.audio")).unwrap(), b"foreign");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unsupported_ephemeral_creation_never_falls_back_in_an_untrusted_directory() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("untrusted-ephemeral-unsupported");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();

    let error = pinned
        .create_ephemeral_with(OsStr::new("must-not-exist.stage"), |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "forced unsupported ephemeral primitive",
            ))
        })
        .expect_err("untrusted named fallback must stay disabled");

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(!root.join("must-not-exist.stage").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn untrusted_named_generation_cannot_be_promoted() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("untrusted-named-promote");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();
    let stage = pinned.create_new(OsStr::new("owned.stage")).unwrap();

    let error = stage
        .promote_noreplace(&pinned, OsStr::new("final.audio"))
        .expect_err("untrusted named promotion must fail closed");

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(root.join("owned.stage").is_file());
    assert!(!root.join("final.audio").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn private_named_promotion_rejects_a_stage_name_swap() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("private-promote-stage-swap");
    ensure_private_dir(&root).unwrap();
    let pinned = PinnedDir::open_private_existing(&root, PathBuf::new().as_path()).unwrap();
    let mut stage = pinned.create_new(OsStr::new("owned.stage")).unwrap();
    stage.file_mut().unwrap().write_all(b"owned").unwrap();
    stage.sync_durable().unwrap();
    fs::rename(root.join("owned.stage"), root.join("displaced-owned.stage")).unwrap();
    fs::write(root.join("owned.stage"), b"foreign").unwrap();

    let error = stage
        .promote_noreplace(&pinned, OsStr::new("final.audio"))
        .expect_err("a foreign stage name must not be promoted");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(root.join("owned.stage")).unwrap(), b"foreign");
    assert_eq!(
        fs::read(root.join("displaced-owned.stage")).unwrap(),
        b"owned"
    );
    assert!(!root.join("final.audio").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn private_generation_cleanup_preserves_a_replacement() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("private-remove-swap");
    ensure_private_dir(&root).unwrap();
    fs::write(root.join("owned.audio"), b"owned").unwrap();
    let pinned = PinnedDir::open_private_existing(&root, PathBuf::new().as_path()).unwrap();
    let generation = pinned
        .open_child_readonly(OsStr::new("owned.audio"))
        .unwrap();
    fs::rename(root.join("owned.audio"), root.join("moved-owned.audio")).unwrap();
    fs::write(root.join("owned.audio"), b"foreign").unwrap();

    let error = generation
        .remove_from_private_parent()
        .expect_err("foreign replacement must not be removed");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(root.join("owned.audio")).unwrap(), b"foreign");
    assert_eq!(fs::read(root.join("moved-owned.audio")).unwrap(), b"owned");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn ordinary_pinned_directory_cannot_claim_private_cleanup_authority() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("nonprivate-remove");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();
    let generation = pinned.create_new(OsStr::new("owned.audio")).unwrap();

    let error = generation
        .remove_from_private_parent()
        .expect_err("ordinary user directory must not authorize cleanup");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(root.join("owned.audio").is_file());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rejects_non_basename_children() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("bad-basename");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();

    for invalid in ["", ".", "..", "nested/child"] {
        let error = pinned
            .create_new(OsStr::new(invalid))
            .expect_err("non-basename must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn mutation_revocation_blocks_creation() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("revoked");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();

    revoke_process_mutations(Arc::from("test recovery failure"));
    let error = pinned
        .create_new(OsStr::new("must-not-exist"))
        .expect_err("revoked mutation must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(!root.join("must-not-exist").exists());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn pathname_swap_targets_pinned_original_or_fails_closed() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("path-swap");
    let original_path = root.join("destination");
    let moved_original = root.join("moved-original");
    fs::create_dir_all(&original_path).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::from("destination").as_path())
        .expect("pin destination");

    fs::rename(&original_path, &moved_original).expect("move pinned directory");
    fs::create_dir(&original_path).expect("install replacement directory");

    if let Ok(mut generation) = pinned.create_new(OsStr::new("owned.part")) {
        generation.file_mut().unwrap().write_all(b"owned").unwrap();
        generation.sync_durable().expect("sync in pinned directory");
        assert_eq!(
            fs::read(moved_original.join("owned.part")).unwrap(),
            b"owned"
        );
    }
    assert!(
        !original_path.join("owned.part").exists(),
        "replacement pathname must never receive the child"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn lock_child_respects_create_and_contention() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("lock-create");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();
    let missing = pinned
        .try_lock_child(OsStr::new("owner.lock"), None, false)
        .expect_err("create=false must preserve absence");
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);

    let (first, identity) = pinned
        .try_lock_child(OsStr::new("owner.lock"), None, true)
        .unwrap()
        .expect("first lock");
    assert!(
        pinned
            .try_lock_child(OsStr::new("owner.lock"), Some(identity), false)
            .unwrap()
            .is_none()
    );
    drop(first);
    assert!(
        pinned
            .try_lock_child(OsStr::new("owner.lock"), Some(identity), false)
            .unwrap()
            .is_some()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn lock_child_rejects_foreign_expected_identity() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("lock-foreign");
    fs::create_dir_all(&root).unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::new().as_path()).unwrap();
    let first = pinned.create_new(OsStr::new("first.lock")).unwrap();
    let foreign = pinned.create_new(OsStr::new("foreign.lock")).unwrap();
    let error = pinned
        .try_lock_child(OsStr::new("first.lock"), Some(foreign.identity()), false)
        .expect_err("foreign identity must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    drop(first);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn lock_child_parent_swap_stays_on_original_directory() {
    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("lock-parent-swap");
    let original = root.join("owner");
    let moved = root.join("moved");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("owner.lock"), b"original").unwrap();
    let pinned = PinnedDir::open_existing(&root, PathBuf::from("owner").as_path()).unwrap();
    let identity = pinned
        .open_child(OsStr::new("owner.lock"))
        .unwrap()
        .identity();
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(original.join("owner.lock"), b"replacement").unwrap();

    let (lock, _) = pinned
        .try_lock_child(OsStr::new("owner.lock"), Some(identity), false)
        .unwrap()
        .expect("pinned original lock");
    let moved_pin = PinnedDir::open_existing(&root, PathBuf::from("moved").as_path()).unwrap();
    assert!(
        moved_pin
            .try_lock_child(OsStr::new("owner.lock"), Some(identity), false)
            .unwrap()
            .is_none()
    );
    let replacement_pin =
        PinnedDir::open_existing(&root, PathBuf::from("owner").as_path()).unwrap();
    assert!(
        replacement_pin
            .try_lock_child(OsStr::new("owner.lock"), None, false)
            .unwrap()
            .is_some()
    );
    drop(lock);
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn rejects_symlink_directory_component() {
    use std::os::unix::fs::symlink;

    let _revoke_reset = MutationRevokeReset::clear();
    let root = temp_root("symlink-component");
    let outside = temp_root("symlink-outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, root.join("redirect")).unwrap();

    assert!(PinnedDir::open_existing(&root, PathBuf::from("redirect").as_path()).is_err());

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ytt-safe-fs-pinned-{label}-{}-{nonce}",
        std::process::id()
    ))
}
