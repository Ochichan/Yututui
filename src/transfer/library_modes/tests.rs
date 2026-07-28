use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    PublishAudience, ensure_scoped_directory, publish_modes, reject_symlink_or_non_directory,
    validate_publish_mode,
};

fn temp_root(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ytt-library-modes-{label}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

#[test]
#[cfg(unix)]
fn a_private_root_keeps_a_shared_library_publish_private() {
    let root = temp_root("shared-from-private-root");
    set_mode(&root, 0o700);

    let modes = publish_modes(PublishAudience::SharedLibrary, &root).unwrap();

    assert_eq!(modes.file, Some(0o600));
    assert_eq!(modes.directory, Some(0o700));
    assert_eq!(modes.sidecar, Some(0o600));
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn a_group_readable_root_publishes_group_readable_artifacts() {
    let root = temp_root("shared-from-group-root");
    set_mode(&root, 0o750);

    let modes = publish_modes(PublishAudience::SharedLibrary, &root).unwrap();

    // A server daemon under its own account reads through the group bit, so the artifact has to
    // carry it too. The sidecar deliberately does not.
    assert_eq!(modes.file, Some(0o640));
    assert_eq!(modes.directory, Some(0o750));
    assert_eq!(modes.sidecar, Some(0o600));
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn a_world_readable_root_publishes_world_readable_artifacts() {
    let root = temp_root("shared-from-world-root");
    set_mode(&root, 0o755);

    let modes = publish_modes(PublishAudience::SharedLibrary, &root).unwrap();

    assert_eq!(modes.file, Some(0o644));
    assert_eq!(modes.directory, Some(0o755));
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn a_private_audience_never_widens_with_the_root() {
    let root = temp_root("private-from-world-root");
    set_mode(&root, 0o755);

    let modes = publish_modes(PublishAudience::Private, &root).unwrap();

    assert_eq!(modes.file, Some(0o600));
    assert_eq!(modes.sidecar, Some(0o600));
    assert_eq!(modes.directory, None);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_inherited_audience_requests_no_mode_at_all() {
    let root = temp_root("inherited");

    let modes = publish_modes(PublishAudience::Inherited, &root).unwrap();

    assert_eq!(modes.file, None);
    assert_eq!(modes.sidecar, None);
    assert_eq!(modes.directory, None);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn only_owner_readable_publication_modes_are_accepted() {
    validate_publish_mode(None).unwrap();
    validate_publish_mode(Some(0o600)).unwrap();
    validate_publish_mode(Some(0o640)).unwrap();
    validate_publish_mode(Some(0o644)).unwrap();
    validate_publish_mode(Some(0o666)).unwrap_err();
    validate_publish_mode(Some(0o777)).unwrap_err();
}

#[test]
fn scoped_directories_are_created_beneath_the_root() {
    let root = temp_root("scoped-create");

    let created = ensure_scoped_directory(&root, Path::new("YuTuTui/Artist/Album"), None).unwrap();

    assert_eq!(created, root.join("YuTuTui").join("Artist").join("Album"));
    assert!(created.is_dir());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn scoped_directory_creation_is_idempotent() {
    let root = temp_root("scoped-idempotent");
    let relative = Path::new("YuTuTui/Artist/Album");

    let first = ensure_scoped_directory(&root, relative, None).unwrap();
    let second = ensure_scoped_directory(&root, relative, None).unwrap();

    assert_eq!(first, second);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn scoped_directories_reject_a_parent_traversal() {
    let root = temp_root("scoped-traversal");

    let error =
        ensure_scoped_directory(&root, Path::new("YuTuTui/../../escaped"), None).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!root.parent().unwrap().join("escaped").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn scoped_directories_refuse_to_follow_a_planted_symlink() {
    let root = temp_root("scoped-symlink");
    let outside = temp_root("scoped-symlink-outside");
    std::os::unix::fs::symlink(&outside, root.join("YuTuTui")).unwrap();

    // Without the per-component check this would create `Artist` inside `outside`, escaping the
    // root the user chose before any later canonical comparison could object.
    let error = ensure_scoped_directory(&root, Path::new("YuTuTui/Artist"), None).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!outside.join("Artist").exists());
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[test]
#[cfg(unix)]
fn scoped_directory_creation_applies_the_requested_mode() {
    let root = temp_root("scoped-mode");
    set_mode(&root, 0o755);

    let created = ensure_scoped_directory(&root, Path::new("YuTuTui/Artist"), Some(0o755)).unwrap();

    assert_eq!(mode_of(&created) & 0o755, 0o755);
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn an_existing_directory_too_closed_for_publication_is_refused() {
    let root = temp_root("scoped-mode-too-closed");
    set_mode(&root, 0o755);
    fs::create_dir(root.join("YuTuTui")).unwrap();
    set_mode(&root.join("YuTuTui"), 0o700);

    // The server daemon could not traverse this, so publishing into it would silently produce a
    // library the server cannot read.
    let error =
        ensure_scoped_directory(&root, Path::new("YuTuTui/Artist"), Some(0o755)).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let _ = fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn a_symlinked_scope_is_rejected_outright() {
    let root = temp_root("reject-symlink");
    let outside = temp_root("reject-symlink-outside");
    let link = root.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();

    let error = reject_symlink_or_non_directory(&link).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn a_regular_file_is_not_a_directory_scope() {
    let root = temp_root("reject-file");
    fs::write(root.join("file"), b"x").unwrap();

    let error = reject_symlink_or_non_directory(&root.join("file")).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let _ = fs::remove_dir_all(root);
}
