use super::*;

#[test]
fn durable_store_removal_is_idempotent_and_reports_presence() {
    let dir = temp_dir("durable-store-removal");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");
    std::fs::write(&path, b"session").unwrap();

    assert!(remove_store_file(&path).unwrap());
    assert!(!path.exists());
    assert!(!remove_store_file(&path).unwrap());

    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn durable_store_removal_refuses_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("durable-store-removal-symlink");
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("foreign.json");
    let path = dir.join("session.json");
    std::fs::write(&target, b"foreign").unwrap();
    symlink(&target, &path).unwrap();

    assert_eq!(
        remove_store_file(&path).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&target).unwrap(), b"foreign");

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(dir);
}
