use super::*;

pub(in crate::recorder::job) fn install_with_sync_fault(
    source: &Path,
    destination: &Path,
) -> io::Result<InstallOutcome> {
    let intent = test_intent(
        source,
        destination,
        "00000000000000000000000000000000",
        "fault",
    );
    safe_fs::ensure_private_dir(&work_dir(&intent.final_dir))?;
    let mut stage = prepare_stage(&intent)?;
    atomic_install_noreplace(&stage.path, destination)?;
    Ok(finalize_visible_install(
        &mut stage,
        destination,
        |_| {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected post-install sync failure",
            ))
        },
        safe_fs::remove_private_file_durable,
    ))
}

pub(in crate::recorder::job) fn is_committed_sync_failure(outcome: &InstallOutcome) -> bool {
    matches!(outcome, InstallOutcome::CommittedButSyncFailed(_))
}

pub(in crate::recorder::job) fn install_order_trace(
    source: &Path,
    destination: &Path,
) -> io::Result<Vec<&'static str>> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let intent = test_intent(
        source,
        destination,
        "11111111111111111111111111111111",
        "order",
    );
    safe_fs::ensure_private_dir(&work_dir(&intent.final_dir))?;
    let mut stage = prepare_stage(&intent)?;
    #[cfg(not(windows))]
    let stage_path = stage.path.clone();
    atomic_install_noreplace(&stage.path, destination)?;

    let trace = Rc::new(RefCell::new(Vec::new()));
    let sync_trace = Rc::clone(&trace);
    let cleanup_trace = Rc::clone(&trace);
    let outcome = finalize_visible_install(
        &mut stage,
        destination,
        |path| {
            assert!(path.exists());
            #[cfg(not(windows))]
            assert!(stage_path.exists());
            sync_trace.borrow_mut().push("sync_destination_parent");
            safe_fs::sync_parent_dir(path)
        },
        |path| {
            cleanup_trace.borrow_mut().push("unlink_stage");
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            cleanup_trace.borrow_mut().push("sync_stage_parent");
            safe_fs::sync_parent_dir(path)
        },
    );
    assert!(matches!(outcome, InstallOutcome::Durable));
    let result = trace.borrow().clone();
    Ok(result)
}

fn test_intent(source: &Path, destination: &Path, token: &str, filename: &str) -> SaveIntent {
    let final_dir = destination.parent().unwrap().to_path_buf();
    safe_fs::ensure_private_dir(&work_dir(&final_dir)).unwrap();
    safe_fs::ensure_private_dir(source.parent().unwrap()).unwrap();
    let source_parent = pin_private_absolute_dir(source.parent().unwrap(), None).unwrap();
    let source_generation = source_parent
        .open_child_readonly(source.file_name().unwrap())
        .unwrap();
    let final_parent = pin_absolute_dir(&final_dir, None).unwrap();
    let work_parent = pin_private_absolute_dir(&work_dir(&final_dir), None).unwrap();
    let (_, save_lock_identity) = work_parent
        .try_lock_child(std::ffi::OsStr::new(SAVE_LOCK_NAME), None, true)
        .unwrap()
        .unwrap();
    SaveIntent {
        schema: JOURNAL_SCHEMA,
        token: token.to_owned(),
        id: 1,
        source: source.to_path_buf(),
        final_dir,
        filename: filename.to_owned(),
        ext: "mkv".to_owned(),
        title: None,
        artist: None,
        station: None,
        source_parent_identity: Some(source_parent.identity()),
        source_identity: Some(source_generation.identity()),
        final_parent_identity: Some(final_parent.identity()),
        work_parent_identity: Some(work_parent.identity()),
        save_lock_identity: Some(save_lock_identity),
        journal_parent_identity: None,
        journal_identity: None,
        stage_identity: None,
        destination_identity: None,
        settled: false,
        collision_reselected: false,
        destination: Some(destination.to_path_buf()),
        commit_identity: None,
    }
}

pub(in crate::recorder::job) fn install_with_stage_sync_fault(
    source: &Path,
    destination: &Path,
) -> io::Result<(InstallOutcome, Vec<&'static str>)> {
    use std::cell::RefCell;
    use std::rc::Rc;

    let intent = test_intent(
        source,
        destination,
        "22222222222222222222222222222222",
        "stage-fault",
    );
    safe_fs::ensure_private_dir(&work_dir(&intent.final_dir))?;
    let mut stage = prepare_stage(&intent)?;
    atomic_install_noreplace(&stage.path, destination)?;

    let trace = Rc::new(RefCell::new(Vec::new()));
    let sync_trace = Rc::clone(&trace);
    let cleanup_trace = Rc::clone(&trace);
    let outcome = finalize_visible_install(
        &mut stage,
        destination,
        |path| {
            sync_trace.borrow_mut().push("sync_destination_parent");
            safe_fs::sync_parent_dir(path)
        },
        |path| {
            cleanup_trace.borrow_mut().push("unlink_stage");
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            cleanup_trace.borrow_mut().push("sync_stage_parent_failed");
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected stage-parent sync failure",
            ))
        },
    );
    let result = trace.borrow().clone();
    Ok((outcome, result))
}

pub(in crate::recorder::job) fn cleanup_with_injected_journal_entry_error(
    temp_dir: &Path,
    final_dir: &Path,
) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let entries = vec![
        Ok(registry_journal_dir(temp_dir).join("valid.json")),
        Err(io::Error::other("injected read_dir entry failure")),
    ];
    let paths = collect_journal_paths(entries);
    if let Some(preserve) = pending_sources_from_paths(paths, &mut report) {
        cleanup_ordinary_temps(temp_dir, final_dir, &preserve, &HashSet::new(), &mut report);
    }
    report
}

pub(in crate::recorder::job) fn journal_json(save: &AcceptedSave) -> io::Result<serde_json::Value> {
    serde_json::to_value(load_journal(&save.journal_path)?).map_err(io::Error::other)
}

pub(in crate::recorder::job) fn journal_path(save: &AcceptedSave) -> PathBuf {
    save.journal_path.clone()
}

pub(in crate::recorder::job) fn journal_budget_projection(
    save: &AcceptedSave,
) -> io::Result<(usize, usize, usize, usize)> {
    let initial = serde_json::to_vec(&save.intent)
        .map_err(io::Error::other)?
        .len();
    let mut largest_state = save.intent.clone();
    largest_state.destination = Some(largest_state.final_dir.join(format!(
        "{} (100000).{}",
        "x".repeat(200),
        largest_state.ext
    )));
    largest_state.stage_identity = largest_state.source_identity;
    largest_state.destination_identity = largest_state.source_identity;
    largest_state.commit_identity = Some(CommitIdentity {
        len: u64::MAX,
        sha256: "f".repeat(64),
    });
    largest_state.settled = true;
    largest_state.collision_reselected = true;
    let largest_snapshot = serde_json::to_vec(&largest_state)
        .map_err(io::Error::other)?
        .len();
    let projected_snapshot = initial + MAX_SNAPSHOT_GROWTH_BYTES;
    let projected_log = (projected_snapshot + 2) * MAX_JOURNAL_RECORDS;
    Ok((
        largest_snapshot,
        projected_snapshot,
        projected_log,
        MAX_JOURNAL_BYTES,
    ))
}

pub(in crate::recorder::job) fn append_identical_journal_snapshot(
    save: &AcceptedSave,
) -> io::Result<(u64, u64)> {
    let before = std::fs::metadata(&save.journal_path)?.len();
    write_journal(&save.journal_path, &save.intent)?;
    let after = std::fs::metadata(&save.journal_path)?.len();
    Ok((before, after))
}

pub(in crate::recorder::job) fn inject_torn_journal_tail(save: &AcceptedSave) -> io::Result<()> {
    let parent = pin_absolute_dir(
        save.journal_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording journal has no parent",
            )
        })?,
        save.intent.journal_parent_identity,
    )?;
    let mut journal = parent.open_existing_child(
        save.journal_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording journal has no basename",
            )
        })?,
        save.intent.journal_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "recording journal identity is missing",
            )
        })?,
    )?;
    let file = journal.file_mut()?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(b"{\"schema\":")?;
    journal.sync_durable()
}

pub(in crate::recorder::job) fn cleanup_atomic_temps_for_test(
    temp_dir: &Path,
    final_dir: &Path,
) -> RecoveryReport {
    let _publication = ownership::acquire_publication_lock(temp_dir).unwrap();
    let mut report = RecoveryReport::default();
    cleanup_atomic_journal_temps(temp_dir, final_dir, &mut report);
    report
}

pub(in crate::recorder::job) fn hold_save_lock_for_test(
    final_dir: &Path,
) -> io::Result<safe_fs::AdvisoryFileLock> {
    safe_fs::ensure_private_dir(&work_dir(final_dir))?;
    let parent = pin_private_absolute_dir(&work_dir(final_dir), None)?;
    parent
        .try_lock_child(std::ffi::OsStr::new(SAVE_LOCK_NAME), None, true)?
        .map(|(lock, _)| lock)
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "save lock is already held"))
}

pub(in crate::recorder::job) fn settle_with_source_cleanup_fault(
    save: &AcceptedSave,
) -> io::Result<Option<String>> {
    settle_commit_with(
        save,
        save.intent.final_dir.join("visible.mkv"),
        InstallOutcome::Durable,
        |_| {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected source cleanup failure",
            ))
        },
        |_| panic!("journal cleanup must not run while source ownership is unresolved"),
    )
    .map(|(_, warning)| warning)
}

pub(in crate::recorder::job) fn run_with_destination_identity_fault(
    mut save: AcceptedSave,
) -> io::Result<(RecorderEvent, PathBuf)> {
    let destination = inject_ready_stage_before_commit(&mut save)?;
    let work = pin_private_absolute_dir(
        &work_dir(&save.intent.final_dir),
        save.intent.work_parent_identity,
    )?;
    let stage_path = stage_path(&save.intent);
    let stage = work.open_existing_child(
        stage_path.file_name().unwrap(),
        save.intent.stage_identity.unwrap(),
    )?;
    let final_parent = pin_absolute_dir(&save.intent.final_dir, save.intent.final_parent_identity)?;
    let _destination = stage.promote_noreplace(&final_parent, destination.file_name().unwrap())?;
    let event = RecorderEvent::SaveDeferred {
        id: save.intent.id,
        error: "injected destination identity read failure".to_owned(),
    };
    Ok((event, destination))
}

pub(in crate::recorder::job) fn journal_exists(save: &AcceptedSave) -> bool {
    save.journal_path.exists()
}

pub(in crate::recorder::job) fn duplicate_accepted(
    save: &AcceptedSave,
) -> io::Result<AcceptedSave> {
    Ok(AcceptedSave {
        journal_path: save.journal_path.clone(),
        intent: load_journal(&save.journal_path)?,
        temp_root: save.temp_root.clone(),
        close_barrier: save.close_barrier.clone(),
    })
}

pub(in crate::recorder::job) fn accept_with_post_rename_sync_fault(
    job: RecorderJob,
) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer(job, |path, intent| {
        create_journal(path, intent)?;
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected journal parent sync failure after rename",
        ))
    })
}

pub(in crate::recorder::job) fn accept_with_unpublished_would_block(
    job: RecorderJob,
) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer(job, |_, _| {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "injected failure before journal publication",
        ))
    })
}

pub(in crate::recorder::job) fn accept_with_partial_initial_journal(
    job: RecorderJob,
) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer(job, |path, intent| {
        let parent = pin_private_absolute_dir(
            path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recording journal has no parent",
                )
            })?,
            intent.journal_parent_identity,
        )?;
        let mut journal = parent.create_new(path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording journal has no basename",
            )
        })?)?;
        intent.journal_identity = Some(journal.identity());
        journal.file_mut()?.write_all(b"{\"schema\":1,")?;
        journal.sync_durable()?;
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected disk-full during the first journal record",
        ))
    })
}

pub(in crate::recorder::job) fn accept_with_partial_initial_journal_cleanup_swap(
    job: RecorderJob,
) -> (
    Result<AcceptedSave, RecorderEvent>,
    io::Result<(PathBuf, PathBuf)>,
) {
    let mut swapped = None;
    let result = accept_with_writer(job, |path, intent| {
        let parent = pin_private_absolute_dir(
            path.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recording journal has no parent",
                )
            })?,
            intent.journal_parent_identity,
        )?;
        let mut journal = parent.create_new(path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording journal has no basename",
            )
        })?)?;
        intent.journal_identity = Some(journal.identity());
        journal.file_mut()?.write_all(b"{\"schema\":1,")?;
        journal.sync_durable()?;
        drop(journal);

        let owned = path.with_file_name(format!(
            ".{}.owned-generation",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::rename(path, &owned)?;
        std::fs::write(path, b"foreign invalid replacement")?;
        safe_fs::sync_parent_dir(path)?;
        swapped = Some((owned, path.to_path_buf()));
        Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected disk-full with cleanup generation swap",
        ))
    });
    (
        result,
        swapped.ok_or_else(|| io::Error::other("cleanup swap hook was not reached")),
    )
}

pub(in crate::recorder::job) fn accept_with_double_sync_fault(
    job: RecorderJob,
) -> (Result<AcceptedSave, RecorderEvent>, Vec<&'static str>) {
    use std::sync::{Arc, Mutex};

    let trace = Arc::new(Mutex::new(Vec::new()));
    let write_trace = Arc::clone(&trace);
    let sync_trace = Arc::clone(&trace);
    let result = accept_with_writer_and_limits(
        job,
        move |path, intent| {
            create_journal(path, intent)?;
            write_trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("journal_visible");
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected first parent sync failure",
            ))
        },
        move |_| {
            sync_trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push("parent_resync_failed");
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected second parent sync failure",
            ))
        },
        safe_fs::ensure_dir_durable,
        |file| file.sync_all(),
        MAX_PENDING_SAVES,
        MAX_PENDING_SOURCE_BYTES,
    );
    let trace = Arc::try_unwrap(trace)
        .expect("fault trace closures released")
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (result, trace)
}

pub(in crate::recorder::job) fn accept_with_limits(
    job: RecorderJob,
    max_count: usize,
    max_bytes: u64,
) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer_and_limits(
        job,
        create_journal,
        safe_fs::sync_parent_dir,
        safe_fs::ensure_dir_durable,
        |file| file.sync_all(),
        max_count,
        max_bytes,
    )
}

pub(in crate::recorder::job) fn accept_with_final_dir_sync_fault(
    job: RecorderJob,
) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer_and_limits(
        job,
        create_journal,
        safe_fs::sync_parent_dir,
        |path| {
            std::fs::create_dir_all(path)?;
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected final directory parent sync failure",
            ))
        },
        |file| file.sync_all(),
        MAX_PENDING_SAVES,
        MAX_PENDING_SOURCE_BYTES,
    )
}

pub(in crate::recorder::job) fn accept_with_source_sync_fault(
    job: RecorderJob,
) -> Result<AcceptedSave, RecorderEvent> {
    accept_with_writer_and_limits(
        job,
        create_journal,
        safe_fs::sync_parent_dir,
        safe_fs::ensure_dir_durable,
        |_| {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "injected recording source sync failure",
            ))
        },
        MAX_PENDING_SAVES,
        MAX_PENDING_SOURCE_BYTES,
    )
}

pub(in crate::recorder::job) fn run_with_ack_timeout(
    save: AcceptedSave,
    timeout: Duration,
) -> RecorderEvent {
    let id = save.intent.id;
    if let Some(barrier) = &save.close_barrier
        && let Err(error) = barrier.wait_for_test(timeout)
    {
        return RecorderEvent::SaveDeferred { id, error };
    }
    run_accepted(save)
}

pub(in crate::recorder::job) fn inject_partial_stage(
    save: &mut AcceptedSave,
) -> io::Result<PathBuf> {
    let _lock = acquire_save_lock(&save.intent)?;
    let destination = unique_recording_path(
        &save.intent.final_dir,
        accepted_journal_dir(save)?,
        &save.intent.filename,
        &save.intent.ext,
        &save.intent.token,
    )?;
    save.intent.destination = Some(destination.clone());
    save.intent.commit_identity = None;
    write_journal(&save.journal_path, &save.intent)?;
    let work_parent = pin_private_absolute_dir(
        &work_dir(&save.intent.final_dir),
        save.intent.work_parent_identity,
    )?;
    let stage_path = stage_path(&save.intent);
    let mut stage = work_parent.create_new(stage_path.file_name().unwrap())?;
    save.intent.stage_identity = Some(stage.identity());
    write_journal(&save.journal_path, &save.intent)?;
    stage.file_mut()?.write_all(b"partial")?;
    stage.sync_durable()?;
    Ok(destination)
}

pub(in crate::recorder::job) fn inject_ready_stage_before_commit(
    save: &mut AcceptedSave,
) -> io::Result<PathBuf> {
    let _lock = acquire_save_lock(&save.intent)?;
    let destination = select_destination(save)?;
    let work_parent = pin_private_absolute_dir(
        &work_dir(&save.intent.final_dir),
        save.intent.work_parent_identity,
    )?;
    let stage = prepare_or_reuse_stage(save, &work_parent)?;
    save.intent.commit_identity = Some(file_identity_handle(stage.generation.file()?)?);
    write_journal(&save.journal_path, &save.intent)?;
    Ok(destination)
}

pub(in crate::recorder::job) fn inject_legacy_destination(
    save: &mut AcceptedSave,
) -> io::Result<PathBuf> {
    let _lock = acquire_save_lock(&save.intent)?;
    let destination = select_destination(save)?;
    let mut value = serde_json::to_value(&save.intent).map_err(io::Error::other)?;
    let object = value
        .as_object_mut()
        .expect("SaveIntent serializes as an object");
    for field in [
        "commit_identity",
        "source_parent_identity",
        "source_identity",
        "final_parent_identity",
        "work_parent_identity",
        "save_lock_identity",
        "journal_parent_identity",
        "journal_identity",
        "stage_identity",
        "destination_identity",
        "settled",
        "collision_reselected",
    ] {
        object.remove(field);
    }
    let journal_parent = pin_private_absolute_dir(
        save.journal_path.parent().unwrap(),
        save.intent.journal_parent_identity,
    )?;
    let mut journal = journal_parent.open_existing_child(
        save.journal_path.file_name().unwrap(),
        save.intent.journal_identity.unwrap(),
    )?;
    let mut bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let file = journal.file_mut()?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    journal.sync_durable()?;
    Ok(destination)
}

pub(in crate::recorder::job) fn run_with_external_collision(
    mut save: AcceptedSave,
    external_bytes: &[u8],
) -> RecorderEvent {
    let external_path = save
        .intent
        .final_dir
        .join(format!("{}.{}", save.intent.filename, save.intent.ext));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&external_path)
        .and_then(|mut external| {
            external.write_all(external_bytes)?;
            external.sync_all()
        })
        .and_then(|()| execute(&mut save));
    match result {
        Ok(ExecuteOutcome::Committed(final_path, warning)) => RecorderEvent::Saved {
            id: save.intent.id,
            final_path,
            recovery_owned: warning.is_some(),
            durability_warning: warning,
            capacity_available: spool_has_capacity(&save),
        },
        Ok(ExecuteOutcome::AlreadySettled) => RecorderEvent::AlreadySettled {
            id: save.intent.id,
            capacity_available: spool_has_capacity(&save),
        },
        Err(error) => RecorderEvent::SaveDeferred {
            id: save.intent.id,
            error: sanitize_error_text(error.to_string()),
        },
    }
}

#[derive(Clone, Copy)]
pub(in crate::recorder::job) enum CleanupSwap {
    Source,
    Stage,
    Journal,
}

pub(in crate::recorder::job) fn run_with_cleanup_name_swap(
    mut save: AcceptedSave,
    swap: CleanupSwap,
) -> io::Result<(RecorderEvent, PathBuf, PathBuf)> {
    if matches!(swap, CleanupSwap::Stage) {
        let _destination = inject_ready_stage_before_commit(&mut save)?;
        let (owned, replacement) = swap_generation_name(&stage_path(&save.intent))?;
        return Ok((run_accepted(save), owned, replacement));
    }
    let mut swapped = None;
    let result = execute_with_cleanup_hook(&mut save, |accepted| {
        if swapped.is_some() {
            return Ok(());
        }
        let path = match swap {
            CleanupSwap::Source => accepted.intent.source.clone(),
            CleanupSwap::Journal => accepted.journal_path.clone(),
            CleanupSwap::Stage => unreachable!("stage swaps happen before promotion"),
        };
        swapped = Some(swap_generation_name(&path)?);
        Ok(())
    });
    let event = match result {
        Ok(ExecuteOutcome::Committed(final_path, warning)) => RecorderEvent::Saved {
            id: save.intent.id,
            final_path,
            recovery_owned: warning.is_some(),
            durability_warning: warning,
            capacity_available: spool_has_capacity(&save),
        },
        Ok(ExecuteOutcome::AlreadySettled) => RecorderEvent::AlreadySettled {
            id: save.intent.id,
            capacity_available: spool_has_capacity(&save),
        },
        Err(error) => RecorderEvent::SaveDeferred {
            id: save.intent.id,
            error: sanitize_error_text(error.to_string()),
        },
    };
    let (owned, replacement) =
        swapped.ok_or_else(|| io::Error::other("cleanup hook not reached"))?;
    Ok((event, owned, replacement))
}

fn swap_generation_name(path: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let owned = path.with_file_name(format!(
        ".{}.owned-generation",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::rename(path, &owned)?;
    let mut foreign = OpenOptions::new().write(true).create_new(true).open(path)?;
    foreign.write_all(b"foreign replacement")?;
    foreign.sync_all()?;
    safe_fs::sync_parent_dir(path)?;
    Ok((owned, path.to_path_buf()))
}
