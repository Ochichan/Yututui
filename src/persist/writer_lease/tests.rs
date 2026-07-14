use super::*;
use std::cell::{Cell, RefCell};

fn test_dir(label: &str) -> PathBuf {
    let mut suffix = [0_u8; 8];
    getrandom::fill(&mut suffix).unwrap();
    let suffix = suffix
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-writer-lease-{}-{label}-{suffix}",
        std::process::id()
    ))
}

fn assert_read_only(state: &InitializedWriterState) {
    assert!(matches!(state.access(), PersistenceAccess::ReadOnly { .. }));
    assert_eq!(
        state.ensure_writable().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

fn empty_writable_state() -> InitializedWriterState {
    InitializedWriterState::Writable(PersistenceWriterLease { _locks: Vec::new() })
}

#[test]
fn uninitialized_writer_state_is_default_deny() {
    const REASON: &str = "the process did not initialize its persistence writer lease";
    let state = ProcessWriterState::Uninitialized;

    assert_eq!(
        state.access(),
        PersistenceAccess::ReadOnly {
            reason: Arc::from(REASON)
        }
    );
    let error = state.ensure_writable().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(
        error.to_string(),
        format!("persistence is read-only: {REASON}")
    );
}

#[test]
fn writer_state_transitions_are_terminal_and_configured_once() {
    let acquisitions = Cell::new(0_u8);
    let configurations = RefCell::new(Vec::new());
    let mut state = ProcessWriterState::Uninitialized;

    assert_eq!(
        initialize_writer_state(
            &mut state,
            std::iter::empty::<PathBuf>(),
            false,
            |_, allow_read_only| {
                assert!(!allow_read_only);
                acquisitions.set(acquisitions.get() + 1);
                Ok(empty_writable_state())
            },
            |access| {
                configurations.borrow_mut().push(access.is_ok());
                Ok(())
            },
        )
        .unwrap(),
        PersistenceAccess::Writable
    );
    assert!(matches!(state, ProcessWriterState::Writable(_)));
    assert_eq!(acquisitions.get(), 1);
    assert_eq!(&*configurations.borrow(), &[true]);

    assert_eq!(
        initialize_writer_state(
            &mut state,
            std::iter::empty::<PathBuf>(),
            false,
            |_, _| panic!("an initialized writer must not reacquire its lease"),
            |_| panic!("an initialized writer must not reconfigure mutations"),
        )
        .unwrap(),
        PersistenceAccess::Writable
    );
    assert_eq!(
        initialize_reader_state(&mut state, Arc::from("reader"), |_| {
            panic!("a writer-to-reader request must not reconfigure mutations")
        })
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(matches!(state, ProcessWriterState::Writable(_)));
}

#[test]
fn read_only_state_cannot_upgrade_and_strict_failure_stays_uninitialized() {
    let reason: Arc<str> = Arc::from("lease contention");
    let mut state = ProcessWriterState::Uninitialized;
    let configured_reason = RefCell::new(None::<Arc<str>>);
    assert!(matches!(
        initialize_writer_state(
            &mut state,
            std::iter::empty::<PathBuf>(),
            true,
            |_, allow_read_only| {
                assert!(allow_read_only);
                Ok(InitializedWriterState::ReadOnly(Arc::clone(&reason)))
            },
            |access| {
                *configured_reason.borrow_mut() = access.err();
                Ok(())
            },
        )
        .unwrap(),
        PersistenceAccess::ReadOnly { .. }
    ));
    assert_eq!(
        configured_reason.borrow().as_deref(),
        Some("lease contention")
    );

    assert!(matches!(
        initialize_writer_state(
            &mut state,
            std::iter::empty::<PathBuf>(),
            true,
            |_, _| panic!("a read-only process must not attempt an upgrade"),
            |_| panic!("a read-only process must not reconfigure mutations"),
        )
        .unwrap(),
        PersistenceAccess::ReadOnly { .. }
    ));
    assert_eq!(
        initialize_writer_state(
            &mut state,
            std::iter::empty::<PathBuf>(),
            false,
            |_, _| panic!("a strict request cannot upgrade a read-only process"),
            |_| panic!("a strict request cannot reconfigure a read-only process"),
        )
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::WouldBlock
    );

    let mut failed = ProcessWriterState::Uninitialized;
    let error = initialize_writer_state(
        &mut failed,
        std::iter::empty::<PathBuf>(),
        false,
        |_, _| Err(std::io::Error::other("acquisition failed")),
        |_| panic!("failed acquisition must not configure mutations"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "acquisition failed");
    assert!(matches!(failed, ProcessWriterState::Uninitialized));

    let mut reader = ProcessWriterState::Uninitialized;
    let reader_reason: Arc<str> = Arc::from("explicit reader");
    assert!(matches!(
        initialize_reader_state(&mut reader, Arc::clone(&reader_reason), |access| {
            assert_eq!(access.err().as_deref(), Some("explicit reader"));
            Ok(())
        })
        .unwrap(),
        PersistenceAccess::ReadOnly { .. }
    ));
    assert!(matches!(reader, ProcessWriterState::ReadOnly(_)));
    assert!(matches!(
        initialize_reader_state(&mut reader, Arc::from("ignored"), |_| {
            panic!("an initialized reader must not reconfigure mutations")
        })
        .unwrap(),
        PersistenceAccess::ReadOnly { .. }
    ));
}

#[test]
fn failed_state_publication_drops_the_acquired_lease() {
    let root = test_dir("state-publication-rollback");
    let mut state = ProcessWriterState::Uninitialized;
    let error =
        initialize_writer_state(&mut state, [&root], false, acquire_state_for_roots, |_| {
            Err(std::io::Error::other("mutation gate rejected state"))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "mutation gate rejected state");
    assert!(matches!(state, ProcessWriterState::Uninitialized));

    let successor = acquire_state_for_roots([&root], false).unwrap();
    successor.ensure_writable().unwrap();
    drop(successor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn rejected_read_only_configurations_leave_process_uninitialized() {
    let mut fallback = ProcessWriterState::Uninitialized;
    let error = initialize_writer_state(
        &mut fallback,
        std::iter::empty::<PathBuf>(),
        true,
        |_, _| {
            Ok(InitializedWriterState::ReadOnly(Arc::from(
                "injected lease contention",
            )))
        },
        |_| Err(std::io::Error::other("mutation gate rejected fallback")),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "mutation gate rejected fallback");
    assert!(matches!(fallback, ProcessWriterState::Uninitialized));

    let mut reader = ProcessWriterState::Uninitialized;
    let error = initialize_reader_state(&mut reader, Arc::from("explicit reader"), |_| {
        Err(std::io::Error::other("mutation gate rejected reader"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "mutation gate rejected reader");
    assert!(matches!(reader, ProcessWriterState::Uninitialized));
}

#[test]
fn configured_roots_include_the_local_index_storage_domain() {
    let roots = configured_roots();
    let local_index_root = crate::paths::local_index_data_dir().unwrap();

    assert!(roots.contains(&local_index_root));
    assert_eq!(
        roots.len(),
        4,
        "all four configured storage domains remain leased"
    );
}

#[test]
fn shared_root_is_deduplicated_and_only_one_writer_wins() {
    let root = test_dir("dedupe");
    let primary = acquire_state_for_roots([&root, &root, &root], false).unwrap();
    primary.ensure_writable().unwrap();
    let secondary = acquire_state_for_roots([&root], true).unwrap();
    assert_read_only(&secondary);
    drop(primary);
    acquire_state_for_roots([&root], false)
        .unwrap()
        .ensure_writable()
        .unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn shared_config_with_split_data_and_cache_blocks_second_writer() {
    let shared = test_dir("shared-config");
    let a_data = test_dir("a-data");
    let a_cache = test_dir("a-cache");
    let b_data = test_dir("b-data");
    let b_cache = test_dir("b-cache");
    let primary = acquire_state_for_roots([&shared, &a_data, &a_cache], false).unwrap();
    let secondary = acquire_state_for_roots([&shared, &b_data, &b_cache], true).unwrap();
    assert_read_only(&secondary);
    drop(primary);
    for path in [shared, a_data, a_cache, b_data, b_cache] {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[test]
fn shared_cache_with_split_config_and_data_blocks_second_writer() {
    let shared = test_dir("shared-cache");
    let a_config = test_dir("a-config");
    let a_data = test_dir("a-data");
    let b_config = test_dir("b-config");
    let b_data = test_dir("b-data");
    let primary = acquire_state_for_roots([&a_config, &a_data, &shared], false).unwrap();
    let secondary = acquire_state_for_roots([&b_config, &b_data, &shared], true).unwrap();
    assert_read_only(&secondary);
    drop(primary);
    for path in [shared, a_config, a_data, b_config, b_data] {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[test]
fn fully_split_roots_allow_independent_writers() {
    let a = [
        test_dir("a-config"),
        test_dir("a-data"),
        test_dir("a-cache"),
    ];
    let b = [
        test_dir("b-config"),
        test_dir("b-data"),
        test_dir("b-cache"),
    ];
    let first = acquire_state_for_roots(&a, false).unwrap();
    let second = acquire_state_for_roots(&b, false).unwrap();
    first.ensure_writable().unwrap();
    second.ensure_writable().unwrap();
    drop((first, second));
    for path in a.into_iter().chain(b) {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[test]
fn partial_acquisition_failure_releases_all_earlier_roots() {
    let earlier = test_dir("a-earlier");
    let later = test_dir("z-later");
    let held_later = crate::util::safe_fs::try_lock_private_file(
        &normalize_root(&later).unwrap().join(WRITER_LEASE_FILE),
    )
    .unwrap()
    .unwrap();

    let failed = acquire_state_for_roots([&earlier, &later], true).unwrap();
    assert_read_only(&failed);
    let earlier_lock = crate::util::safe_fs::try_lock_private_file(
        &normalize_root(&earlier).unwrap().join(WRITER_LEASE_FILE),
    )
    .unwrap();
    assert!(
        earlier_lock.is_some(),
        "partial lease stranded the first root"
    );

    drop((earlier_lock, held_later));
    let _ = std::fs::remove_dir_all(earlier);
    let _ = std::fs::remove_dir_all(later);
}

#[test]
fn primary_remains_writable_until_drop_then_successor_acquires_every_root() {
    let roots = [test_dir("config"), test_dir("data"), test_dir("cache")];
    let primary = acquire_state_for_roots(&roots, false).unwrap();
    primary.ensure_writable().unwrap();
    assert_read_only(&acquire_state_for_roots(&roots, true).unwrap());

    drop(primary);
    let successor = acquire_state_for_roots(&roots, false).unwrap();
    successor.ensure_writable().unwrap();
    drop(successor);
    for path in roots {
        let _ = std::fs::remove_dir_all(path);
    }
}
