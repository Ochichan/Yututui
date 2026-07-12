use super::*;

#[test]
fn source_name_swap_before_staging_preserves_foreign_bytes_and_defers() {
    let dir = temp_dir("source-generation-swap");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("source.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"accepted source generation").unwrap();
    let accepted = accept_save(save_job(101, source.clone(), final_dir.clone(), "Swap")).unwrap();

    let original = temp_dir.join("original-source.mkv");
    std::fs::rename(&source, &original).unwrap();
    std::fs::write(&source, b"foreign source replacement").unwrap();
    let event = run_accepted(accepted);

    assert!(matches!(event, RecorderEvent::SaveDeferred { id: 101, .. }));
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"foreign source replacement"
    );
    assert_eq!(
        std::fs::read(&original).unwrap(),
        b"accepted source generation"
    );
    assert!(!final_dir.join("Swap.mkv").exists());
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir(&temp_dir))
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        1
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cleanup_name_swaps_never_remove_or_save_foreign_generations() {
    for (index, swap) in [
        durable::CleanupSwap::Source,
        durable::CleanupSwap::Stage,
        durable::CleanupSwap::Journal,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = temp_dir(&format!("cleanup-generation-swap-{index}"));
        let temp_dir = dir.join("temp");
        let final_dir = dir.join("final");
        let source = temp_dir.join("source.mkv");
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::write(&source, b"accepted source generation").unwrap();
        let accepted = accept_save(save_job(
            110 + index as u64,
            source,
            final_dir.clone(),
            "Safe",
        ))
        .unwrap();

        let (event, original, replacement) =
            durable::run_with_cleanup_name_swap(accepted, swap).unwrap();
        if matches!(swap, durable::CleanupSwap::Stage) {
            assert!(matches!(event, RecorderEvent::SaveDeferred { .. }));
            assert!(!final_dir.join("Safe.mkv").exists());
        } else {
            assert!(matches!(
                event,
                RecorderEvent::Saved {
                    recovery_owned: true,
                    durability_warning: Some(_),
                    ..
                }
            ));
            assert_eq!(
                std::fs::read(final_dir.join("Safe.mkv")).unwrap(),
                b"accepted source generation"
            );
        }
        assert_eq!(std::fs::read(&replacement).unwrap(), b"foreign replacement");
        assert!(
            original.exists(),
            "owned generation must remain recoverable"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn work_and_final_parent_generation_swaps_fail_closed() {
    for swap_final in [false, true] {
        let dir = temp_dir(if swap_final {
            "final-parent-swap"
        } else {
            "work-parent-swap"
        });
        let temp_dir = dir.join("temp");
        let final_dir = dir.join("final");
        let source = temp_dir.join("source.mkv");
        std::fs::create_dir_all(&temp_dir).unwrap();
        std::fs::write(&source, b"parent protected bytes").unwrap();
        let accepted = accept_save(save_job(120, source, final_dir.clone(), "Parent")).unwrap();

        let target = if swap_final {
            final_dir.clone()
        } else {
            final_dir.join(".ytt-recorder-work")
        };
        let original = target.with_file_name(format!(
            "{}.owned-generation",
            target.file_name().unwrap().to_string_lossy()
        ));
        std::fs::rename(&target, &original).unwrap();
        crate::util::safe_fs::ensure_private_dir_durable(&target).unwrap();
        let marker = target.join("foreign-marker");
        std::fs::write(&marker, b"foreign parent").unwrap();

        assert!(matches!(
            run_accepted(accepted),
            RecorderEvent::SaveDeferred { id: 120, .. }
        ));
        assert_eq!(std::fs::read(marker).unwrap(), b"foreign parent");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
#[cfg(not(windows))]
fn stale_owner_namespace_swap_is_preserved_and_reported_uncertain() {
    let dir = temp_dir("stale-owner-namespace-swap");
    let temp_root = dir.join("recordings");
    let final_dir = dir.join("final");
    let namespace =
        crate::recorder::ownership::owners_dir(&temp_root).join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    crate::util::safe_fs::ensure_private_dir_durable(&namespace).unwrap();
    std::fs::write(namespace.join("lease.lock"), b"").unwrap();
    std::fs::write(namespace.join("rec-1.mkv"), b"owned stale source").unwrap();

    let claim = crate::recorder::ownership::try_claim_stale_owner(&namespace)
        .unwrap()
        .expect("stale namespace should be claimable");
    let original = namespace.with_file_name("cccccccccccccccccccccccccccccccc");
    std::fs::rename(&namespace, &original).unwrap();
    crate::util::safe_fs::ensure_private_dir_durable(&namespace).unwrap();
    std::fs::write(namespace.join("lease.lock"), b"").unwrap();
    let replacement = namespace.join("foreign.mkv");
    std::fs::write(&replacement, b"foreign namespace bytes").unwrap();
    claim.verify().unwrap();
    drop(claim);

    let report = recover_pending(&temp_root, &final_dir);
    assert!(report.admission_uncertain, "{:?}", report.warnings);
    assert!(report.capacity_blocked());
    assert_eq!(
        std::fs::read(&replacement).unwrap(),
        b"foreign namespace bytes"
    );
    assert_eq!(
        std::fs::read(original.join("rec-1.mkv")).unwrap(),
        b"owned stale source"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("manual recovery")),
        "{:?}",
        report.warnings
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[cfg(windows)]
fn stale_owner_namespace_claim_blocks_directory_rename() {
    let dir = temp_dir("stale-owner-namespace-rename-blocked");
    let temp_root = dir.join("recordings");
    let namespace =
        crate::recorder::ownership::owners_dir(&temp_root).join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    crate::util::safe_fs::ensure_private_dir_durable(&namespace).unwrap();
    std::fs::write(namespace.join("lease.lock"), b"").unwrap();

    let claim = crate::recorder::ownership::try_claim_stale_owner(&namespace)
        .unwrap()
        .expect("stale namespace should be claimable");
    let moved = namespace.with_file_name("cccccccccccccccccccccccccccccccc");
    assert_eq!(
        std::fs::rename(&namespace, &moved).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    claim.verify().unwrap();
    drop(claim);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn journal_has_a_proven_finite_bound_and_ignores_a_torn_tail() {
    let dir = temp_dir("bounded-torn-journal");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("source.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"journal protected bytes").unwrap();
    let accepted = accept_save(save_job(130, source, final_dir.clone(), "Journal")).unwrap();

    let (largest_snapshot, projected_snapshot, projected_log, maximum_log) =
        durable::journal_budget_projection(&accepted).unwrap();
    assert!(largest_snapshot <= projected_snapshot);
    assert!(projected_log <= maximum_log);
    let (before, after) = durable::append_identical_journal_snapshot(&accepted).unwrap();
    assert_eq!(before, after, "an unchanged state spent a journal record");
    let (before_again, after_again) =
        durable::append_identical_journal_snapshot(&accepted).unwrap();
    assert_eq!(before_again, after_again);
    assert_eq!(after, before_again);

    durable::inject_torn_journal_tail(&accepted).unwrap();
    let path = saved_path(run_accepted(accepted));
    assert_eq!(path, final_dir.join("Journal.mkv"));
    assert_eq!(std::fs::read(path).unwrap(), b"journal protected bytes");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn byte_identical_foreign_destination_is_not_a_commit_proof() {
    let dir = temp_dir("same-content-foreign-destination");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("source.mkv");
    let bytes = b"distinct recording with equal bytes";
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, bytes).unwrap();
    let mut accepted =
        accept_save(save_job(131, source.clone(), final_dir.clone(), "Equal")).unwrap();
    let selected = durable::inject_ready_stage_before_commit(&mut accepted).unwrap();
    std::fs::write(&selected, bytes).unwrap();

    let saved = saved_path(run_accepted(accepted));
    assert_eq!(saved, final_dir.join("Equal (2).mkv"));
    assert_eq!(std::fs::read(&selected).unwrap(), bytes);
    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    assert!(
        !source.exists(),
        "source retires only after exact promotion"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn partial_first_journal_record_is_exactly_retired_and_retry_safe() {
    let dir = temp_dir("partial-first-journal");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("source.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"retry-safe recording").unwrap();

    let failure = match durable::accept_with_partial_initial_journal(save_job(
        132,
        source.clone(),
        final_dir.clone(),
        "Retry",
    )) {
        Err(event) => event,
        Ok(_) => panic!("a partial first record cannot cross durable acceptance"),
    };
    assert!(matches!(failure, RecorderEvent::SaveFailed { id: 132, .. }));
    assert!(source.exists(), "retry-safe failure must retain the source");
    assert!(durable::capacity_available(&temp_dir, &final_dir));
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir(&temp_dir))
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        0,
        "the invalid exact journal must not self-lock the spool"
    );

    let saved = saved_path(run(save_job(132, source, final_dir.clone(), "Retry")).unwrap());
    assert_eq!(std::fs::read(saved).unwrap(), b"retry-safe recording");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn partial_journal_cleanup_generation_mismatch_remains_ambiguous() {
    let dir = temp_dir("partial-journal-cleanup-swap");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("source.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"ambiguous recording").unwrap();

    let (result, swapped) = durable::accept_with_partial_initial_journal_cleanup_swap(save_job(
        133,
        source.clone(),
        final_dir.clone(),
        "Ambiguous",
    ));
    let event = match result {
        Err(event) => event,
        Ok(_) => panic!("a cleanup generation mismatch cannot be retry-safe"),
    };
    assert!(matches!(event, RecorderEvent::SaveDeferred { id: 133, .. }));
    let (owned, replacement) = swapped.unwrap();
    assert_eq!(std::fs::read(&owned).unwrap(), b"{\"schema\":1,");
    assert_eq!(
        std::fs::read(&replacement).unwrap(),
        b"foreign invalid replacement"
    );
    assert!(source.exists());
    assert!(!durable::capacity_available(&temp_dir, &final_dir));
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn lofty_tag_rewrite_mutates_only_the_open_generation_handle() {
    use lofty::file::TaggedFileExt as _;
    use lofty::probe::Probe;
    use lofty::tag::Accessor as _;
    use std::fs::OpenOptions;

    let dir = temp_dir("tag-handle-generation");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("audio.mp3");
    let owned = dir.join("owned-audio.mp3");
    let mut audio = vec![0_u8; 417 * 3];
    for offset in [0, 417, 834] {
        audio[offset..offset + 4].copy_from_slice(&[0xff, 0xfb, 0x90, 0x64]);
    }
    std::fs::write(&path, &audio).unwrap();
    let mut handle = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    std::fs::rename(&path, &owned).unwrap();
    std::fs::write(&path, b"foreign replacement").unwrap();

    crate::recorder::job::tag_file_handle(
        &mut handle,
        Some("Pinned title"),
        Some("Pinned artist"),
        Some("Pinned station"),
    )
    .unwrap();
    handle.sync_all().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");

    let tagged = Probe::open(&owned)
        .unwrap()
        .guess_file_type()
        .unwrap()
        .read()
        .unwrap();
    let tag = tagged.primary_tag().unwrap();
    assert_eq!(tag.title().as_deref(), Some("Pinned title"));
    assert_eq!(tag.artist().as_deref(), Some("Pinned artist"));
    assert_eq!(tag.album().as_deref(), Some("Pinned station"));
    let _ = std::fs::remove_dir_all(dir);
}
