use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use super::durable;
use super::*;

mod capacity_growth;
mod path_identity;

fn temp_dir(name: &str) -> PathBuf {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-recorder-job-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn save_job(id: u64, temp: PathBuf, final_dir: PathBuf, filename: &str) -> RecorderJob {
    let temp_dir = temp
        .parent()
        .expect("test recording source has a temp parent")
        .to_path_buf();
    RecorderJob::Save {
        id,
        temp,
        temp_dir,
        final_dir,
        filename: filename.to_owned(),
        ext: "mkv",
        title: Some(format!("Title {id}")),
        artist: Some("Artist".to_owned()),
        station: Some("Station".to_owned()),
        close_barrier: None,
        automatic: false,
        bypass_limits: false,
    }
}

fn with_barrier(
    job: RecorderJob,
    barrier: crate::recorder::barrier::CommandBarrier,
) -> RecorderJob {
    match job {
        RecorderJob::Save {
            id,
            temp,
            temp_dir,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            automatic,
            bypass_limits,
            ..
        } => RecorderJob::Save {
            id,
            temp,
            temp_dir,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier: Some(barrier),
            automatic,
            bypass_limits,
        },
        RecorderJob::Discard { .. }
        | RecorderJob::AwaitTransition { .. }
        | RecorderJob::ProbeCapacity { .. } => {
            unreachable!("test helper expects Save")
        }
    }
}

fn automatic(job: RecorderJob) -> RecorderJob {
    match job {
        RecorderJob::Save {
            id,
            temp,
            temp_dir,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier,
            bypass_limits,
            ..
        } => RecorderJob::Save {
            id,
            temp,
            temp_dir,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier,
            automatic: true,
            bypass_limits,
        },
        RecorderJob::Discard { .. }
        | RecorderJob::AwaitTransition { .. }
        | RecorderJob::ProbeCapacity { .. } => {
            unreachable!("test helper expects Save")
        }
    }
}

fn bypass_limits(job: RecorderJob) -> RecorderJob {
    match automatic(job) {
        RecorderJob::Save {
            id,
            temp,
            temp_dir,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier,
            automatic,
            ..
        } => RecorderJob::Save {
            id,
            temp,
            temp_dir,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier,
            automatic,
            bypass_limits: true,
        },
        _ => unreachable!("test helper expects Save"),
    }
}

fn with_temp_root(job: RecorderJob, temp_root: PathBuf) -> RecorderJob {
    match job {
        RecorderJob::Save {
            id,
            temp,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier,
            automatic,
            bypass_limits,
            ..
        } => RecorderJob::Save {
            id,
            temp,
            temp_dir: temp_root,
            final_dir,
            filename,
            ext,
            title,
            artist,
            station,
            close_barrier,
            automatic,
            bypass_limits,
        },
        RecorderJob::Discard { .. }
        | RecorderJob::AwaitTransition { .. }
        | RecorderJob::ProbeCapacity { .. } => {
            unreachable!("test helper expects Save")
        }
    }
}

fn saved_path(event: RecorderEvent) -> PathBuf {
    match event {
        RecorderEvent::Saved {
            final_path,
            durability_warning: None,
            ..
        } => final_path,
        RecorderEvent::Saved {
            durability_warning: Some(warning),
            ..
        } => panic!("unexpected durability warning: {warning}"),
        RecorderEvent::AlreadySettled { id, .. } => {
            panic!("unexpected peer settlement for recording {id}")
        }
        RecorderEvent::SaveDeferred { error, .. } => panic!("unexpected deferral: {error}"),
        RecorderEvent::SaveFailed { error, .. } => panic!("unexpected failure: {error}"),
        RecorderEvent::CapacityBlocked { .. } => panic!("unexpected spool capacity block"),
        RecorderEvent::SaveAccepted { .. } | RecorderEvent::CapacityProbed { .. } => {
            panic!("unexpected recorder control event")
        }
        RecorderEvent::TransitionResolved { .. } => panic!("unexpected transition outcome"),
    }
}

#[test]
fn discard_job_is_best_effort_without_an_event() {
    let dir = temp_dir("wipe-discard");
    let temp = dir.join("segment.tmp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&temp, b"partial").unwrap();

    assert!(
        run(RecorderJob::Discard {
            temp: temp.clone(),
            close_barrier: None,
        })
        .is_none()
    );
    assert!(!temp.exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn accepted_but_unstarted_save_recovers_once_before_temp_cleanup() {
    let dir = temp_dir("cutoff-recovery");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let temp = temp_dir.join("rec-1.mkv");
    let ordinary = temp_dir.join("rec-2.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&temp, b"explicitly accepted audio").unwrap();
    std::fs::write(&ordinary, b"ordinary undecided audio").unwrap();

    let accepted = accept_save(save_job(7, temp.clone(), final_dir.clone(), "Song")).unwrap();
    assert!(durable::journal_exists(&accepted));
    drop(accepted); // deterministic hard cutoff before the blocking closure starts

    let first = recover_pending(&temp_dir, &final_dir);
    assert_eq!(first.recovered, 1, "warnings: {:?}", first.warnings);
    assert_eq!(first.pending, 0, "warnings: {:?}", first.warnings);
    assert_eq!(
        std::fs::read(final_dir.join("Song.mkv")).unwrap(),
        b"explicitly accepted audio"
    );
    assert!(!temp.exists());
    assert!(
        !ordinary.exists(),
        "ordinary Decide temp keeps startup-wipe semantics"
    );

    let second = recover_pending(&temp_dir, &final_dir);
    assert_eq!(second.recovered, 0, "recovery is idempotent");
    assert!(!final_dir.join("Song (2).mkv").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stable_registry_recovers_old_destination_after_recording_config_changes() {
    let dir = temp_dir("destination-change");
    let temp_dir = dir.join("temp");
    let old_final_dir = dir.join("old-final");
    let new_final_dir = dir.join("new-final");
    let source = temp_dir.join("accepted.mkv");
    let ordinary = temp_dir.join("ordinary.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"save to the originally accepted destination").unwrap();
    std::fs::write(&ordinary, b"ordinary stale temp").unwrap();

    let accepted = accept_save(save_job(
        23,
        source.clone(),
        old_final_dir.clone(),
        "Original",
    ))
    .unwrap();
    assert!(durable::journal_exists(&accepted));
    drop(accepted);

    let report = recover_pending(&temp_dir, &new_final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(report.pending, 0, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(old_final_dir.join("Original.mkv")).unwrap(),
        b"save to the originally accepted destination"
    );
    assert!(!new_final_dir.join("Original.mkv").exists());
    assert!(!source.exists());
    assert!(!ordinary.exists());
    assert!(
        crate::recorder::ownership::stable_root(&temp_dir).exists(),
        "stable discovery root must survive ordinary temp cleanup"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn config_change_preserves_recovered_old_destination_nested_in_temp() {
    let dir = temp_dir("nested-destination-change");
    let temp_dir = dir.join("temp");
    let old_final_dir = temp_dir.join("saved");
    let new_final_dir = dir.join("new-final");
    let source = temp_dir.join("accepted.mkv");
    let ordinary = temp_dir.join("ordinary.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"nested destination survives cleanup").unwrap();
    std::fs::write(&ordinary, b"ordinary stale temp").unwrap();

    let accepted = accept_save(save_job(
        27,
        source.clone(),
        old_final_dir.clone(),
        "Nested",
    ))
    .unwrap();
    drop(accepted);

    let report = recover_pending(&temp_dir, &new_final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(report.pending, 0, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(old_final_dir.join("Nested.mkv")).unwrap(),
        b"nested destination survives cleanup"
    );
    assert!(!new_final_dir.join("Nested.mkv").exists());
    assert!(!source.exists());
    assert!(!ordinary.exists());
    assert!(crate::recorder::ownership::stable_root(&temp_dir).exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn post_rename_journal_sync_failure_never_returns_retry_safe_failure() {
    let dir = temp_dir("accept-sync-fault");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("accepted.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"accepted despite ambiguous sync").unwrap();

    let accepted = durable::accept_with_post_rename_sync_fault(save_job(
        24,
        source,
        final_dir.clone(),
        "Ambiguous",
    ))
    .expect("a visible exact journal crosses the no-retry acceptance boundary");
    assert!(durable::journal_exists(&accepted));
    let path = saved_path(run_accepted(accepted));
    assert_eq!(path, final_dir.join("Ambiguous.mkv"));
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"accepted despite ambiguous sync"
    );
    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 0, "settled intent must not replay");
    assert!(!final_dir.join("Ambiguous (2).mkv").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unpublished_would_block_error_remains_retry_safe() {
    let dir = temp_dir("unpublished-would-block");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("not-accepted.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"no journal owns these bytes").unwrap();

    let event = match durable::accept_with_unpublished_would_block(save_job(
        26,
        source.clone(),
        final_dir,
        "Retryable",
    )) {
        Err(event) => event,
        Ok(_) => panic!("an unpublished request must remain retry-safe"),
    };
    assert!(matches!(event, RecorderEvent::SaveFailed { id: 26, .. }));
    assert!(source.exists());
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir(&temp_dir))
            .unwrap()
            .count(),
        0
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn nested_final_directory_sync_failure_precedes_journal_acceptance() {
    let dir = temp_dir("final-dir-sync-fault");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("new").join("nested").join("final");
    let source = temp_dir.join("not-accepted.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"source remains app-owned").unwrap();

    let event = match durable::accept_with_final_dir_sync_fault(save_job(
        28,
        source.clone(),
        final_dir.clone(),
        "Retryable",
    )) {
        Err(event) => event,
        Ok(_) => panic!("an unsynced final directory must not cross journal acceptance"),
    };

    assert!(matches!(event, RecorderEvent::SaveFailed { id: 28, .. }));
    assert!(source.exists());
    assert!(final_dir.exists(), "fault happens after mkdir publication");
    assert!(
        !crate::recorder::ownership::pending_dir(&temp_dir).exists(),
        "the recovery journal must not publish after its output tree failed durability"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn source_sync_failure_precedes_journal_acceptance_and_retains_exact_source() {
    let dir = temp_dir("source-sync-fault");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("not-accepted.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"source bytes remain app-owned").unwrap();

    let event = match durable::accept_with_source_sync_fault(automatic(save_job(
        29,
        source.clone(),
        final_dir,
        "Retryable",
    ))) {
        Err(event) => event,
        Ok(_) => panic!("an unsynced recording source must not cross journal acceptance"),
    };

    assert!(matches!(
        event,
        RecorderEvent::SaveFailed {
            id: 29,
            automatic: true,
            ref error,
        } if error.contains("source sync failure")
    ));
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"source bytes remain app-owned"
    );
    assert!(
        !crate::recorder::ownership::pending_dir(&temp_dir).exists(),
        "no recovery journal may claim a source whose durability sync failed"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn two_contenders_for_exact_intent_commit_once_and_stale_owner_stops() {
    let dir = temp_dir("exact-contenders");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_dir.join("accepted.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"one accepted intent").unwrap();

    let first = accept_save(save_job(25, source, final_dir.clone(), "Once")).unwrap();
    let second = durable::duplicate_accepted(&first).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let mut contenders = Vec::new();
    for accepted in [first, second] {
        let barrier = Arc::clone(&barrier);
        contenders.push(std::thread::spawn(move || {
            barrier.wait();
            run_accepted(accepted)
        }));
    }
    let events = contenders
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RecorderEvent::Saved { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RecorderEvent::AlreadySettled { id: 25, .. }))
            .count(),
        1
    );
    assert_eq!(
        std::fs::read(final_dir.join("Once.mkv")).unwrap(),
        b"one accepted intent"
    );
    assert!(!final_dir.join("Once (2).mkv").exists());
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir(&temp_dir))
            .unwrap()
            .count(),
        0
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn partial_staging_cutoff_is_rebuilt_without_exposing_partial_destination() {
    let dir = temp_dir("partial-cutoff");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let temp = temp_dir.join("rec-1.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&temp, b"complete recording bytes").unwrap();

    let mut accepted = accept_save(save_job(8, temp, final_dir.clone(), "Song")).unwrap();
    let destination = durable::inject_partial_stage(&mut accepted).unwrap();
    assert!(!destination.exists(), "staging bytes must stay hidden");
    drop(accepted);

    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(destination).unwrap(),
        b"complete recording bytes"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn identity_persisted_cutoff_before_commit_recovers_exactly_once() {
    #[derive(serde::Deserialize)]
    struct PreIdentityReader {
        schema: u8,
        destination: Option<PathBuf>,
    }

    let dir = temp_dir("ready-cutoff");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let temp = temp_dir.join("rec-1.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&temp, b"complete recording before commit").unwrap();

    let mut accepted = accept_save(save_job(18, temp, final_dir.clone(), "Ready")).unwrap();
    let destination = durable::inject_ready_stage_before_commit(&mut accepted).unwrap();
    let journal = durable::journal_json(&accepted).unwrap();
    assert!(journal["commit_identity"]["sha256"].as_str().is_some());
    let rollback_reader: PreIdentityReader = serde_json::from_value(journal).unwrap();
    assert_eq!(rollback_reader.schema, 1);
    assert_eq!(
        rollback_reader.destination.as_deref(),
        Some(destination.as_path())
    );
    assert!(
        !destination.exists(),
        "identity persistence must precede install"
    );
    drop(accepted);

    let first = recover_pending(&temp_dir, &final_dir);
    assert_eq!(first.recovered, 1, "warnings: {:?}", first.warnings);
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"complete recording before commit"
    );
    let second = recover_pending(&temp_dir, &final_dir);
    assert_eq!(
        second.recovered, 0,
        "committed identity makes replay idempotent"
    );
    assert!(!final_dir.join("Ready (2).mkv").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn synced_stage_identity_recovers_even_if_original_source_is_later_unavailable() {
    let dir = temp_dir("stage-only-recovery");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let temp = temp_dir.join("rec-1.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&temp, b"durable staged recording").unwrap();

    let mut accepted = accept_save(save_job(21, temp.clone(), final_dir.clone(), "Stage")).unwrap();
    let destination = durable::inject_ready_stage_before_commit(&mut accepted).unwrap();
    std::fs::remove_file(&temp).unwrap();
    drop(accepted);

    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(destination).unwrap(),
        b"durable staged recording"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pending_cutoff_reserves_its_name_from_a_fast_successor() {
    let dir = temp_dir("fast-restart-reservation");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let first_temp = temp_dir.join("old-process.mkv");
    let second_temp = temp_dir.join("successor.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&first_temp, b"old accepted save").unwrap();
    std::fs::write(&second_temp, b"new process save").unwrap();

    let mut first = accept_save(save_job(1, first_temp, final_dir.clone(), "Same")).unwrap();
    let first_destination = durable::inject_partial_stage(&mut first).unwrap();
    drop(first);

    let second = accept_save(save_job(2, second_temp, final_dir.clone(), "Same")).unwrap();
    let second_destination = saved_path(run_accepted(second));
    assert_eq!(first_destination, final_dir.join("Same.mkv"));
    assert_eq!(second_destination, final_dir.join("Same (2).mkv"));
    assert_eq!(
        std::fs::read(&second_destination).unwrap(),
        b"new process save"
    );

    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(first_destination).unwrap(),
        b"old accepted save"
    );
    assert_eq!(
        std::fs::read(second_destination).unwrap(),
        b"new process save"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn preexisting_recording_is_preserved_and_save_uses_numbered_name() {
    let dir = temp_dir("preexisting-name");
    let temp = dir.join("temp").join("new.mkv");
    let final_dir = dir.join("final");
    std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&final_dir).unwrap();
    std::fs::write(&temp, b"new recording").unwrap();
    std::fs::write(final_dir.join("Song.mkv"), b"existing recording").unwrap();

    let path = saved_path(run(save_job(3, temp, final_dir.clone(), "Song")).unwrap());
    assert_eq!(path, final_dir.join("Song (2).mkv"));
    assert_eq!(
        std::fs::read(final_dir.join("Song.mkv")).unwrap(),
        b"existing recording"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"new recording");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn external_collision_between_selection_and_commit_is_never_overwritten() {
    let dir = temp_dir("external-race");
    let temp = dir.join("temp").join("new.mkv");
    let final_dir = dir.join("final");
    std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
    std::fs::write(&temp, b"accepted recording").unwrap();

    let accepted = accept_save(save_job(19, temp, final_dir.clone(), "Race")).unwrap();
    let path = saved_path(durable::run_with_external_collision(
        accepted,
        b"external winner",
    ));
    assert_eq!(path, final_dir.join("Race (2).mkv"));
    assert_eq!(
        std::fs::read(final_dir.join("Race.mkv")).unwrap(),
        b"external winner"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"accepted recording");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unreadable_destination_identity_defers_without_reselecting_or_mutating_destination() {
    let dir = temp_dir("destination-identity-fault");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_root.join("source.mkv");
    let bytes = b"identity-protected destination";
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, bytes).unwrap();
    let accepted = accept_save(save_job(58, source, final_dir.clone(), "Identity Fault")).unwrap();

    let (event, destination) = durable::run_with_destination_identity_fault(accepted).unwrap();
    assert!(matches!(
        event,
        RecorderEvent::SaveDeferred { id: 58, ref error }
            if error.contains("identity read failure")
    ));
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    assert!(!final_dir.join("Identity Fault (2).mkv").exists());
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir_for_test(&temp_root))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count(),
        1,
        "identity uncertainty must retain the exact journal"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn dangling_external_symlink_is_an_occupied_name_not_an_infinite_retry() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("dangling-collision");
    let temp = dir.join("temp").join("new.mkv");
    let final_dir = dir.join("final");
    std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&final_dir).unwrap();
    std::fs::write(&temp, b"accepted recording").unwrap();
    let external = final_dir.join("Race.mkv");
    symlink(final_dir.join("missing-target"), &external).unwrap();

    let path = saved_path(run(save_job(22, temp, final_dir.clone(), "Race")).unwrap());
    assert_eq!(path, final_dir.join("Race (2).mkv"));
    assert!(
        std::fs::symlink_metadata(external)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(path).unwrap(), b"accepted recording");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn legacy_schema_one_destination_is_untrusted_and_recovered_without_clobbering() {
    #[derive(serde::Deserialize)]
    struct LegacyProjection {
        schema: u8,
        token: String,
        destination: Option<PathBuf>,
    }

    let dir = temp_dir("legacy-journal");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let temp = temp_dir.join("legacy.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&temp, b"legacy accepted recording").unwrap();

    let mut accepted = accept_save(save_job(20, temp, final_dir.clone(), "Legacy")).unwrap();
    let legacy_destination = durable::inject_legacy_destination(&mut accepted).unwrap();
    let legacy: LegacyProjection =
        serde_json::from_value(durable::journal_json(&accepted).unwrap()).unwrap();
    assert_eq!(legacy.schema, 1);
    assert_eq!(legacy.token.len(), 32);
    assert_eq!(
        legacy.destination.as_deref(),
        Some(legacy_destination.as_path())
    );
    std::fs::write(&legacy_destination, b"unrelated external file").unwrap();
    drop(accepted);

    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 0, "warnings: {:?}", report.warnings);
    assert_eq!(report.pending, 1, "warnings: {:?}", report.warnings);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("lacks kernel identities")),
        "{:?}",
        report.warnings
    );
    assert_eq!(
        std::fs::read(&legacy_destination).unwrap(),
        b"unrelated external file"
    );
    assert!(!final_dir.join("Legacy (2).mkv").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn concurrent_same_name_saves_are_serialized_and_never_overwrite() {
    const JOBS: usize = 6;
    let dir = temp_dir("concurrent-names");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let barrier = Arc::new(Barrier::new(JOBS));
    let mut threads = Vec::new();

    for id in 1..=JOBS {
        let temp = temp_dir.join(format!("rec-{id}.mkv"));
        std::fs::write(&temp, format!("recording-{id}")).unwrap();
        let accepted = accept_save(save_job(id as u64, temp, final_dir.clone(), "Same")).unwrap();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            saved_path(run_accepted(accepted))
        }));
    }

    let mut paths = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), JOBS);
    assert!(final_dir.join("Same.mkv").exists());
    for n in 2..=JOBS {
        assert!(final_dir.join(format!("Same ({n}).mkv")).exists());
    }
    let mut contents = paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>();
    contents.sort();
    assert_eq!(
        contents,
        (1..=JOBS)
            .map(|id| format!("recording-{id}"))
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn post_install_sync_failure_is_success_with_complete_destination() {
    let dir = temp_dir("post-rename-sync");
    let source = dir.join("source.mkv");
    let destination = dir.join("final").join("Song.mkv");
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&source, b"complete before rename").unwrap();

    let outcome = durable::install_with_sync_fault(&source, &destination).unwrap();
    assert!(durable::is_committed_sync_failure(&outcome));
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"complete before rename"
    );
    #[cfg(not(windows))]
    assert!(
        destination
            .parent()
            .unwrap()
            .join(".ytt-recorder-work/00000000000000000000000000000000.media.mkv")
            .exists(),
        "the durable stage link remains until destination-parent sync can be retried"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn destination_parent_is_synced_before_stage_link_is_unlinked_and_synced() {
    let dir = temp_dir("install-order");
    let source = dir.join("source.mkv");
    let destination = dir.join("final").join("Song.mkv");
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&source, b"ordered durable install").unwrap();

    let trace = durable::install_order_trace(&source, &destination).unwrap();
    assert_eq!(
        trace,
        [
            "sync_destination_parent",
            "unlink_stage",
            "sync_stage_parent"
        ]
    );
    assert_eq!(
        std::fs::read(destination).unwrap(),
        b"ordered durable install"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stage_parent_sync_fault_still_reports_committed_destination() {
    let dir = temp_dir("stage-sync-fault");
    let source = dir.join("source.mkv");
    let destination = dir.join("final").join("Song.mkv");
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(&source, b"destination already durable").unwrap();

    let (outcome, trace) = durable::install_with_stage_sync_fault(&source, &destination).unwrap();
    assert!(durable::is_committed_sync_failure(&outcome));
    assert_eq!(
        trace,
        [
            "sync_destination_parent",
            "unlink_stage",
            "sync_stage_parent_failed"
        ]
    );
    assert_eq!(
        std::fs::read(destination).unwrap(),
        b"destination already durable"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn read_dir_entry_error_aborts_all_ordinary_temp_cleanup() {
    let dir = temp_dir("entry-error");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let ordinary = temp_dir.join("ordinary.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&ordinary, b"must survive ambiguous enumeration").unwrap();

    let report = durable::cleanup_with_injected_journal_entry_error(&temp_dir, &final_dir);
    assert_eq!(report.discarded_temps, 0);
    assert!(report.admission_uncertain);
    assert!(report.capacity_blocked());
    assert!(ordinary.exists());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("injected read_dir entry failure"))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn save_job_reports_missing_source_without_leaving_recovery_intent() {
    let dir = temp_dir("missing-source");
    let temp_dir = dir.join("temp");
    let final_dir = dir.join("final");
    let missing = temp_dir.join("missing.mkv");

    let event = run(save_job(9, missing, final_dir.clone(), "Missing")).unwrap();
    match event {
        RecorderEvent::SaveFailed {
            id,
            error,
            automatic,
        } => {
            assert_eq!(id, 9);
            assert!(!automatic);
            assert!(!error.is_empty());
            assert!(!error.contains('\n'));
        }
        RecorderEvent::Saved { final_path, .. } => {
            panic!(
                "missing source unexpectedly saved to {}",
                final_path.display()
            )
        }
        RecorderEvent::SaveDeferred { error, .. } => {
            panic!("missing source unexpectedly deferred: {error}")
        }
        RecorderEvent::AlreadySettled { id, .. } => {
            panic!("missing source unexpectedly settled as accepted intent {id}")
        }
        RecorderEvent::CapacityBlocked { .. } => {
            panic!("manual save unexpectedly hit automatic spool capacity")
        }
        RecorderEvent::SaveAccepted { .. } | RecorderEvent::CapacityProbed { .. } => {
            panic!("save unexpectedly returned a recorder control event")
        }
        RecorderEvent::TransitionResolved { .. } => {
            panic!("save unexpectedly returned a transition outcome")
        }
    }
    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 0);
    assert_eq!(report.pending, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn zero_byte_closed_source_is_retry_safe_and_never_journaled() {
    let dir = temp_dir("empty-source");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_root.join("empty.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, []).unwrap();

    let event = run(save_job(69, source.clone(), final_dir.clone(), "Empty")).unwrap();
    assert!(matches!(
        event,
        RecorderEvent::SaveFailed {
            id: 69,
            automatic: false,
            ..
        }
    ));
    assert!(source.exists());
    let report = recover_pending(&temp_root, &final_dir);
    assert_eq!(report.pending, 0);
    assert_eq!(report.recovered, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn final_property_ack_includes_more_than_two_seconds_of_continuous_writes() {
    let dir = temp_dir("continuous-until-ack");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_root.join("live.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, []).unwrap();
    let barrier = crate::recorder::barrier::CommandBarrier::pending();
    let signal = barrier.signal();
    let accepted = accept_save(with_barrier(
        save_job(70, source.clone(), final_dir, "Continuous"),
        barrier,
    ))
    .unwrap();
    let saver = std::thread::spawn(move || run_accepted(accepted));

    let mut expected = Vec::new();
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap();
    for byte in 0..45u8 {
        let chunk = [byte; 32];
        writer.write_all(&chunk).unwrap();
        writer.flush().unwrap();
        expected.extend_from_slice(&chunk);
        std::thread::sleep(Duration::from_millis(50));
    }
    writer.sync_all().unwrap();
    drop(writer);
    signal.succeed();

    let saved = saved_path(saver.join().unwrap());
    assert_eq!(std::fs::read(saved).unwrap(), expected);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn final_property_ack_includes_append_after_a_long_pause() {
    let dir = temp_dir("paused-append-until-ack");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_root.join("paused.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, b"before-pause").unwrap();
    let barrier = crate::recorder::barrier::CommandBarrier::pending();
    let signal = barrier.signal();
    let accepted = accept_save(with_barrier(
        save_job(71, source.clone(), final_dir, "Paused"),
        barrier,
    ))
    .unwrap();
    let saver = std::thread::spawn(move || run_accepted(accepted));

    std::thread::sleep(Duration::from_millis(250));
    let mut writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap();
    writer.write_all(b"-after-pause").unwrap();
    writer.sync_all().unwrap();
    drop(writer);
    signal.succeed();

    let saved = saved_path(saver.join().unwrap());
    assert_eq!(std::fs::read(saved).unwrap(), b"before-pause-after-pause");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn command_failure_and_timeout_keep_source_and_journal_for_recovery() {
    for (name, timeout) in [("failure", false), ("timeout", true)] {
        let dir = temp_dir(name);
        let temp_root = dir.join("temp");
        let final_dir = dir.join("final");
        let source = temp_root.join("retained.mkv");
        std::fs::create_dir_all(&temp_root).unwrap();
        std::fs::write(&source, b"retained bytes").unwrap();
        let barrier = crate::recorder::barrier::CommandBarrier::pending();
        let signal = barrier.signal();
        let accepted = accept_save(with_barrier(
            save_job(72, source.clone(), final_dir, "Retained"),
            barrier,
        ))
        .unwrap();
        let event = if timeout {
            let event = durable::run_with_ack_timeout(accepted, Duration::from_millis(1));
            drop(signal);
            event
        } else {
            signal.fail("injected stream-record failure");
            run_accepted(accepted)
        };
        assert!(matches!(event, RecorderEvent::SaveDeferred { id: 72, .. }));
        assert!(source.exists());
        assert_eq!(
            std::fs::read_dir(crate::recorder::ownership::pending_dir(&temp_root))
                .unwrap()
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn discard_waits_for_ack_and_retains_source_on_failure() {
    let dir = temp_dir("discard-ack");
    let source = dir.join("source.mkv");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&source, b"still open").unwrap();
    let barrier = crate::recorder::barrier::CommandBarrier::pending();
    let signal = barrier.signal();
    let worker_source = source.clone();
    let worker = std::thread::spawn(move || {
        run(RecorderJob::Discard {
            temp: worker_source,
            close_barrier: Some(barrier),
        })
    });
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        source.exists(),
        "discard must wait for the correlated reply"
    );
    signal.fail("injected clear failure");
    assert!(worker.join().unwrap().is_none());
    assert!(source.exists(), "failed clear must retain the source");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn automatic_spool_enforces_count_and_byte_limits_then_frees_capacity() {
    let dir = temp_dir("spool-limits");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    std::fs::create_dir_all(&temp_root).unwrap();
    let first_source = temp_root.join("first.mkv");
    let second_source = temp_root.join("second.mkv");
    std::fs::write(&first_source, b"1234").unwrap();
    std::fs::write(&second_source, b"5678").unwrap();

    let first = durable::accept_with_limits(
        automatic(save_job(80, first_source, final_dir.clone(), "First")),
        1,
        6,
    )
    .unwrap();
    let blocked = match durable::accept_with_limits(
        automatic(save_job(
            81,
            second_source.clone(),
            final_dir.clone(),
            "Second",
        )),
        1,
        6,
    ) {
        Err(event) => event,
        Ok(_) => panic!("second automatic save must exceed both test ceilings"),
    };
    assert!(matches!(
        blocked,
        RecorderEvent::CapacityBlocked {
            id: 81,
            pending_count: 2,
            pending_bytes: 8
        }
    ));
    assert!(second_source.exists());

    assert!(matches!(run_accepted(first), RecorderEvent::Saved { .. }));
    let next = durable::accept_with_limits(
        automatic(save_job(81, second_source, final_dir, "Second")),
        1,
        6,
    );
    assert!(next.is_ok(), "settlement must free spool admission");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn live_owner_is_protected_and_unjournaled_stale_audio_is_quarantined() {
    let dir = temp_dir("owner-leases");
    let temp_root = dir.join("recordings");
    let final_dir = dir.join("final");
    let mut live = crate::recorder::RecorderState {
        temp_dir: temp_root.clone(),
        ..Default::default()
    };
    live.ensure_owner_active().unwrap();
    let (_, live_source) = live.next_temp("mkv");
    std::fs::write(&live_source, b"live process bytes").unwrap();

    let stale =
        crate::recorder::ownership::owners_dir(&temp_root).join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    crate::util::safe_fs::ensure_private_dir_durable(&stale).unwrap();
    std::fs::write(stale.join("lease.lock"), b"").unwrap();
    std::fs::write(stale.join("rec-1.mkv"), b"stale bytes").unwrap();

    let report = recover_pending(&temp_root, &final_dir);
    assert!(report.admission_uncertain);
    assert!(report.capacity_blocked());
    assert_eq!(report.pending, 1);
    assert_eq!(report.pending_bytes, b"stale bytes".len() as u64);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains(&stale.join("rec-1.mkv").display().to_string())),
        "{:?}",
        report.warnings
    );
    assert!(
        live_source.exists(),
        "live namespace must be lease-protected"
    );
    assert!(
        stale.exists(),
        "unknown nonempty source must be quarantined"
    );

    let automatic_result = accept_save(automatic(with_temp_root(
        save_job(86, live_source.clone(), final_dir.clone(), "Automatic"),
        temp_root.clone(),
    )));
    assert!(matches!(
        automatic_result,
        Err(RecorderEvent::CapacityBlocked { id: 86, .. })
    ));
    let manual = accept_save(with_temp_root(
        save_job(87, live_source, final_dir, "Manual"),
        temp_root,
    ));
    assert!(
        manual.is_ok(),
        "explicit manual Save remains admissible while automatic admission is fail-closed"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn successful_owner_save_removes_source_before_journal_and_never_quarantines() {
    let dir = temp_dir("owner-success-cleanup");
    let temp_root = dir.join("recordings");
    let final_dir = dir.join("final");
    let mut owner = crate::recorder::RecorderState {
        temp_dir: temp_root.clone(),
        ..Default::default()
    };
    owner.ensure_owner_active().unwrap();
    let (_, source) = owner.next_temp("mkv");
    std::fs::write(&source, b"complete owner bytes").unwrap();

    let accepted = accept_save(with_temp_root(
        save_job(82, source.clone(), final_dir.clone(), "Complete"),
        temp_root.clone(),
    ))
    .unwrap();
    assert!(matches!(
        run_accepted(accepted),
        RecorderEvent::Saved { .. }
    ));
    assert!(
        !source.exists(),
        "source must be durably retired before journal"
    );
    drop(owner);

    let report = recover_pending(&temp_root, &final_dir);
    assert!(!report.admission_uncertain, "{:?}", report.warnings);
    assert_eq!(report.pending, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn source_cleanup_failure_keeps_source_and_journal_recovery_owned() {
    let dir = temp_dir("source-cleanup-fault");
    let temp_root = dir.join("recordings");
    let final_dir = dir.join("final");
    let source = temp_root.join("source.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, b"recovery ownership bytes").unwrap();
    let accepted = accept_save(save_job(85, source.clone(), final_dir, "Owned")).unwrap();

    let warning = durable::settle_with_source_cleanup_fault(&accepted)
        .unwrap()
        .expect("cleanup fault is an observable recovery-owned warning");
    assert!(warning.contains("source cleanup failed"));
    assert!(source.exists());
    assert!(durable::journal_exists(&accepted));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unpublished_shutdown_save_is_quarantined_on_next_startup() {
    let dir = temp_dir("shutdown-unpublished-quarantine");
    let temp_root = dir.join("recordings");
    let final_dir = dir.join("final");
    let mut owner = crate::recorder::RecorderState {
        temp_dir: temp_root.clone(),
        ..Default::default()
    };
    owner.ensure_owner_active().unwrap();
    let (_, source) = owner.next_temp("mkv");
    std::fs::write(&source, b"must survive journal ENOSPC").unwrap();

    let result = durable::accept_with_unpublished_would_block(bypass_limits(with_temp_root(
        save_job(83, source.clone(), final_dir.clone(), "Quarantine"),
        temp_root.clone(),
    )));
    assert!(matches!(
        result,
        Err(RecorderEvent::SaveFailed { id: 83, .. })
    ));
    drop(owner);

    let report = recover_pending(&temp_root, &final_dir);
    assert!(
        source.exists(),
        "unjournaled source must survive stale cleanup"
    );
    assert!(report.admission_uncertain);
    assert!(report.capacity_blocked());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains(&source.display().to_string()))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn atomic_journal_temporaries_are_reaped_but_busy_targets_are_preserved() {
    let dir = temp_dir("journal-temp-cleanup");
    let temp_root = dir.join("recordings");
    let final_dir = dir.join("final");
    let pending = crate::recorder::ownership::pending_dir_for_test(&temp_root);
    std::fs::create_dir_all(&pending).unwrap();

    let mut abandoned = Vec::new();
    for index in 0..24u64 {
        let path = pending.join(format!(".{:032x}.json.tmp.999.{index:016x}", index + 1));
        std::fs::write(&path, b"cutoff").unwrap();
        abandoned.push(path);
    }
    let report = durable::cleanup_atomic_temps_for_test(&temp_root, &final_dir);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(abandoned.iter().all(|path| !path.exists()));

    let source = temp_root.join("busy.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, b"busy journal bytes").unwrap();
    let accepted = accept_save(save_job(84, source, final_dir.clone(), "Busy")).unwrap();
    let journal = durable::journal_path(&accepted);
    let busy_temp = journal.with_file_name(format!(
        ".{}.tmp.999.0123456789abcdef",
        journal.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&busy_temp, b"active rewrite").unwrap();
    let save_lock = durable::hold_save_lock_for_test(&final_dir).unwrap();
    let report = durable::cleanup_atomic_temps_for_test(&temp_root, &final_dir);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(
        busy_temp.exists(),
        "busy worker temporary must remain fenced"
    );
    drop(save_lock);
    durable::cleanup_atomic_temps_for_test(&temp_root, &final_dir);
    assert!(!busy_temp.exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn new_layout_recovery_waits_for_live_owner_then_uses_original_destination() {
    let dir = temp_dir("new-layout-config-change");
    let temp_root = dir.join("recordings");
    let old_final = dir.join("old-final");
    let new_final = dir.join("new-final");
    let mut owner = crate::recorder::RecorderState {
        temp_dir: temp_root.clone(),
        ..Default::default()
    };
    owner.ensure_owner_active().unwrap();
    let (_, source) = owner.next_temp("mkv");
    std::fs::write(&source, b"stable owner source").unwrap();
    let accepted = accept_save(with_temp_root(
        save_job(90, source, old_final.clone(), "Stable"),
        temp_root.clone(),
    ))
    .unwrap();
    drop(accepted);

    let live_report = recover_pending(&temp_root, &new_final);
    assert_eq!(live_report.recovered, 0);
    assert_eq!(live_report.pending, 1);
    assert!(!old_final.join("Stable.mkv").exists());
    drop(owner);

    let recovered = recover_pending(&temp_root, &new_final);
    assert_eq!(recovered.recovered, 1, "{:?}", recovered.warnings);
    assert_eq!(
        std::fs::read(old_final.join("Stable.mkv")).unwrap(),
        b"stable owner source"
    );
    assert!(!new_final.join("Stable.mkv").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn visible_journal_with_two_sync_failures_is_deferred_and_recovers_once() {
    let dir = temp_dir("double-sync-failure");
    let temp_root = dir.join("temp");
    let final_dir = dir.join("final");
    let source = temp_root.join("source.mkv");
    std::fs::create_dir_all(&temp_root).unwrap();
    std::fs::write(&source, b"ambiguous durable publication").unwrap();

    let (result, trace) =
        durable::accept_with_double_sync_fault(save_job(91, source, final_dir.clone(), "Twice"));
    assert_eq!(trace, ["journal_visible", "parent_resync_failed"]);
    assert!(matches!(
        result,
        Err(RecorderEvent::SaveDeferred { id: 91, .. })
    ));
    let report = recover_pending(&temp_root, &final_dir);
    assert_eq!(report.recovered, 1, "{:?}", report.warnings);
    assert_eq!(
        std::fs::read(final_dir.join("Twice.mkv")).unwrap(),
        b"ambiguous durable publication"
    );
    let second = recover_pending(&temp_root, &final_dir);
    assert_eq!(second.recovered, 0);
    assert!(!final_dir.join("Twice (2).mkv").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tag_write_failure_does_not_fail_the_audio_save() {
    let dir = temp_dir("tag-failure");
    let temp = dir.join("temp").join("rec-1.mp3");
    let final_dir = dir.join("final");
    std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
    std::fs::write(&temp, b"not actually audio").unwrap();

    let event = run(RecorderJob::Save {
        id: 10,
        temp: temp.clone(),
        temp_dir: temp.parent().unwrap().to_path_buf(),
        final_dir: final_dir.clone(),
        filename: "Tagged".to_owned(),
        ext: "mp3",
        title: Some("Title".to_owned()),
        artist: Some("Artist".to_owned()),
        station: Some("Station".to_owned()),
        close_barrier: None,
        automatic: false,
        bypass_limits: false,
    })
    .unwrap();
    let path = saved_path(event);
    assert_eq!(path, final_dir.join("Tagged.mp3"));
    assert_eq!(std::fs::read(path).unwrap(), b"not actually audio");
    let _ = std::fs::remove_dir_all(dir);
}
