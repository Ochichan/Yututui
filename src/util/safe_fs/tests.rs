use std::cell::Cell;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtomicWriteFault {
    Create,
    Write,
    FileSync,
    Rename,
    ParentSync,
}

impl AtomicWriteFault {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Write => "write",
            Self::FileSync => "file-sync",
            Self::Rename => "rename",
            Self::ParentSync => "parent-sync",
        }
    }
}

struct FaultingAtomicWriteOps {
    stage: AtomicWriteFault,
    triggered: Cell<bool>,
}

impl FaultingAtomicWriteOps {
    fn new(stage: AtomicWriteFault) -> Self {
        Self {
            stage,
            triggered: Cell::new(false),
        }
    }

    fn error(&self, stage: AtomicWriteFault) -> io::Error {
        debug_assert_eq!(self.stage, stage);
        self.triggered.set(true);
        io::Error::new(
            io::ErrorKind::StorageFull,
            format!("fault injection: disk full during {}", stage.label()),
        )
    }
}

impl AtomicWriteOps for FaultingAtomicWriteOps {
    fn create(&self, path: &Path) -> io::Result<File> {
        if self.stage == AtomicWriteFault::Create {
            return Err(self.error(AtomicWriteFault::Create));
        }
        RealAtomicWriteOps.create(path)
    }

    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        if self.stage == AtomicWriteFault::Write {
            let partial_len = bytes.len().div_ceil(2);
            RealAtomicWriteOps.write_all(file, &bytes[..partial_len])?;
            return Err(self.error(AtomicWriteFault::Write));
        }
        RealAtomicWriteOps.write_all(file, bytes)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        if self.stage == AtomicWriteFault::FileSync {
            return Err(self.error(AtomicWriteFault::FileSync));
        }
        RealAtomicWriteOps.sync_file(file)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.stage == AtomicWriteFault::Rename {
            return Err(self.error(AtomicWriteFault::Rename));
        }
        RealAtomicWriteOps.rename(from, to)
    }

    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        if self.stage == AtomicWriteFault::ParentSync {
            return Err(self.error(AtomicWriteFault::ParentSync));
        }
        RealAtomicWriteOps.sync_parent(path)
    }
}

fn temp_root(name: &str) -> PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    std::env::temp_dir().join(format!("yututui-{name}-{}-{suffix}", std::process::id()))
}

#[test]
fn windows_change_time_requires_a_positive_generation() {
    assert_eq!(validate_windows_change_time(1).unwrap(), 1);
    assert_eq!(
        validate_windows_change_time(0).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        validate_windows_change_time(-1).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

#[cfg(windows)]
#[test]
fn windows_file_change_time_uses_a_transient_handle() {
    let dir = temp_root("file-change-time");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("artifact.json");
    fs::write(&path, b"{}").unwrap();

    assert!(windows_file_change_time(&path).unwrap() > 0);
    // The helper's local handle has already been dropped, so the artifact is not pinned.
    fs::remove_file(&path).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

fn atomic_temp_paths(path: &Path) -> Vec<PathBuf> {
    let parent = path.parent().expect("test target has a parent");
    let file_name = path
        .file_name()
        .expect("test target has a file name")
        .to_string_lossy();
    let prefix = format!(".{file_name}.tmp.");
    let mut paths = fs::read_dir(parent)
        .expect("test directory exists")
        .map(|entry| entry.expect("test directory entry").path())
        .filter(|entry| {
            entry
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn writes_and_reads_private_file() {
    let dir = temp_root("safe-fs");
    let path = dir.join("config.json");
    write_private_atomic(&path, br#"{"ok":true}"#).unwrap();
    assert_eq!(read_no_symlink(&path).unwrap(), br#"{"ok":true}"#);
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn advisory_lock_drop_unlocks_even_if_a_cloned_descriptor_outlives_the_guard() {
    let dir = temp_root("advisory-lock-cloned-fd");
    let path = dir.join("writer.lock");
    let guard = try_lock_private_file(&path)
        .expect("open lock file")
        .expect("acquire initial lock");
    let cloned_descriptor = guard._file.try_clone().expect("clone locked descriptor");

    assert!(
        try_lock_private_file(&path).unwrap().is_none(),
        "an independently opened descriptor must observe the live guard's lock",
    );
    drop(guard);

    let reacquired = try_lock_private_file(&path)
        .expect("reopen lock file")
        .expect("dropping the guard must explicitly unlock despite the surviving clone");

    drop(reacquired);
    drop(cloned_descriptor);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn durable_private_directory_creation_publishes_each_component() {
    let root = temp_root("durable-private-dirs");
    let nested = root.join("one").join("two").join("three");
    ensure_private_dir_durable(&nested).unwrap();
    assert!(nested.is_dir());
    ensure_private_dir_durable(&nested).unwrap();
    #[cfg(unix)]
    for path in [
        root.as_path(),
        &root.join("one"),
        &root.join("one/two"),
        &nested,
    ] {
        assert_eq!(
            fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn durable_private_directory_creation_does_not_repermission_shared_parent() {
    let shared = temp_root("durable-private-shared-parent");
    fs::create_dir_all(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    let nested = shared.join("private").join("child");

    ensure_private_dir_durable(&nested).unwrap();

    assert_eq!(
        fs::symlink_metadata(&shared).unwrap().permissions().mode() & 0o777,
        0o755
    );
    for path in [shared.join("private"), nested] {
        assert_eq!(
            fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let _ = fs::remove_dir_all(shared);
}

#[cfg(unix)]
#[test]
fn durable_private_directory_creation_rejects_requested_symlink() {
    use std::os::unix::fs::symlink;

    let root = temp_root("durable-private-dir-symlink");
    let target = root.join("target");
    fs::create_dir_all(&target).unwrap();
    let link = root.join("link");
    symlink(&target, &link).unwrap();
    let error = ensure_private_dir_durable(&link).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn durable_output_directory_syncs_each_new_component() {
    let shared = temp_root("durable-output-dirs");
    fs::create_dir_all(&shared).unwrap();
    let nested = shared.join("one").join("two");
    let mut synced = Vec::new();

    ensure_dir_durable_with(&nested, false, |component| {
        synced.push(component.to_path_buf());
        sync_parent_dir(component)
    })
    .unwrap();

    assert_eq!(synced, vec![shared.join("one"), nested]);
    let _ = fs::remove_dir_all(shared);
}

#[cfg(windows)]
#[test]
fn windows_atomic_write_replaces_existing_target() {
    let dir = temp_root("windows-atomic-replace");
    let path = dir.join("queue.jsonl");
    write_private_atomic(&path, b"old queue\n").expect("seed existing target");

    write_private_atomic(&path, b"compacted queue\n")
        .expect("MoveFileExW atomically replaces an existing target");

    assert_eq!(fs::read(&path).unwrap(), b"compacted queue\n");
    assert!(atomic_temp_paths(&path).is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn pre_rename_disk_failures_preserve_target_cleanup_temp_and_allow_latest_retry() {
    const OLD: &[u8] = b"known-good snapshot";
    const FAILED: &[u8] = b"partially attempted snapshot";
    const LATEST: &[u8] = b"newest valid snapshot";

    for stage in [
        AtomicWriteFault::Create,
        AtomicWriteFault::Write,
        AtomicWriteFault::FileSync,
        AtomicWriteFault::Rename,
    ] {
        let dir = temp_root(stage.label());
        let path = dir.join("state.json");
        write_private_atomic(&path, OLD).expect("seed target");
        let ops = FaultingAtomicWriteOps::new(stage);

        let error = write_private_atomic_with_ops(&path, FAILED, &ops)
            .expect_err("injected disk failure must surface");

        assert!(ops.triggered.get(), "{stage:?} failpoint was reached");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull, "{stage:?}");
        assert_eq!(
            error.to_string(),
            format!("fault injection: disk full during {}", stage.label())
        );
        assert_eq!(fs::read(&path).unwrap(), OLD, "{stage:?}");
        assert!(
            atomic_temp_paths(&path).is_empty(),
            "{stage:?} must not leave a partial temp file"
        );

        write_private_atomic(&path, LATEST).expect("retry latest snapshot");
        assert_eq!(fs::read(&path).unwrap(), LATEST, "{stage:?}");
        assert!(atomic_temp_paths(&path).is_empty(), "{stage:?}");
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn parent_sync_failure_surfaces_after_new_target_is_visible() {
    const OLD: &[u8] = b"known-good snapshot";
    const NEW: &[u8] = b"renamed but not directory-synced snapshot";

    let dir = temp_root("parent-sync");
    let path = dir.join("state.json");
    write_private_atomic(&path, OLD).expect("seed target");
    let ops = FaultingAtomicWriteOps::new(AtomicWriteFault::ParentSync);

    let error = write_private_atomic_with_ops(&path, NEW, &ops)
        .expect_err("parent sync failure must surface");

    assert!(ops.triggered.get(), "parent-sync failpoint was reached");
    assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    assert_eq!(
        error.to_string(),
        "fault injection: disk full during parent-sync"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        NEW,
        "rename committed before the failed parent sync; only durability is uncertain"
    );
    assert!(atomic_temp_paths(&path).is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn durable_append_writes_jsonl_line() {
    let dir = temp_root("append-durable");
    let path = dir.join("queue.jsonl");
    append_private_jsonl_durable(&path, r#"{"ok":true}"#).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"ok\":true}\n");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn append_separates_next_record_from_crash_torn_tail() {
    let dir = temp_root("append-torn-tail");
    let path = dir.join("queue.jsonl");
    write_private_atomic(&path, br#"{"interrupted":"record"#).unwrap();

    append_private_jsonl(&path, r#"{"next":true}"#).unwrap();

    let contents = fs::read_to_string(&path).unwrap();
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines, [r#"{"interrupted":"record"#, r#"{"next":true}"#]);
    assert!(serde_json::from_str::<serde_json::Value>(lines[0]).is_err());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines[1]).unwrap(),
        serde_json::json!({"next": true})
    );
    assert!(contents.ends_with('\n'));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn durable_append_separates_valid_tail_without_terminal_newline() {
    let dir = temp_root("append-valid-tail");
    let path = dir.join("queue.jsonl");
    write_private_atomic(&path, br#"{"sequence":1}"#).unwrap();
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    append_private_jsonl_durable(&path, r#"{"sequence":2}"#).unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\"sequence\":1}\n{\"sequence\":2}\n"
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        private_file_mode(),
        "append must continue repairing private-file permissions"
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn private_modes_are_enforced() {
    let dir = temp_root("modes");
    ensure_private_dir(&dir).unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        private_dir_mode()
    );
    let path = dir.join("secret.json");
    write_private_atomic(&path, b"{}").unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        private_file_mode()
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn symlink_reads_and_appends_are_rejected() {
    use std::os::unix::fs::symlink;

    let dir = temp_root("symlink");
    fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real");
    let link = dir.join("link");
    fs::write(&real, b"secret").unwrap();
    symlink(&real, &link).unwrap();
    assert!(read_no_symlink(&link).is_err());
    assert!(append_private_jsonl(&link, r#"{"overwrite":true}"#).is_err());
    assert_eq!(fs::read(&real).unwrap(), b"secret");
    let _ = fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn reparse_point_reads_and_appends_are_rejected() {
    use std::os::windows::fs::symlink_file;

    let dir = temp_root("reparse");
    fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real");
    let link = dir.join("link");
    fs::write(&real, b"secret").unwrap();
    if symlink_file(&real, &link).is_err() {
        let _ = fs::remove_dir_all(dir);
        return;
    }
    assert!(read_no_symlink(&link).is_err());
    assert!(append_private_jsonl(&link, r#"{"overwrite":true}"#).is_err());
    assert_eq!(fs::read(&real).unwrap(), b"secret");
    let _ = fs::remove_dir_all(dir);
}

#[cfg(windows)]
#[test]
fn windows_recorder_sync_uses_a_write_capable_non_reparse_handle() {
    use std::os::windows::fs::symlink_file;

    let dir = temp_root("windows-recorder-source-sync");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.mkv");
    fs::write(&source, b"durable recorder source").unwrap();

    sync_regular_file_durable(&source)
        .expect("FlushFileBuffers succeeds only because the sync handle has GENERIC_WRITE");
    sync_regular_file_durable(&source).expect("the deterministic marker is safely replaceable");
    assert_eq!(
        fs::read(dir.join(SOURCE_DURABILITY_MARKER)).unwrap(),
        b"v1\n"
    );
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SOURCE_DURABILITY_MARKER)
            })
            .count(),
        1,
        "repeated source syncs retain one bounded write-through marker"
    );

    let link = dir.join("source-link.mkv");
    if symlink_file(&source, &link).is_ok() {
        assert!(sync_regular_file_durable(&link).is_err());
    }
    assert_eq!(fs::read(source).unwrap(), b"durable recorder source");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn limited_load_reads_within_cap_and_sets_oversized_aside() {
    use serde::{Deserialize, Serialize};
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct S {
        n: u32,
    }
    let dir = temp_root("limited");
    let path = dir.join("s.json");

    // Under the cap → loads normally.
    write_private_atomic(&path, br#"{"n":7}"#).unwrap();
    assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S { n: 7 });

    // Over the cap → default, and the offending file is moved to `*.too-large.bak`
    // (never destroyed, so nothing is silently lost).
    let big = format!(r#"{{"n":7,"pad":"{}"}}"#, "x".repeat(2048));
    write_private_atomic(&path, big.as_bytes()).unwrap();
    assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S::default());
    assert!(!path.exists());
    assert!(path.with_extension("too-large.bak").exists());

    // Invalid UTF-8 under the cap is still present-but-unreadable and must be preserved.
    write_private_atomic(&path, &[0xff, 0xfe, 0xfd]).unwrap();
    assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S::default());
    assert!(!path.exists());
    assert!(path.with_extension("unreadable.bak").exists());

    // Missing file → default (no panic, no backup).
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(load_json_or_default_limited::<S>(&path, 1024), S::default());
}

#[test]
fn present_but_unreadable_file_is_backed_up_not_clobbered() {
    use serde::{Deserialize, Serialize};
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct S {
        n: u32,
    }
    let dir = temp_root("unreadable");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    // Invalid UTF-8 is present-but-unreadable: preserve it, don't silently default+clobber.
    fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
    assert_eq!(load_json_or_default::<S>(&path), S::default());
    assert!(!path.exists(), "the unreadable file is moved aside");
    assert!(
        path.with_extension("unreadable.bak").exists(),
        "present-but-unreadable file must be preserved as a backup",
    );
    // A genuinely missing file leaves no backup.
    let missing = dir.join("missing.json");
    assert_eq!(load_json_or_default::<S>(&missing), S::default());
    assert!(!missing.with_extension("unreadable.bak").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn copy_backup_preserves_original_when_linking_is_unavailable() {
    let dir = temp_root("backup-copy");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    let bak = path.with_extension("corrupt.bak");
    fs::write(&path, b"copy-me").unwrap();

    backup_by_copy(&path, &bak).unwrap();

    assert!(!path.exists());
    assert_eq!(fs::read(&bak).unwrap(), b"copy-me");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn failed_recovery_blocks_default_overwrite_until_original_is_removed() {
    let dir = temp_root("backup-guard");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    fs::write(&path, b"original").unwrap();

    mark_do_not_clobber(&path);
    let err = write_private_atomic(&path, b"replacement").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&path).unwrap(), b"original");

    fs::remove_file(&path).unwrap();
    write_private_atomic(&path, b"replacement").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn backup_aside_never_overwrites_existing_backup() {
    let dir = temp_root("backup-numbered");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.json");
    let first_backup = path.with_extension("corrupt.bak");
    let numbered_backup = path.with_extension("corrupt.1.bak");
    fs::write(&path, b"new-bad").unwrap();
    fs::write(&first_backup, b"old-bad").unwrap();

    let backed_up = backup_aside(&path).unwrap();

    assert_eq!(backed_up, numbered_backup);
    assert_eq!(fs::read(&first_backup).unwrap(), b"old-bad");
    assert_eq!(fs::read(&backed_up).unwrap(), b"new-bad");
    assert!(!path.exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn secret_backup_retention_keeps_newest_and_prunes_old_recovery_files() {
    let dir = temp_root("secret-backup-retention");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let mut newest = PathBuf::new();

    for idx in 0..(SECRET_BACKUP_RETENTION + 2) {
        fs::write(&path, format!("secret-{idx}")).unwrap();
        newest = backup_aside_secret(&path).unwrap();
    }

    let backups = recovery_backups(&path).unwrap();
    assert_eq!(backups.len(), SECRET_BACKUP_RETENTION);
    assert!(backups.iter().any(|backup| backup.path == newest));
    assert_eq!(
        fs::read_to_string(&newest).unwrap(),
        format!("secret-{}", SECRET_BACKUP_RETENTION + 1)
    );
    assert!(!path.exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn explicit_secret_backup_cleanup_prunes_to_retention() {
    let dir = temp_root("secret-backup-cleanup");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    for idx in 0..(SECRET_BACKUP_RETENTION + 1) {
        fs::write(backup_path(&path, "corrupt", idx), b"old").unwrap();
    }
    fs::write(&path, b"current").unwrap();

    let removed = enforce_secret_backup_retention(&path).unwrap();

    assert_eq!(removed, 1);
    assert_eq!(
        recovery_backups(&path).unwrap().len(),
        SECRET_BACKUP_RETENTION
    );
    assert_eq!(fs::read(&path).unwrap(), b"current");
    let _ = fs::remove_dir_all(dir);
}

mod recover {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        #[default]
        Balanced,
        Aggressive,
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Inner {
        mode: Mode,
        weight: f32,
        label: String,
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Row {
        id: String,
        kind: Mode,
    }

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    struct Store {
        volume: u8,
        name: String,
        inner: Inner,
        log: Vec<Row>,
    }

    #[test]
    fn valid_input_is_returned_unchanged() {
        let store = Store {
            volume: 42,
            name: "hi".into(),
            inner: Inner {
                mode: Mode::Aggressive,
                weight: 0.7,
                label: "x".into(),
            },
            log: vec![Row {
                id: "a".into(),
                kind: Mode::Balanced,
            }],
        };
        let value = serde_json::to_value(&store).unwrap();
        assert_eq!(recover_lenient::<Store>(value), store);
    }

    #[test]
    fn unknown_nested_enum_defaults_only_that_field() {
        // `inner.mode` is a variant this build no longer knows; everything else survives.
        let value = json!({
            "volume": 55,
            "name": "keep me",
            "inner": { "mode": "wild", "weight": 0.9, "label": "still here" },
            "log": [],
        });
        let recovered = recover_lenient::<Store>(value);
        assert_eq!(recovered.volume, 55, "sibling scalar preserved");
        assert_eq!(recovered.name, "keep me");
        assert_eq!(recovered.inner.mode, Mode::Balanced, "bad enum -> default");
        assert_eq!(
            recovered.inner.weight, 0.9,
            "sibling in same object preserved"
        );
        assert_eq!(recovered.inner.label, "still here");
    }

    #[test]
    fn retyped_scalar_defaults_only_that_field() {
        let value = json!({ "volume": "loud", "name": "kept" });
        let recovered = recover_lenient::<Store>(value);
        assert_eq!(recovered.volume, 0, "string-where-u8-expected -> default");
        assert_eq!(recovered.name, "kept");
    }

    #[test]
    fn bad_array_element_is_dropped_not_the_whole_log() {
        let value = json!({
            "log": [
                { "id": "a", "kind": "balanced" },
                { "id": "b", "kind": "wild" },        // drifted enum
                { "id": "c", "kind": "aggressive" },
            ],
        });
        let recovered = recover_lenient::<Store>(value);
        assert_eq!(
            recovered.log,
            vec![
                Row {
                    id: "a".into(),
                    kind: Mode::Balanced
                },
                Row {
                    id: "c".into(),
                    kind: Mode::Aggressive
                },
            ],
            "only the drifted record is dropped",
        );
    }

    #[test]
    fn overwhelmingly_incompatible_array_drops_to_default() {
        // 300 rows whose enum variant this build no longer knows. Element-wise recovery
        // would try each one; the early-abort keeps the default once the keep-ratio
        // collapses, bounding startup CPU on a crafted under-size-cap file.
        let rows: Vec<Value> = (0..300)
            .map(|i| json!({ "id": format!("r{i}"), "kind": "wild" }))
            .collect();
        let recovered = recover_lenient::<Store>(json!({ "log": rows, "volume": 9 }));
        assert!(
            recovered.log.is_empty(),
            "all-drifted array -> default (empty)"
        );
        assert_eq!(recovered.volume, 9, "sibling field is still preserved");
    }

    #[test]
    fn unrelated_json_falls_back_to_default() {
        assert_eq!(
            recover_lenient::<Store>(json!({ "nope": 1 })),
            Store::default()
        );
        assert_eq!(recover_lenient::<Store>(json!([1, 2, 3])), Store::default());
    }

    #[test]
    fn corrupt_file_is_backed_up_and_defaulted() {
        let dir = temp_root("recover-corrupt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.json");
        fs::write(&path, b"this is not { json").unwrap();

        let recovered = load_json_or_default::<Store>(&path);
        assert_eq!(recovered, Store::default());
        assert!(
            dir.join("store.corrupt.bak").exists(),
            "unparseable file must be preserved as a backup, not destroyed",
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_default_without_backup() {
        let dir = temp_root("recover-missing");
        let path = dir.join("store.json");
        assert_eq!(load_json_or_default::<Store>(&path), Store::default());
        assert!(!dir.join("store.corrupt.bak").exists());
    }
}
