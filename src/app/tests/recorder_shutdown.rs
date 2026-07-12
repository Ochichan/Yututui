use super::*;

fn recorder_scratch(name: &str) -> std::path::PathBuf {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-app-recorder-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn snapshot_recorder_destination(
    root: &std::path::Path,
) -> Vec<(std::path::PathBuf, bool, Vec<u8>)> {
    fn walk(
        root: &std::path::Path,
        directory: &std::path::Path,
        snapshot: &mut Vec<(std::path::PathBuf, bool, Vec<u8>)>,
    ) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                snapshot.push((relative, true, Vec::new()));
                walk(root, &path, snapshot);
            } else {
                snapshot.push((relative, false, std::fs::read(path).unwrap()));
            }
        }
    }

    let mut snapshot = Vec::new();
    walk(root, root, &mut snapshot);
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

fn admit_recorder_transition_without_replies(app: &mut App, cmds: Vec<Cmd>) {
    let mut intent = cmds
        .into_iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(intent),
            _ => None,
        })
        .expect("recorder player intent");
    let _commands = std::mem::take(&mut intent.commands);
    let _wait = crate::runtime::player_delivery::settle_player_intent(
        app,
        *intent,
        Ok(crate::util::delivery::DeliveryReceipt::Enqueued),
    );
}

#[test]
fn shutdown_reissues_only_unaccepted_save_with_its_exact_snapshot() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let requested = feed_title(&mut app, "C - Three");
    let (id, final_dir, automatic, bypass_limits) = requested
        .iter()
        .find_map(|effect| match effect {
            Cmd::Recorder(RecorderJob::Save {
                id,
                final_dir,
                automatic,
                bypass_limits,
                ..
            }) => Some((*id, final_dir.clone(), *automatic, *bypass_limits)),
            _ => None,
        })
        .expect("boundary emits an unaccepted Save");
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::SaveRequested
    );
    app.config.recording.track_directory = Some("/tmp/changed-after-reducer".into());

    let shutdown = app.settle_recorder_owner_shutdown();
    assert!(shutdown.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::Save {
            id: retry_id,
            final_dir: retry_dir,
            automatic: retry_automatic,
            bypass_limits: retry_bypass,
            ..
        }) if *retry_id == id
            && retry_dir == &final_dir
            && *retry_automatic == automatic
            && *retry_bypass == bypass_limits
    )));

    app.on_recorder_event(RecorderEvent::SaveAccepted { id });
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::SavePending
    );
    assert!(
        app.settle_recorder_owner_shutdown()
            .iter()
            .all(|effect| !matches!(
                effect,
                Cmd::Recorder(RecorderJob::Save { id: retry_id, .. }) if *retry_id == id
            ))
    );
}

#[test]
fn shutdown_accepts_existing_request_but_defers_copy_to_recovery() {
    use crate::recorder::job::{
        RecorderEvent, RecorderJob, accept_save, recover_pending, run_accepted,
    };

    let root = recorder_scratch("shutdown-request-deferral");
    let temp_dir = root.join("temp");
    let final_dir = root.join("final");
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    app.recorder.temp_dir = temp_dir.clone();
    app.config.recording.track_directory = Some(final_dir.clone());
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let track = app.recorder.history.front().unwrap();
    let id = track.id;
    let source = track.temp_path.clone();
    let ext = track.ext;
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"defer these complete recording bytes").unwrap();

    let save = app
        .settle_recorder_owner_shutdown()
        .into_iter()
        .find_map(|effect| match effect {
            Cmd::Recorder(job @ RecorderJob::Save { id: save_id, .. }) if save_id == id => {
                Some(job)
            }
            _ => None,
        })
        .expect("shutdown reissues the existing request");
    let accepted = accept_save(save).expect("shutdown publishes durable recovery ownership");
    let destination_before_run = snapshot_recorder_destination(&final_dir);

    assert!(matches!(
        run_accepted(accepted),
        RecorderEvent::SaveDeferred {
            id: deferred_id,
            ..
        } if deferred_id == id
    ));
    assert!(
        source.exists(),
        "shutdown execution fence retains the source"
    );
    assert_eq!(
        snapshot_recorder_destination(&final_dir),
        destination_before_run,
        "shutdown must not change the accepted intent's private work area or begin destination copy/tag work"
    );
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir_for_test(&temp_dir))
            .unwrap()
            .count(),
        1,
        "the accepted journal remains the sole recovery owner"
    );
    drop(app);

    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 1, "{:?}", report.warnings);
    assert!(!report.admission_uncertain, "{:?}", report.warnings);
    assert!(!source.exists());
    assert!(final_dir.join(format!("B - Two.{ext}")).exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn deferred_save_crosses_acceptance_state_and_shutdown_never_duplicates_it() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let id = app.recorder.history.front().unwrap().id;
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::SaveRequested
    );

    app.on_recorder_event(RecorderEvent::SaveDeferred {
        id,
        error: "injected post-acceptance deferral".to_owned(),
    });

    let track = app.recorder.history.front().unwrap();
    assert_eq!(track.state, RecordingState::SavePending);
    assert!(track.save_request.is_none());
    assert!(
        app.settle_recorder_owner_shutdown()
            .iter()
            .all(|effect| !matches!(
                effect,
                Cmd::Recorder(RecorderJob::Save { id: retry_id, .. }) if *retry_id == id
            )),
        "a deferred accepted journal is the sole retry owner"
    );
}

#[test]
fn shutdown_pending_boundary_emits_each_automatic_save_once() {
    use crate::recorder::job::RecorderJob;

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let boundary = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "C - Three" }),
    ));
    admit_recorder_transition_without_replies(&mut app, boundary);
    assert!(app.recorder.pending_transition.is_some());
    let outgoing_id = app.recorder.current.as_ref().unwrap().id;

    let shutdown = app.settle_recorder_owner_shutdown();

    assert_eq!(
        shutdown
            .iter()
            .filter(|effect| matches!(
                effect,
                Cmd::Recorder(RecorderJob::Save { id, .. }) if *id == outgoing_id
            ))
            .count(),
        1,
        "the newly finalized Save and the shutdown reissue scan must not duplicate one intent"
    );
}

#[test]
fn shutdown_reissues_an_automatic_retry_that_has_not_reached_acceptance() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let owner_id = app.recorder.history.front().unwrap().id;
    app.on_recorder_event(RecorderEvent::CapacityBlocked {
        id: owner_id,
        pending_count: 128,
        pending_bytes: 1024,
    });
    let retry = app.on_recorder_event(RecorderEvent::Saved {
        id: 900,
        final_path: "/tmp/drained.mkv".into(),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    let expected_dir = retry
        .iter()
        .find_map(|effect| match effect {
            Cmd::Recorder(RecorderJob::Save { id, final_dir, .. }) if *id == owner_id => {
                Some(final_dir.clone())
            }
            _ => None,
        })
        .expect("capacity hint emits the exact automatic retry");
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::AutomaticSaveRetrying
    );

    let shutdown = app.settle_recorder_owner_shutdown();
    let shutdown_barrier = shutdown
        .iter()
        .find_map(|effect| match effect {
            Cmd::Recorder(RecorderJob::Save {
                id,
                final_dir,
                close_barrier,
                automatic: true,
                bypass_limits: false,
                ..
            }) if *id == owner_id && final_dir == &expected_dir => close_barrier.clone(),
            _ => None,
        })
        .expect("shutdown reissues the automatic retry behind an execution fence");
    assert!(
        shutdown_barrier
            .wait()
            .unwrap_err()
            .contains("deferred until startup")
    );
}

#[test]
fn orderly_shutdown_reclaims_active_and_decide_history_without_quarantine() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::{RecorderJob, recover_pending, run};

    let root = recorder_scratch("orderly-shutdown-cleanup");
    let temp_dir = root.join("temp");
    let final_dir = root.join("final");
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    app.recorder.temp_dir = temp_dir.clone();
    app.config.recording.track_directory = Some(final_dir.clone());
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");

    let history_source = app.recorder.history.front().unwrap().temp_path.clone();
    let active_source = app.recorder.current.as_ref().unwrap().temp_path.clone();
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::Recorded
    );
    std::fs::create_dir_all(history_source.parent().unwrap()).unwrap();
    std::fs::write(&history_source, b"completed Decide bytes").unwrap();
    std::fs::write(&active_source, b"incomplete active bytes").unwrap();

    let shutdown = app.settle_recorder_owner_shutdown();
    assert!(app.recorder.current.is_none());
    assert!(app.recorder.history.is_empty());
    let mut discarded = 0;
    for effect in shutdown {
        if let Cmd::Recorder(job @ RecorderJob::Discard { .. }) = effect {
            discarded += 1;
            assert!(run(job).is_none());
        }
    }
    assert_eq!(discarded, 2);
    assert!(!history_source.exists());
    assert!(!active_source.exists());
    drop(app);

    let report = recover_pending(&temp_dir, &final_dir);
    assert!(!report.admission_uncertain, "{:?}", report.warnings);
    assert_eq!(report.pending, 0);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let _ = std::fs::remove_dir_all(root);
}
