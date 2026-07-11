use super::*;
use crate::app::local::{
    LocalImportFilesFingerprintCache, local_import_files_fingerprint_for_test,
    stable_import_cache_key_for_test,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, io};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestImportData {
    root: PathBuf,
    transfers: PathBuf,
    sessions: PathBuf,
}

impl TestImportData {
    fn new() -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-import-row-cache-{}-{sequence}",
            std::process::id()
        ));
        let transfers = root.join("transfers");
        let sessions = transfers.join("sessions");
        fs::create_dir_all(&sessions).expect("create isolated import data directories");
        Self {
            root,
            transfers,
            sessions,
        }
    }

    fn cache(&self) -> LocalImportFilesFingerprintCache {
        let mut cache = LocalImportFilesFingerprintCache::default();
        cache.set_data_dir_for_test(self.root.clone());
        cache
    }

    fn write_transfer(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.transfers.join(name);
        fs::write(&path, bytes).expect("write isolated transfer artifact");
        path
    }

    fn write_session(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.sessions.join(name);
        fs::write(&path, bytes).expect("write isolated session artifact");
        path
    }

    fn create_hard_link_set(&self, suffix: &str, count: usize) {
        let backing = self.write_transfer("hard-link-backing.scratch", b"x");
        self.create_hard_link_range(&backing, &self.transfers, "artifact", suffix, 0..count);
    }

    fn create_hard_link_range(
        &self,
        backing: &Path,
        directory: &Path,
        prefix: &str,
        suffix: &str,
        indices: std::ops::Range<usize>,
    ) {
        for index in indices {
            let path = directory.join(format!("{prefix}-{index:04}{suffix}"));
            if fs::hard_link(backing, &path).is_err() {
                fs::write(path, b"x").expect("write import artifact fallback");
            }
        }
    }
}

impl Drop for TestImportData {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct MirroredArtifact {
    fingerprint_path: PathBuf,
    projection_path: PathBuf,
}

impl MirroredArtifact {
    fn create(fingerprint_path: PathBuf, projection_path: PathBuf, bytes: &[u8]) -> Self {
        fs::create_dir_all(
            projection_path
                .parent()
                .expect("mirrored artifact has parent"),
        )
        .expect("create projection artifact directory");
        let artifact = Self {
            fingerprint_path,
            projection_path,
        };
        artifact.write(bytes);
        artifact
    }

    fn write(&self, bytes: &[u8]) {
        fs::write(&self.fingerprint_path, bytes).expect("write fingerprint artifact");
        fs::write(&self.projection_path, bytes).expect("write projection artifact");
    }

    fn remove(&self) {
        for path in [&self.fingerprint_path, &self.projection_path] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove mirrored artifact {}: {error}", path.display()),
            }
        }
    }
}

impl Drop for MirroredArtifact {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.projection_path);
    }
}

fn projection_transfers_dir() -> PathBuf {
    crate::paths::data_dir()
        .expect("test platform has app data directory")
        .join("transfers")
}

fn app_with_fingerprint_data(data: &TestImportData) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            summary: crate::local::LocalScanSummary::default(),
            index: crate::local::LocalIndex::default(),
            errors: Vec::new(),
        },
    }));
    app.local_mode
        .import_files_fingerprint_cache
        .borrow_mut()
        .set_data_dir_for_test(data.root.clone());
    app
}

#[test]
fn ime_scrub_requires_a_matching_successfully_rendered_import_projection() {
    let data = TestImportData::new();
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = "ime-fast-path".to_owned();

    assert!(!app.ime_scrub_local_projection_fresh());
    let _snapshot = app.local_rows_snapshot();
    assert!(
        !app.ime_scrub_local_projection_fresh(),
        "building a cache is not proof that it reached the terminal"
    );

    app.local_mode.ui.filter_query.push_str("-changed");
    app.mark_local_rows_rendered();
    app.local_mode.ui.filter_query = "ime-fast-path".to_owned();
    assert!(
        !app.ime_scrub_local_projection_fresh(),
        "a full draw may only mark the row cache matching the rendered view"
    );
    app.mark_local_rows_rendered();
    assert!(app.ime_scrub_local_projection_fresh());

    app.switch_local_section(LocalSection::Tracks);
    assert!(
        app.ime_scrub_local_projection_fresh(),
        "Local views that do not read import artifacts need no external fingerprint"
    );
}

#[test]
fn ime_scrub_rejects_changed_or_unreliable_import_fingerprints() {
    let data = TestImportData::new();
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = "ime-external-change".to_owned();
    let _snapshot = app.local_rows_snapshot();
    app.mark_local_rows_rendered();
    assert!(app.ime_scrub_local_projection_fresh());

    data.write_transfer("ime-external-change.report.json", b"{}");
    assert!(
        !app.ime_scrub_local_projection_fresh(),
        "recognized external writes must invalidate before the next scrub"
    );

    let _snapshot = app.local_rows_snapshot();
    app.mark_local_rows_rendered();
    assert!(app.ime_scrub_local_projection_fresh());
    app.local_mode
        .import_files_fingerprint_cache
        .borrow_mut()
        .fail_next_transfers_read_dir_for_test(io::ErrorKind::PermissionDenied);
    assert!(
        !app.ime_scrub_local_projection_fresh(),
        "an unreliable filesystem observation must fail closed"
    );
}

fn fingerprint(cache: &mut LocalImportFilesFingerprintCache) -> (u64, bool) {
    local_import_files_fingerprint_for_test(cache)
}

#[test]
fn import_projection_is_cached_only_for_a_stable_reliable_pre_post_fingerprint() {
    assert_eq!(
        stable_import_cache_key_for_test(Some((7, true)), Some((7, true))),
        Some(Some(7))
    );
    assert_eq!(
        stable_import_cache_key_for_test(Some((7, true)), Some((8, true))),
        None
    );
    assert_eq!(
        stable_import_cache_key_for_test(Some((7, false)), Some((7, true))),
        None
    );
    assert_eq!(
        stable_import_cache_key_for_test(Some((7, true)), Some((7, false))),
        None
    );
    assert_eq!(
        stable_import_cache_key_for_test(None, Some((7, true))),
        None
    );
    assert_eq!(
        stable_import_cache_key_for_test(Some((7, true)), None),
        None
    );
    assert_eq!(stable_import_cache_key_for_test(None, None), Some(None));
}

#[test]
fn fingerprint_implementation_forbids_content_read_apis() {
    let source = include_str!("../local/import_fingerprint.rs");
    for forbidden in [
        "std::fs::File",
        "OpenOptions",
        "std::io::Read",
        "std::fs::read(",
        "std::fs::read_to_string(",
        "BufReader",
        ".read_exact(",
        ".read_to_end(",
        ".read_to_string(",
    ] {
        assert!(
            !source.contains(forbidden),
            "metadata-only fingerprint must not use content API {forbidden}"
        );
    }
    assert!(
        !source.contains("directory_change_handle"),
        "Windows directory generations must reopen the current path instead of retaining a stale handle"
    );
    let safe_fs = include_str!("../../util/safe_fs.rs");
    assert!(
        safe_fs.contains("pub(crate) fn windows_directory_change_time(path: &Path)"),
        "Windows directory generation must use the transient current-path helper"
    );
}

fn import_session(
    session_id: &str,
    status: crate::transfer::session::ImportSessionRowStatus,
    title: &str,
) -> crate::transfer::session::ImportSession {
    crate::transfer::session::ImportSession {
        schema_version: 1,
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        updated_at: 10,
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-1".to_owned(),
            source_order: 1,
            status,
            title: title.to_owned(),
            artists: vec!["Artist".to_owned()],
            source_key: "source:row-1".to_owned(),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    }
}

fn session_bytes(session: &crate::transfer::session::ImportSession) -> Vec<u8> {
    serde_json::to_vec(session).expect("serialize import session fixture")
}

#[test]
fn cold_large_unrecognized_file_has_zero_content_or_artifact_io_on_warm_lookup() {
    let data = TestImportData::new();
    let path = data.transfers.join("cold-32mib.scratch");
    let file = fs::File::create(path).expect("create cold sparse artifact");
    file.set_len(32 * 1024 * 1024)
        .expect("size cold sparse artifact");
    drop(file);
    let mut cache = data.cache();

    let first = fingerprint(&mut cache);
    assert!(first.1);
    assert_eq!(cache.probe().read_dir_calls, 2);
    assert_eq!(cache.probe().artifact_metadata_stats, 0);

    cache.reset_probe();
    assert_eq!(fingerprint(&mut cache), first);
    let warm = cache.probe();
    assert_eq!(warm.directory_metadata_stats, 2);
    assert_eq!(warm.read_dir_calls, 0);
    assert_eq!(warm.directory_entries_visited, 0);
    assert_eq!(warm.artifact_metadata_stats, 0);
}

#[test]
fn over_legacy_cap_unrecognized_files_do_not_overflow_or_rescan() {
    let data = TestImportData::new();
    data.create_hard_link_set(".scratch", 4097);
    let mut cache = data.cache();
    let first = fingerprint(&mut cache);
    assert!(first.1);

    cache.reset_probe();
    assert_eq!(fingerprint(&mut cache), first);
    assert_eq!(fingerprint(&mut cache), first);
    let warm = cache.probe();
    assert_eq!(warm.read_dir_calls, 0);
    assert_eq!(warm.directory_entries_visited, 0);
    assert_eq!(warm.artifact_metadata_stats, 0);
}

#[test]
fn recognized_artifacts_are_metadata_statted_without_content_reads() {
    let data = TestImportData::new();
    data.write_transfer("checkpoint.json", b"checkpoint");
    data.write_transfer("report.report.json", b"report");
    data.write_transfer("journal.journal.jsonl", b"journal");
    data.write_session("UPPER_name.json", b"session");
    data.write_session("summary.summary.json", b"summary");
    data.write_transfer("ignored.scratch", b"ignored");
    let mut cache = data.cache();
    assert!(fingerprint(&mut cache).1);

    cache.reset_probe();
    assert!(fingerprint(&mut cache).1);
    let warm = cache.probe();
    assert_eq!(warm.read_dir_calls, 0);
    assert_eq!(warm.artifact_metadata_stats, 5);
}

#[test]
fn recognized_cap_overflow_and_shrink_preserve_the_bounded_cache_contract() {
    let data = TestImportData::new();
    data.create_hard_link_set(".json", 4096);
    let mut cache = data.cache();
    let at_cap = fingerprint(&mut cache);
    assert!(at_cap.1);

    cache.reset_probe();
    assert_eq!(fingerprint(&mut cache), at_cap);
    let at_cap_warm = cache.probe();
    assert_eq!(at_cap_warm.read_dir_calls, 0);
    assert_eq!(at_cap_warm.artifact_metadata_stats, 4096);

    let trailing = data.write_transfer("artifact-4096.json", b"x");
    let first_overflow = fingerprint(&mut cache);
    assert!(!first_overflow.1);

    cache.reset_probe();
    let unchanged_overflow = fingerprint(&mut cache);
    assert_eq!(unchanged_overflow.0, first_overflow.0);
    assert!(!unchanged_overflow.1);
    let overflow = cache.probe();
    assert_eq!(overflow.read_dir_calls, 2);
    assert_eq!(overflow.artifact_metadata_stats, 4097);

    fs::write(&trailing, b"changed trailing artifact")
        .expect("rewrite artifact beyond retained-path cap");
    cache.reset_probe();
    let changed_overflow = fingerprint(&mut cache);
    assert_ne!(changed_overflow.0, unchanged_overflow.0);
    assert!(!changed_overflow.1);
    assert_eq!(cache.probe().read_dir_calls, 2);
    assert_eq!(cache.probe().artifact_metadata_stats, 4097);

    fs::remove_file(&trailing).expect("shrink recognized artifacts back to cap");
    let recovered = fingerprint(&mut cache);
    assert!(recovered.1);
    assert_eq!(recovered.0, at_cap.0);
    cache.reset_probe();
    assert_eq!(fingerprint(&mut cache), recovered);
    assert_eq!(cache.probe().read_dir_calls, 0);
    assert_eq!(cache.probe().artifact_metadata_stats, 4096);

    data.write_transfer("artifact-4096.json", b"overflow again");
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = "no-overflow-row".to_owned();
    let first_snapshot = app.local_rows_snapshot();
    let second_snapshot = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(
        &first_snapshot.data,
        &second_snapshot.data
    ));
    assert!(app.local_mode.rows_cache.borrow().is_none());
}

#[test]
fn recognized_path_cap_is_global_across_transfers_and_sessions() {
    let data = TestImportData::new();
    data.create_hard_link_set(".json", 2048);
    let backing = data.transfers.join("hard-link-backing.scratch");
    data.create_hard_link_range(
        &backing,
        &data.sessions,
        "session-artifact",
        ".json",
        0..2048,
    );
    let mut cache = data.cache();
    let at_cap = fingerprint(&mut cache);
    assert!(at_cap.1);
    assert_eq!(cache.retained_path_count_for_test(), 4096);
    assert!(cache.retained_path_capacity_for_test() <= 4096);

    cache.reset_probe();
    assert_eq!(fingerprint(&mut cache), at_cap);
    assert_eq!(cache.probe().read_dir_calls, 0);
    assert_eq!(cache.probe().artifact_metadata_stats, 4096);

    data.create_hard_link_range(
        &backing,
        &data.sessions,
        "session-artifact",
        ".json",
        2048..2049,
    );
    assert!(!fingerprint(&mut cache).1);
    assert!(cache.retained_path_count_for_test() <= 4096);
    assert!(cache.retained_path_capacity_for_test() <= 4096);
    cache.reset_probe();
    assert!(!fingerprint(&mut cache).1);
    assert_eq!(cache.probe().read_dir_calls, 2);
    assert_eq!(cache.probe().artifact_metadata_stats, 4097);

    data.create_hard_link_range(&backing, &data.transfers, "artifact", ".json", 2048..3000);
    data.create_hard_link_range(
        &backing,
        &data.sessions,
        "session-artifact",
        ".json",
        2049..3000,
    );
    assert!(!fingerprint(&mut cache).1);
    assert!(cache.retained_path_count_for_test() <= 4096);
    assert!(cache.retained_path_capacity_for_test() <= 4096);
    cache.reset_probe();
    assert!(!fingerprint(&mut cache).1);
    assert_eq!(cache.probe().read_dir_calls, 2);
    assert_eq!(cache.probe().artifact_metadata_stats, 6000);

    for index in 2048..3000 {
        fs::remove_file(data.transfers.join(format!("artifact-{index:04}.json")))
            .expect("shrink transfer artifacts to the global cap split");
        fs::remove_file(
            data.sessions
                .join(format!("session-artifact-{index:04}.json")),
        )
        .expect("shrink session artifacts to the global cap split");
    }
    let recovered = fingerprint(&mut cache);
    assert!(recovered.1);
    assert_eq!(cache.retained_path_count_for_test(), 4096);
    assert!(cache.retained_path_capacity_for_test() <= 4096);
    cache.reset_probe();
    assert_eq!(fingerprint(&mut cache), recovered);
    assert_eq!(cache.probe().read_dir_calls, 0);
    assert_eq!(cache.probe().artifact_metadata_stats, 4096);
}

#[test]
fn unrelated_membership_churn_rescans_but_keeps_the_semantic_row_cache_key() {
    let data = TestImportData::new();
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = "unrelated-churn-missing".to_owned();
    let before = app.local_rows_snapshot();

    let scratch = data.write_transfer("external.scratch", b"ignored");
    app.local_mode
        .import_files_fingerprint_cache
        .borrow_mut()
        .reset_probe();
    let after_add = app.local_rows_snapshot();
    assert!(std::rc::Rc::ptr_eq(&before.data, &after_add.data));
    let add_probe = app
        .local_mode
        .import_files_fingerprint_cache
        .borrow()
        .probe();
    assert_eq!(add_probe.read_dir_calls, 1);
    assert_eq!(add_probe.artifact_metadata_stats, 0);

    fs::remove_file(scratch).expect("remove unrelated scratch artifact");
    let after_remove = app.local_rows_snapshot();
    assert!(std::rc::Rc::ptr_eq(&before.data, &after_remove.data));
}

#[test]
fn read_dir_error_is_volatile_and_recovery_redetects_membership() {
    let data = TestImportData::new();
    let session_id = format!("read-dir-recovery-{}", std::process::id());
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = session_id.clone();
    let before = app.local_rows_snapshot();
    assert!(before.rows().is_empty());

    let _artifact = MirroredArtifact::create(
        data.transfers.join(format!("{session_id}.report.json")),
        projection_transfers_dir().join(format!("{session_id}.report.json")),
        b"{}",
    );
    {
        let mut fingerprint_cache = app.local_mode.import_files_fingerprint_cache.borrow_mut();
        fingerprint_cache.reset_probe();
        fingerprint_cache.fail_next_transfers_read_dir_for_test(io::ErrorKind::PermissionDenied);
    }

    let during_error = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(&before.data, &during_error.data));
    assert_eq!(
        during_error.rows(),
        [crate::local::LocalRowId::ImportSession(session_id)].as_slice()
    );
    assert!(app.local_mode.rows_cache.borrow().is_none());
    let error_probe = app
        .local_mode
        .import_files_fingerprint_cache
        .borrow()
        .probe();
    assert_eq!(error_probe.read_dir_errors, 1);
    assert_eq!(error_probe.read_dir_calls, 2);

    let recovered = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(&during_error.data, &recovered.data));
    assert!(app.local_mode.rows_cache.borrow().is_some());
    let cached = app.local_rows_snapshot();
    assert!(std::rc::Rc::ptr_eq(&recovered.data, &cached.data));
}

#[test]
fn recognized_add_and_delete_invalidate_membership_at_the_next_lookup() {
    let data = TestImportData::new();
    let session_id = format!("metadata-membership-{}", std::process::id());
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = session_id.clone();
    let before = app.local_rows_snapshot();
    assert!(before.rows().is_empty());

    let artifact = MirroredArtifact::create(
        data.transfers.join(format!("{session_id}.report.json")),
        projection_transfers_dir().join(format!("{session_id}.report.json")),
        b"{}",
    );
    let after_add = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(&before.data, &after_add.data));
    assert_eq!(
        after_add.rows(),
        [crate::local::LocalRowId::ImportSession(session_id)].as_slice()
    );

    artifact.remove();
    let after_delete = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(&after_add.data, &after_delete.data));
    assert!(after_delete.rows().is_empty());
}

#[test]
fn restored_mtime_status_rewrite_invalidates_from_platform_change_identity() {
    let data = TestImportData::new();
    let session_id = format!("metadata-status-{}", std::process::id());
    let mut session = import_session(
        &session_id,
        crate::transfer::session::ImportSessionRowStatus::Ambiguous,
        "Review",
    );
    let initial = session_bytes(&session);
    let artifact = MirroredArtifact::create(
        data.sessions.join(format!("{session_id}.json")),
        projection_transfers_dir()
            .join("sessions")
            .join(format!("{session_id}.json")),
        &initial,
    );
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode
        .ui
        .drill
        .push(LocalDrill::ImportSession(session_id));
    let before = app.local_rows_snapshot();
    assert!(app.local_row_text_at(&before, 0).contains("review Review"));

    let original_modified = fs::metadata(&artifact.fingerprint_path)
        .and_then(|metadata| metadata.modified())
        .expect("read original fingerprint timestamp");
    session.rows[0].status = crate::transfer::session::ImportSessionRowStatus::NotFound;
    session.rows[0].title = "Absent".to_owned();
    #[cfg(not(any(unix, windows)))]
    session.rows[0]
        .warnings
        .push("normal metadata-length change".to_owned());
    let changed = session_bytes(&session);
    #[cfg(any(unix, windows))]
    assert_eq!(initial.len(), changed.len());
    artifact.write(&changed);
    #[cfg(any(unix, windows))]
    fs::File::options()
        .write(true)
        .open(&artifact.fingerprint_path)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(original_modified)))
        .expect("restore original fingerprint mtime");

    let after = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(&before.data, &after.data));
    assert!(app.local_row_text_at(&after, 0).contains("missing Absent"));
}

#[test]
fn nonsafe_utf8_json_candidate_is_tracked_and_invalidated() {
    let data = TestImportData::new();
    let session_id = format!("UPPER_name_{}", std::process::id());
    let mut session = import_session(
        &session_id,
        crate::transfer::session::ImportSessionRowStatus::Matched,
        "Unsafe Name",
    );
    let artifact = MirroredArtifact::create(
        data.sessions.join(format!("{session_id}.json")),
        projection_transfers_dir()
            .join("sessions")
            .join(format!("{session_id}.json")),
        &session_bytes(&session),
    );
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode.ui.filter_query = session_id.clone();
    let before = app.local_rows_snapshot();
    assert_eq!(
        before.rows(),
        [crate::local::LocalRowId::ImportSession(session_id)].as_slice()
    );

    session.source.label = Some("externally changed non-safe candidate".repeat(2));
    artifact.write(&session_bytes(&session));
    let after = app.local_rows_snapshot();
    assert!(!std::rc::Rc::ptr_eq(&before.data, &after.data));
    assert_eq!(before.rows(), after.rows());
}

#[test]
fn stable_import_snapshot_reuses_and_memoizes_its_action_hint() {
    let data = TestImportData::new();
    let session_id = format!("metadata-hint-{}", std::process::id());
    let session = import_session(
        &session_id,
        crate::transfer::session::ImportSessionRowStatus::Ambiguous,
        "Review",
    );
    let _artifact = MirroredArtifact::create(
        data.sessions.join(format!("{session_id}.json")),
        projection_transfers_dir()
            .join("sessions")
            .join(format!("{session_id}.json")),
        &session_bytes(&session),
    );
    let mut app = app_with_fingerprint_data(&data);
    app.switch_local_section(LocalSection::ImportSessions);
    app.local_mode
        .ui
        .drill
        .push(LocalDrill::ImportSession(session_id));
    let snapshot = app.local_rows_snapshot();
    let cached = app.local_rows_snapshot();
    assert!(std::rc::Rc::ptr_eq(&snapshot.data, &cached.data));

    app.local_mode
        .import_files_fingerprint_cache
        .borrow_mut()
        .reset_probe();
    let first_hint = app
        .local_import_action_hint_for_snapshot(&snapshot)
        .expect("ambiguous row has action hint");
    assert!(first_hint.contains("a accept"));
    let after_first = app
        .local_mode
        .import_files_fingerprint_cache
        .borrow()
        .probe();
    let second_hint = app
        .local_import_action_hint_for_snapshot(&snapshot)
        .expect("memoized ambiguous-row hint");
    assert_eq!(second_hint, first_hint);
    assert_eq!(
        app.local_mode
            .import_files_fingerprint_cache
            .borrow()
            .probe(),
        after_first,
        "memoized hint must not trigger a second filesystem projection lookup"
    );
}
