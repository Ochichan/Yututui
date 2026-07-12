use super::UpdateLock;

#[test]
fn update_lock_is_exclusive() {
    let dir = std::env::temp_dir().join(format!("ytt-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = UpdateLock::try_acquire(&dir);
    assert!(first.is_some());
    assert!(
        UpdateLock::try_acquire(&dir).is_none(),
        "second acquire fails while the first is held"
    );
    drop(first);
    assert!(UpdateLock::try_acquire(&dir).is_some(), "released on drop");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dropping_update_lock_unlocks_a_duplicated_file_description() {
    let dir = std::env::temp_dir().join(format!(
        "ytt-lock-duplicated-description-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let guard = UpdateLock::try_acquire(&dir).expect("hold update lock");
    let duplicated = guard
        ._file
        .try_clone()
        .expect("duplicate the locked file description");

    assert!(
        UpdateLock::try_acquire(&dir).is_none(),
        "an independent opener must observe the live update lock"
    );
    drop(guard);
    let reacquired = UpdateLock::try_acquire(&dir)
        .expect("guard drop must explicitly unlock despite the surviving duplicate");

    drop(reacquired);
    drop(duplicated);
    let _ = std::fs::remove_dir_all(&dir);
}
