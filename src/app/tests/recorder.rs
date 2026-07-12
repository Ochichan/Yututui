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

fn resolve_admitted_recorder_transition(
    app: &mut App,
    follow_ups: Vec<Cmd>,
    player_commands: Vec<crate::player::PlayerCmd>,
    close_ok: bool,
    open_ok: bool,
) -> Vec<Cmd> {
    for command in player_commands {
        if let crate::player::PlayerCmd::TrackedProperty(tracked) = command {
            let succeeds = if tracked.value.as_str() == Some("") {
                close_ok
            } else {
                open_ok
            };
            if succeeds {
                tracked.acknowledgement.succeed();
            } else {
                tracked.acknowledgement.fail("injected mpv rejection");
            }
        }
    }
    let mut effects = Vec::new();
    for follow_up in follow_ups {
        match follow_up {
            Cmd::Recorder(job @ crate::recorder::job::RecorderJob::AwaitTransition { .. }) => {
                let event = crate::recorder::job::run(job).expect("transition event");
                effects.extend(app.update(Msg::Recorder(event)));
            }
            other => effects.push(other),
        }
    }
    effects
}

fn admit_recorder_transition_without_replies(
    app: &mut App,
    cmds: Vec<Cmd>,
) -> (Vec<Cmd>, Vec<crate::player::PlayerCmd>) {
    let mut intent = cmds
        .into_iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(intent),
            _ => None,
        })
        .expect("recorder player intent");
    let commands = std::mem::take(&mut intent.commands);
    let follow_ups = crate::runtime::player_delivery::settle_player_intent(
        app,
        *intent,
        Ok(crate::util::delivery::DeliveryReceipt::Enqueued),
    );
    (follow_ups, commands)
}

#[test]
fn recorder_first_track_is_incomplete_and_dropped() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "Artist A - Track 1");
    let seg = app.recorder.current.as_ref().expect("segment open");
    assert!(
        seg.incomplete,
        "the first track after tuning in is incomplete"
    );
    assert!(app.recorder.saw_first_title);
    assert!(app.recorder.history.is_empty());

    // Second title: the incomplete first track is dropped; a complete second opens.
    feed_title(&mut app, "Artist B - Track 2");
    assert!(
        app.recorder.history.is_empty(),
        "incomplete first stays out of history"
    );
    let seg = app.recorder.current.as_ref().expect("second segment open");
    assert!(!seg.incomplete);
    assert_eq!(seg.raw, "Artist B - Track 2");
}

#[test]
fn rejected_first_recorder_open_does_not_allocate_live_segment_state() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        let temp_seq = app.recorder.temp_seq;
        let cmds = app.update(PlayerMsg::Metadata(
            serde_json::json!({ "icy-title": "Artist A - Track 1" }),
        ));

        assert!(app.recorder.current.is_none());
        assert_eq!(app.recorder.temp_seq, temp_seq);
        assert!(!app.recorder.saw_first_title);
        assert!(app.recorder.history.is_empty());
        assert_eq!(
            cmds.iter().flat_map(Cmd::player_commands).count(),
            1,
            "opening a segment is one atomic player command"
        );

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert!(app.recorder.current.is_none());
        assert_eq!(app.recorder.temp_seq, temp_seq);
        assert!(!app.recorder.saw_first_title);
        assert!(app.recorder.history.is_empty());
    }
}

#[test]
fn recorder_exact_reply_truth_table_is_fail_closed_and_restart_ordered() {
    use crate::recorder::job::RecorderJob;

    for (close_ok, open_ok, should_restart) in [
        (true, true, false),
        (false, true, false),
        (true, false, true),
        (false, false, true),
    ] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        feed_title(&mut app, "A - One");
        feed_title(&mut app, "B - Two");
        backdate_current(&mut app, 60);

        let cmds = app.update(PlayerMsg::Metadata(
            serde_json::json!({ "icy-title": "C - Three" }),
        ));
        let effects =
            app.admit_player_intents_with_recorder_replies_for_test(&cmds, close_ok, open_ok);

        assert_eq!(
            matches!(
                effects.first(),
                Some(Cmd::PlayerControl(PlayerControl::Restart { .. }))
            ),
            should_restart,
            "close={close_ok} open={open_ok}"
        );
        assert_eq!(
            app.recorder
                .current
                .as_ref()
                .map(|segment| segment.raw.as_str()),
            (!should_restart).then_some("C - Three"),
            "close={close_ok} open={open_ok}"
        );
        assert_eq!(app.recorder.history.len(), 1);
        assert_eq!(app.recorder.history[0].raw, "B - Two");
        if should_restart {
            assert!(app.recorder.execution_blocked);
            assert!(
                effects
                    .iter()
                    .skip(1)
                    .any(|effect| matches!(effect, Cmd::Recorder(RecorderJob::Discard { .. })))
            );
        }
    }
}

#[test]
fn initial_open_failure_unblocks_only_for_the_matching_restart_completion() {
    for restart_succeeds in [false, true] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        let cmds = app.update(PlayerMsg::Metadata(
            serde_json::json!({ "icy-title": "A - One" }),
        ));
        let effects = app.admit_player_intents_with_recorder_replies_for_test(&cmds, true, false);

        assert!(matches!(
            effects.first(),
            Some(Cmd::PlayerControl(PlayerControl::Restart { .. }))
        ));
        assert!(app.recorder.current.is_none());
        assert!(app.recorder.execution_blocked);
        app.recorder_player_restart_completed(restart_succeeds);
        assert_eq!(app.recorder.execution_blocked, !restart_succeeds);
    }
}

#[test]
fn failed_close_retires_writer_before_source_acceptance_and_keeps_final_bytes() {
    use std::io::Write as _;

    use crate::recorder::job::{RecorderJob, accept_save, recover_pending};

    let root = recorder_scratch("failed-close-fence");
    let temp_dir = root.join("temp");
    let final_dir = root.join("final");
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    app.recorder.temp_dir = temp_dir.clone();
    app.config.recording.track_directory = Some(final_dir.clone());
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let source = app.recorder.current.as_ref().unwrap().temp_path.clone();
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"bytes before failed close").unwrap();

    let cmds = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "C - Three" }),
    ));
    let mut effects = app.admit_player_intents_with_recorder_replies_for_test(&cmds, false, false);
    assert!(matches!(
        effects.first(),
        Some(Cmd::PlayerControl(PlayerControl::Restart { .. }))
    ));
    let save_position = effects
        .iter()
        .position(|effect| matches!(effect, Cmd::Recorder(RecorderJob::Save { .. })))
        .expect("automatic Save follows restart");
    assert!(save_position > 0);

    // Mutation model: the failed close's writer can append until the runtime drops/reaps its
    // guard. Completing the retirement fence before dispatching the later Save captures all bytes.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b" + final bytes before guard drop")
        .unwrap();
    let _ = app.settle_recorder_player_retired();
    let save = match effects.remove(save_position) {
        Cmd::Recorder(save @ RecorderJob::Save { .. }) => save,
        _ => unreachable!(),
    };
    let accepted = accept_save(save).expect("retired writer source is durably journaled");
    drop(accepted);
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"bytes before failed close + final bytes before guard drop"
    );
    drop(app);

    let report = recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(final_dir.join("B - Two.mkv")).unwrap(),
        b"bytes before failed close + final bytes before guard drop"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn rejected_recorder_rotation_preserves_current_segment_and_history() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        feed_title(&mut app, "A - One");
        feed_title(&mut app, "B - Two");
        backdate_current(&mut app, 60);
        let current = app.recorder.current.as_ref().expect("segment open");
        let current_id = current.id;
        let current_path = current.temp_path.clone();
        let current_raw = current.raw.clone();
        let temp_seq = app.recorder.temp_seq;
        let history_len = app.recorder.history.len();
        let saw_first_title = app.recorder.saw_first_title;

        let cmds = app.update(PlayerMsg::Metadata(
            serde_json::json!({ "icy-title": "C - Three" }),
        ));
        let commands: Vec<_> = cmds.iter().flat_map(Cmd::player_commands).collect();
        assert_eq!(commands.len(), 2, "rotation must be one clear → set batch");
        assert!(commands[0].property().is_some_and(|(name, value)| {
            name == "stream-record" && value == &serde_json::Value::from("")
        }));
        assert!(commands[1].property().is_some_and(|(name, value)| {
            name == "stream-record" && value != &serde_json::Value::from("")
        }));

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        let current = app.recorder.current.as_ref().expect("old segment retained");
        assert_eq!(current.id, current_id);
        assert_eq!(current.temp_path, current_path);
        assert_eq!(current.raw, current_raw);
        assert_eq!(app.recorder.temp_seq, temp_seq);
        assert_eq!(app.recorder.history.len(), history_len);
        assert_eq!(app.recorder.saw_first_title, saw_first_title);
    }
}

#[test]
fn recorder_keeps_a_long_enough_track_on_the_next_boundary() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One"); // incomplete first
    feed_title(&mut app, "B - Two"); // opens complete "Two"
    backdate_current(&mut app, 60); // pretend "Two" played a full minute
    feed_title(&mut app, "C - Three"); // finalize "Two" -> kept
    assert_eq!(app.recorder.history.len(), 1);
    let t = &app.recorder.history[0];
    assert_eq!(t.raw, "B - Two");
    assert_eq!(t.title.as_deref(), Some("Two"));
    assert_eq!(t.artist.as_deref(), Some("B"));
    assert!(matches!(t.state, crate::recorder::RecordingState::Recorded));
}

#[test]
fn recorder_drops_a_track_below_min_duration() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two"); // complete, but ~0s long
    feed_title(&mut app, "C - Three"); // finalize "Two": below min -> dropped
    assert!(app.recorder.history.is_empty());
}

#[test]
fn repeated_max_duration_splits_never_dedupe_earlier_audio_chunks() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::RecorderJob;

    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    app.config.recording.max_duration_secs = 60;
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Long");

    backdate_current(&mut app, 60);
    let mut first_split = app.recorder_on_tick();
    admit_player_transition(&mut app, &mut first_split);
    assert_eq!(app.recorder.history.len(), 1);
    assert_eq!(
        app.recorder.history[0].state,
        RecordingState::RecordedReachedMaxDuration
    );
    let first_source = app.recorder.history[0].temp_path.clone();

    backdate_current(&mut app, 60);
    let mut second_split = app.recorder_on_tick();
    admit_player_transition(&mut app, &mut second_split);

    assert_eq!(app.recorder.history.len(), 2);
    assert!(app.recorder.history.iter().all(|track| {
        track.raw == "B - Long" && track.state == RecordingState::RecordedReachedMaxDuration
    }));
    assert!(second_split.iter().all(|effect| {
        !matches!(
            effect,
            Cmd::Recorder(RecorderJob::Discard { temp, .. }) if temp == &first_source
        )
    }));
}

#[test]
fn recorder_everything_mode_auto_saves_kept_tracks() {
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let cmds = feed_title(&mut app, "C - Three");
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::Recorder(crate::recorder::job::RecorderJob::Save { .. })
        )),
        "a save job is emitted for the kept track"
    );
    assert_eq!(app.recorder.history.len(), 1);
    assert!(matches!(
        app.recorder.history[0].state,
        crate::recorder::RecordingState::SaveRequested
    ));
}

#[test]
fn duplicate_save_input_is_visibly_rejected_before_and_after_acceptance() {
    let _guard = crate::i18n::lock_for_test();
    use crate::recorder::job::RecorderEvent;

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let id = app.recorder.history.front().unwrap().id;

    assert!(app.recorder_save(id).is_empty());
    assert!(app.status.text.contains("requested"));
    app.on_recorder_event(RecorderEvent::SaveAccepted { id });
    assert!(app.recorder_save(id).is_empty());
    assert!(app.status.text.contains("accepted"));
}

#[test]
fn automatic_spool_block_is_visible_stops_opens_and_preserves_pending_history_sources() {
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    app.config.recording.past_tracks_count = 1;
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let first = feed_title(&mut app, "C - Three");
    let first_source = app.recorder.history.front().unwrap().temp_path.clone();
    let first_id = app.recorder.history.front().unwrap().id;
    assert!(
        first
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Recorder(RecorderJob::Save { .. })))
    );

    backdate_current(&mut app, 60);
    let second = feed_title(&mut app, "D - Four");
    assert_eq!(
        app.recorder.history.len(),
        2,
        "accepted/pending ownership rows may temporarily exceed the presentation cap"
    );
    assert!(
        second.iter().all(|cmd| {
            !matches!(
                cmd,
                Cmd::Recorder(RecorderJob::Discard { temp, .. }) if temp == &first_source
            )
        }),
        "eviction must not delete a source owned by an accepted/deferred Save"
    );
    let blocked_id = app.recorder.history.front().unwrap().id;
    assert_ne!(blocked_id, first_id);

    let mut teardown = app.on_recorder_event(RecorderEvent::CapacityBlocked {
        id: blocked_id,
        pending_count: 129,
        pending_bytes: 8 * 1024 * 1024 * 1024,
    });
    assert!(app.recorder.capacity_blocked);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("129"));
    assert!(emits_stream_record_clear(&teardown));
    admit_player_transition(&mut app, &mut teardown);
    assert!(app.recorder.current.is_none());

    let no_open = feed_title(&mut app, "E - Five");
    assert!(no_open.is_empty());
    assert!(app.recorder.current.is_none());

    let retry = app.on_recorder_event(RecorderEvent::Saved {
        id: first_id,
        final_path: std::path::PathBuf::from("/tmp/drained.mkv"),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    assert!(app.recorder.capacity_blocked);
    assert_eq!(app.recorder.capacity_retry_id, Some(blocked_id));
    assert!(
        retry.iter().any(|effect| matches!(
            effect,
            Cmd::Recorder(RecorderJob::Save { id, automatic: true, .. }) if *id == blocked_id
        )),
        "capacity first re-issues the exact rejected automatic Save"
    );
    assert!(matches!(
        app.recorder.history.front().unwrap().state,
        crate::recorder::RecordingState::AutomaticSaveRetrying
    ));
    assert!(
        app.recorder_discard(blocked_id).is_empty(),
        "an in-flight exact retry keeps its source ownership row"
    );
    assert!(app.status.text.contains("already accepted"));
    assert_eq!(app.recorder.history.len(), 2);

    let blocked_again = app.on_recorder_event(RecorderEvent::CapacityBlocked {
        id: blocked_id,
        pending_count: 129,
        pending_bytes: 8 * 1024 * 1024 * 1024,
    });
    assert!(blocked_again.is_empty());
    assert_eq!(app.recorder.capacity_retry_id, None);
    assert!(app.recorder.capacity_blocked);

    let retry_again = app.on_recorder_event(RecorderEvent::AlreadySettled {
        id: first_id,
        capacity_available: true,
    });
    assert!(matches!(
        retry_again.as_slice(),
        [Cmd::Recorder(RecorderJob::Save { id, automatic: true, .. })] if *id == blocked_id
    ));
    assert!(app.recorder.capacity_blocked);

    let mut resumed = app.on_recorder_event(RecorderEvent::Saved {
        id: blocked_id,
        final_path: std::path::PathBuf::from("/tmp/retried.mkv"),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    assert!(!app.recorder.capacity_blocked);
    assert_eq!(app.recorder.capacity_retry_id, None);
    assert!(
        resumed
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| {
                command.property().is_some_and(|(name, value)| {
                    name == "stream-record" && value != &serde_json::Value::from("")
                })
            }),
        "only the exact retry terminal result reopens against the latest metadata"
    );
    admit_player_transition(&mut app, &mut resumed);
    let segment = app.recorder.current.as_ref().expect("recording resumed");
    assert_eq!(segment.raw, "E - Five");
    assert!(segment.incomplete, "capacity-gap segment joined mid-song");
}

#[test]
fn automatic_preaccept_failure_pauses_and_exposes_the_protected_source() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::RecorderEvent;

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let track = app.recorder.history.front().unwrap();
    let id = track.id;
    let source = track.temp_path.clone();

    let mut teardown = app.on_recorder_event(RecorderEvent::SaveFailed {
        id,
        error: "injected source fsync failure".to_owned(),
        automatic: true,
    });

    assert!(app.recorder.capacity_blocked);
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::AutomaticSaveBlocked
    );
    assert!(app.status.text.contains(&source.display().to_string()));
    assert_eq!(
        app.recorder.health_warning.as_deref(),
        Some(app.status.text.as_str())
    );
    assert!(emits_stream_record_clear(&teardown));
    admit_player_transition(&mut app, &mut teardown);
    assert!(app.recorder.current.is_none());
    assert!(feed_title(&mut app, "D - Four").is_empty());
    assert_eq!(app.recorder.history.front().unwrap().temp_path, source);
}

#[test]
fn explicit_discard_of_an_unaccepted_blocked_save_drains_the_pause() {
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    feed_title(&mut app, "C - Three");
    let blocked_id = app.recorder.history.front().unwrap().id;
    let blocked_source = app.recorder.history.front().unwrap().temp_path.clone();
    let mut teardown = app.on_recorder_event(RecorderEvent::CapacityBlocked {
        id: blocked_id,
        pending_count: 128,
        pending_bytes: 1024,
    });
    admit_player_transition(&mut app, &mut teardown);

    let effects = app.recorder_discard(blocked_id);

    assert!(!app.recorder.capacity_blocked);
    assert!(app.recorder.health_warning.is_none());
    assert!(
        !app.recorder
            .history
            .iter()
            .any(|track| track.id == blocked_id)
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::Discard { temp, .. }) if temp == &blocked_source
    )));
    assert!(
        effects
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| {
                command.property().is_some_and(|(name, value)| {
                    name == "stream-record" && value != &serde_json::Value::from("")
                })
            }),
        "discarding the sole unaccepted blocked source immediately reconciles recording"
    );
}

#[test]
fn capacity_backpressure_blocks_everything_but_not_decide_mode() {
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    app.recorder.capacity_blocked = true;
    app.recorder.health_warning = Some("recorder spool full".to_owned());

    assert!(feed_title(&mut app, "A - One").is_empty());
    assert!(app.recorder.current.is_none());

    app.config.recording.mode = crate::recorder::RecordingMode::Decide;
    let mut effects = app.reconcile_recorder();
    assert!(
        effects
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| {
                command.property().is_some_and(|(name, value)| {
                    name == "stream-record" && value != &serde_json::Value::from("")
                })
            })
    );
    admit_player_transition(&mut app, &mut effects);
    assert!(app.recorder.current.is_some());
}

#[test]
fn recorder_health_status_survives_ttl_without_rearming_redraw_ticks() {
    let mut app = App::new(100);
    let warning = "Automatic recording paused: recovery inventory is uncertain".to_owned();
    app.recorder.health_warning = Some(warning.clone());
    app.status.text = warning.clone();
    app.status.kind = StatusKind::Error;
    app.status.set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    app.dirty = false;

    app.update(Msg::StatusTick);

    assert_eq!(app.status.text, warning);
    assert!(
        !app.status_visible(),
        "persistent health disarms the TTL tick"
    );
    assert!(
        !app.dirty,
        "an unchanged persistent warning does not redraw forever"
    );

    app.set_status_info("temporary playback toast");
    app.status.set_at = Some(Instant::now() - STATUS_TTL - Duration::from_millis(1));
    app.update(Msg::StatusTick);
    assert_eq!(app.status.text, warning);
    assert!(!app.status_visible());
}

#[test]
fn reducer_cannot_clear_a_persistent_recorder_health_warning() {
    let mut app = App::new(100);
    let warning = "manual recovery required at /tmp/rec-1.mkv".to_owned();
    app.recorder.health_warning = Some(warning.clone());
    app.recorder.health_sticky = true;
    app.status.text.clear();
    app.status.set_at = None;

    assert!(app.update(Msg::Noop).is_empty());
    assert_eq!(app.status.text, warning);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.set_at.is_none());
}

#[test]
fn recorder_health_status_survives_a_track_load_commit() {
    let mut app = App::new(100);
    app.queue.set(songs(1), 0);
    let warning = "Recording recovery requires attention".to_owned();
    app.recorder.health_warning = Some(warning.clone());
    app.status.text = warning.clone();

    let mut effects = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut effects);

    assert_eq!(app.status.text, warning);
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(!app.status_visible());
}

#[test]
fn recorder_pauses_through_ad_and_resumes_complete() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One"); // incomplete
    feed_title(&mut app, "B - Two"); // complete "Two"
    backdate_current(&mut app, 60);
    // Metadata goes junk (ad / station id) -> parsed None -> finalize "Two", open nothing.
    feed_title(&mut app, "");
    assert!(
        app.recorder.current.is_none(),
        "recording paused during the ad"
    );
    assert_eq!(app.recorder.history.len(), 1, "\"Two\" kept");
    // A later real title opens a *complete* segment (saw_first_title is already set).
    feed_title(&mut app, "D - Four");
    assert!(!app.recorder.current.as_ref().expect("resumed").incomplete);
}

#[test]
fn recorder_teardown_on_stop_drops_in_progress_and_clears_stream_record() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    assert!(app.recorder.current.is_some());
    let mut cmds = app.apply_media(crate::media::MediaCommand::Stop);
    assert!(
        app.recorder.current.is_some(),
        "recorder teardown waits for Stop admission"
    );
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
    assert!(emits_stream_record_clear(&cmds));
}

#[test]
fn media_stop_clears_radio_recording_and_video_pause_latch() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    app.playback.paused = false;
    app.video.paused_audio = true;

    let epoch = app.playback.position_epoch;
    let mut cmds = app.update(Msg::Media(crate::media::MediaCommand::Stop));

    assert!(!app.playback.paused, "Stop waits for player admission");
    assert!(app.prefetch.loaded_video_id.is_some());
    assert!(app.video.paused_audio);
    assert!(app.recorder.current.is_some());
    let commands: Vec<_> = cmds.iter().flat_map(Cmd::player_commands).collect();
    let clear_pos = commands
        .iter()
        .position(|command| {
            command.property().is_some_and(|(name, value)| {
                name == "stream-record" && value == &serde_json::Value::from("")
            })
        })
        .expect("stream-record cleared");
    let stop_pos = commands
        .iter()
        .position(|command| matches!(command, crate::player::PlayerCmd::Stop))
        .expect("mpv stop emitted");
    assert!(
        clear_pos < stop_pos,
        "stream-record must clear before mpv stops"
    );

    admit_player_transition(&mut app, &mut cmds);
    assert!(app.playback.paused);
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(!app.video.paused_audio);
    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
}

#[test]
fn rejected_media_stop_preserves_playback_video_and_recorder_state() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        feed_title(&mut app, "A - One");
        feed_title(&mut app, "B - Two");
        backdate_current(&mut app, 60);
        app.playback.paused = false;
        app.playback.time_pos = Some(23.0);
        app.playback.time_pos_at = Some(std::time::Instant::now());
        app.video.paused_audio = true;
        let epoch = app.playback.position_epoch;
        let loaded = app.prefetch.loaded_video_id.clone();
        let current_id = app.recorder.current.as_ref().expect("segment open").id;
        let temp_seq = app.recorder.temp_seq;
        let history_len = app.recorder.history.len();

        let cmds = app.update(Msg::Media(crate::media::MediaCommand::Stop));
        assert!(reject_player_transition(&mut app, cmds, error).is_empty());

        assert!(!app.playback.paused);
        assert_eq!(app.playback.time_pos, Some(23.0));
        assert_eq!(app.playback.position_epoch, epoch);
        assert_eq!(app.prefetch.loaded_video_id, loaded);
        assert!(app.video.paused_audio);
        assert_eq!(
            app.recorder.current.as_ref().map(|segment| segment.id),
            Some(current_id)
        );
        assert_eq!(app.recorder.temp_seq, temp_seq);
        assert_eq!(app.recorder.history.len(), history_len);
        assert!(app.recorder.saw_first_title);
    }
}

#[test]
fn current_queue_removal_batches_recorder_clear_before_load_and_rolls_back_on_rejection() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        feed_title(&mut app, "A - One");
        feed_title(&mut app, "B - Two");
        backdate_current(&mut app, 60);
        app.queue
            .extend(vec![Song::remote("next0000001", "Next", "Artist", "3:00")]);
        let current_id = app.recorder.current.as_ref().expect("segment open").id;
        let before_bumps = app.queue.revision_bumps();

        let cmds = app.remove_queue_range(0, 0);
        let commands: Vec<_> = cmds.iter().flat_map(Cmd::player_commands).collect();
        let clear_pos = commands
            .iter()
            .position(|command| {
                command.property().is_some_and(|(name, value)| {
                    name == "stream-record" && value == &serde_json::Value::from("")
                })
            })
            .expect("stream-record clear");
        let load_pos = commands
            .iter()
            .position(|command| matches!(command, crate::player::PlayerCmd::Load(_)))
            .expect("next track load");
        assert!(clear_pos < load_pos, "recorder clear must precede Load");

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert_eq!(current(&app), "rad:groove");
        assert_eq!(app.queue.len(), 2);
        assert_eq!(app.queue.revision_bumps(), before_bumps);
        assert_eq!(
            app.recorder.current.as_ref().map(|segment| segment.id),
            Some(current_id)
        );

        let mut retry = app.remove_queue_range(0, 0);
        admit_player_transition(&mut app, &mut retry);
        assert_eq!(current(&app), "next0000001");
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.queue.revision_bumps(), before_bumps + 1);
        assert!(app.recorder.current.is_none());
        assert!(!app.recorder.saw_first_title);
    }
}

#[test]
fn recorder_off_mode_records_nothing() {
    let mut app = recording_app(crate::recorder::RecordingMode::Nothing);
    let cmds = feed_title(&mut app, "A - One");
    assert!(app.recorder.current.is_none());
    assert!(
        !cmds
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| command
                .property()
                .is_some_and(|(name, _)| name == "stream-record")),
        "no stream-record command when recording is off"
    );
}

#[test]
fn radio_recording_item_hidden_outside_radio_mode() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = false;
    app.open_settings();
    app.settings.as_mut().unwrap().tab = crate::settings::SettingsTab::Playback;
    assert!(
        !app.settings
            .as_ref()
            .unwrap()
            .fields()
            .contains(&crate::settings::Field::RadioRecording)
    );
}

#[test]
fn radio_recording_popup_opens_edits_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(
        app.overlays.recording_settings.is_some(),
        "the button opens the popup"
    );

    // Row 0 = mode; Right cycles Off -> Decide.
    app.recording_settings_key(key(KeyCode::Right));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_mode,
        crate::recorder::RecordingMode::Decide
    );

    // Last row = "Browse recordings…"; Enter opens the recordings browser.
    for _ in 0..(crate::app::RECORDING_POPUP_ROWS - 1) {
        app.recording_settings_key(key(KeyCode::Down));
    }
    app.recording_settings_key(key(KeyCode::Enter));
    assert!(
        app.overlays.recordings_browser.is_some(),
        "browse row opens the browser"
    );

    // Closing Settings commits the draft to config.
    app.overlays.recordings_browser = None;
    app.overlays.recording_settings = None;
    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.config.recording.mode,
        crate::recorder::RecordingMode::Decide
    );
}

#[test]
fn settings_off_reconciles_both_current_and_pending_recorder_transitions() {
    for pending_rotation in [false, true] {
        let mut app = recording_app(crate::recorder::RecordingMode::Decide);
        feed_title(&mut app, "A - One");
        feed_title(&mut app, "B - Two");
        backdate_current(&mut app, 60);

        let pending = if pending_rotation {
            let cmds = app.update(PlayerMsg::Metadata(
                serde_json::json!({ "icy-title": "C - Three" }),
            ));
            let (follow_ups, commands) = admit_recorder_transition_without_replies(&mut app, cmds);
            Some((follow_ups, commands))
        } else {
            None
        };

        app.open_settings();
        app.settings.as_mut().unwrap().draft.recording_mode =
            crate::recorder::RecordingMode::Nothing;
        let settings_cmds = app.close_settings();
        let mut effects = app.admit_player_intents_with_followups_for_test(&settings_cmds);
        assert_eq!(
            app.config.recording.mode,
            crate::recorder::RecordingMode::Nothing
        );

        if let Some((follow_ups, commands)) = pending {
            assert!(effects.iter().all(|effect| {
                !effect.player_commands().any(|command| {
                    command.property().is_some_and(|(name, value)| {
                        name == "stream-record" && value == &serde_json::Value::from("")
                    })
                })
            }));
            effects.extend(resolve_admitted_recorder_transition(
                &mut app, follow_ups, commands, true, true,
            ));
        }

        assert!(emits_stream_record_clear(&effects));
        admit_player_transition(&mut app, &mut effects);
        assert!(app.recorder.current.is_none());
        assert!(!app.recorder.saw_first_title);
    }
}

#[test]
fn finalized_segment_policy_is_snapshotted_at_the_title_boundary() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::RecorderJob;

    let root = recorder_scratch("policy-snapshot");
    let original_dir = root.join("original");
    let later_dir = root.join("later");
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    app.config.recording.track_directory = Some(original_dir.clone());
    app.config.recording.min_duration_secs = 30;
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 35);

    let cmds = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "C - Three" }),
    ));
    let (follow_ups, commands) = admit_recorder_transition_without_replies(&mut app, cmds);
    app.config.recording.mode = crate::recorder::RecordingMode::Nothing;
    app.config.recording.min_duration_secs = 300;
    app.config.recording.track_directory = Some(later_dir);

    let mut effects =
        resolve_admitted_recorder_transition(&mut app, follow_ups, commands, true, true);

    let track = app.recorder.history.front().expect("boundary kept track");
    assert_eq!(track.raw, "B - Two");
    assert_eq!(track.duration_secs, 35);
    assert_eq!(track.state, RecordingState::SaveRequested);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::Save { final_dir, .. }) if final_dir == &original_dir
    )));
    assert!(emits_stream_record_clear(&effects));
    admit_player_transition(&mut app, &mut effects);
    assert!(app.recorder.current.is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn decide_boundary_is_not_retroactively_auto_saved_after_mode_change() {
    use crate::recorder::RecordingState;
    use crate::recorder::job::RecorderJob;

    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let cmds = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "C - Three" }),
    ));
    let (follow_ups, commands) = admit_recorder_transition_without_replies(&mut app, cmds);
    app.config.recording.mode = crate::recorder::RecordingMode::Everything;

    let effects = resolve_admitted_recorder_transition(&mut app, follow_ups, commands, true, true);

    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::Recorded
    );
    assert!(
        effects
            .iter()
            .all(|effect| { !matches!(effect, Cmd::Recorder(RecorderJob::Save { .. })) })
    );
}

#[test]
fn transport_close_settles_pending_boundary_once_and_ignores_late_replies() {
    use crate::recorder::job::{RecorderEvent, RecorderJob};

    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let cmds = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "C - Three" }),
    ));
    let (follow_ups, commands) = admit_recorder_transition_without_replies(&mut app, cmds);

    let restart = app.update(PlayerMsg::TransportClosed("injected EOF".to_owned()));
    assert!(matches!(
        restart.last(),
        Some(Cmd::PlayerControl(PlayerControl::Restart { .. }))
    ));
    let settled = app.settle_recorder_player_retired();
    assert_eq!(app.recorder.history.len(), 1);
    assert_eq!(app.recorder.history[0].raw, "B - Two");
    assert!(app.recorder.current.is_none());
    assert!(
        settled
            .iter()
            .any(|effect| matches!(effect, Cmd::Recorder(RecorderJob::Discard { .. })))
    );

    let late = resolve_admitted_recorder_transition(&mut app, follow_ups, commands, false, false);
    assert!(late.is_empty(), "retired transition outcome is stale");
    assert!(
        app.update(Msg::Recorder(RecorderEvent::TransitionResolved {
            transition_id: u64::MAX,
            close: Some(Err("late".to_owned())),
            open: Some(Err("late".to_owned())),
        }))
        .is_empty()
    );
    app.recorder_player_restart_completed(true);
    let mut fresh = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "D - Fresh" }),
    ));
    admit_player_transition(&mut app, &mut fresh);
    assert_eq!(app.recorder.current.as_ref().unwrap().raw, "D - Fresh");
    assert!(app.recorder.current.as_ref().unwrap().incomplete);
}

#[test]
fn transport_retirement_discards_unplanned_current_without_dead_ipc_proof() {
    use crate::recorder::job::RecorderJob;

    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let source = app.recorder.current.as_ref().unwrap().temp_path.clone();

    let _restart = app.update(PlayerMsg::TransportClosed("injected EOF".to_owned()));
    let settled = app.settle_recorder_player_retired();

    assert!(app.recorder.current.is_none());
    assert!(app.recorder.history.is_empty());
    assert!(settled.iter().any(|effect| matches!(
        effect,
        Cmd::Recorder(RecorderJob::Discard { temp, .. }) if temp == &source
    )));
    assert!(
        settled
            .iter()
            .all(|effect| { !matches!(effect, Cmd::Recorder(RecorderJob::Save { .. })) })
    );
}

#[test]
fn recording_slider_drag_maps_column_to_value() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(app.overlays.recording_settings.is_some());

    // An 11-cell track; the mapping uses width-1 divisions so both ends are reachable.
    let track = Rect {
        x: 20,
        y: 5,
        width: 11,
        height: 1,
    };

    // Keep-recent (row 4, 1..=50 step 1): leftmost cell → min, rightmost cell → max.
    app.recording_slider_set(4, track.x, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        1
    );
    app.recording_slider_set(4, track.x + 10, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        50
    );

    // Min duration (row 1, 5..=600 step 5): a column past the right end clamps to the max,
    // and one before the left end clamps to the min — proof the drag works both directions.
    app.recording_slider_set(1, track.right() + 5, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_min_seconds,
        600
    );
    app.recording_slider_set(1, 0, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_min_seconds,
        5
    );
}

#[test]
fn recorder_saved_emits_desktop_notify_when_enabled() {
    let _guard = crate::i18n::lock_for_test();
    use crate::recorder::job::RecorderEvent;
    let mut app = App::new(100);

    // notify on → a saved recording returns a DesktopNotify command (the filename is the body),
    // in addition to the in-app toast.
    app.config.recording.notify = true;
    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 1,
        final_path: std::path::PathBuf::from("/tmp/Artist - Track.mp3"),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::DesktopNotify { body, .. } if body == "Artist - Track.mp3"
        )),
        "a saved recording fires a desktop notification with the filename as the body"
    );

    // notify off → no desktop notification.
    app.config.recording.notify = false;
    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 2,
        final_path: std::path::PathBuf::from("/tmp/Other.mp3"),
        recovery_owned: false,
        durability_warning: None,
        capacity_available: true,
    });
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::DesktopNotify { .. })),
        "with notifications off, no desktop notification is fired"
    );
}

#[test]
fn recorder_post_commit_sync_warning_keeps_saved_state_and_surfaces_warning() {
    use crate::recorder::job::RecorderEvent;
    use crate::recorder::{RecordedTrack, RecordingState};

    let mut app = App::new(100);
    app.config.recording.notify = false;
    app.recorder.history.push_front(RecordedTrack {
        id: 41,
        title: Some("Track".to_owned()),
        artist: Some("Artist".to_owned()),
        raw: "Artist - Track".to_owned(),
        station: Some("Station".to_owned()),
        temp_path: std::path::PathBuf::from("/tmp/rec-41.mkv"),
        ext: "mkv",
        duration_secs: 60,
        state: RecordingState::Saved,
        final_path: None,
        automatic_final_dir: None,
        close_barrier: None,
        save_request: None,
    });
    let final_path = std::path::PathBuf::from("/tmp/Artist - Track.mkv");

    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 41,
        final_path: final_path.clone(),
        recovery_owned: true,
        durability_warning: Some("injected parent sync fault".to_owned()),
        capacity_available: false,
    });

    assert!(cmds.is_empty());
    let track = app.recorder.history.front().unwrap();
    assert_eq!(track.state, RecordingState::SavePending);
    assert_eq!(track.final_path.as_ref(), Some(&final_path));
    assert!(app.recorder.health_sticky);
    assert_eq!(
        app.recorder.health_warning.as_deref(),
        Some(app.status.text.as_str())
    );
    assert!(app.status.text.contains("injected parent sync fault"));
    assert!(app.recorder_discard(41).is_empty());
    assert_eq!(
        app.recorder.history.front().unwrap().state,
        RecordingState::SavePending
    );
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(
        app.recorder
            .health_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("injected parent sync fault"))
    );
}

#[test]
fn recorder_peer_settlement_keeps_optimistic_saved_state_without_retry() {
    use crate::recorder::job::RecorderEvent;
    use crate::recorder::{RecordedTrack, RecordingState};

    let mut app = App::new(100);
    app.recorder.history.push_front(RecordedTrack {
        id: 42,
        title: Some("Track".to_owned()),
        artist: Some("Artist".to_owned()),
        raw: "Artist - Track".to_owned(),
        station: Some("Station".to_owned()),
        temp_path: std::path::PathBuf::from("/tmp/rec-42.mkv"),
        ext: "mkv",
        duration_secs: 60,
        state: RecordingState::Saved,
        final_path: None,
        automatic_final_dir: None,
        close_barrier: None,
        save_request: None,
    });

    let cmds = app.on_recorder_event(RecorderEvent::AlreadySettled {
        id: 42,
        capacity_available: true,
    });

    assert!(
        cmds.is_empty(),
        "peer settlement must never enqueue a retry"
    );
    let track = app.recorder.history.front().unwrap();
    assert_eq!(track.state, RecordingState::Saved);
    assert!(track.final_path.is_none());
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn recording_settings_popup_closed_by_navigation() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(
        app.overlays.recording_settings.is_some(),
        "the button opens the popup"
    );

    // Navigating Home must drop the top-level overlay so it can't strand over the Player
    // (regression: it used to keep painting on top, unreachable).
    let mut cmds = app.go_home();
    admit_player_transition(&mut app, &mut cmds);
    assert!(
        app.overlays.recording_settings.is_none(),
        "go_home clears the recording-settings popup"
    );
    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn radio_stream_metadata_updates_dj_gem_context() {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    let mut load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut load);

    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));

    assert_eq!(
        app.playback
            .stream_now_playing
            .as_ref()
            .map(StreamNowPlaying::label)
            .as_deref(),
        Some("Track — Artist")
    );
    let ctx = app.build_ai_context();
    assert_eq!(
        ctx.current_radio_station.as_deref(),
        Some("Station groove — KR / MP3")
    );
    assert_eq!(
        ctx.current_radio_now_playing.as_deref(),
        Some("Track — Artist")
    );

    app.dirty = false;
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(!app.dirty, "unchanged stream metadata should not redraw");
}

#[test]
fn stream_metadata_is_ignored_for_regular_tracks() {
    let mut app = app_playing(1, 0);

    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));

    assert!(app.playback.stream_now_playing.is_none());
    let ctx = app.build_ai_context();
    assert!(ctx.current_radio_station.is_none());
    assert!(ctx.current_radio_now_playing.is_none());
}

#[test]
fn loading_a_new_track_clears_stale_stream_metadata() {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    let mut load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut load);
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(app.playback.stream_now_playing.is_some());

    app.queue.set(songs(1), 0);
    let mut load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut load);

    assert!(app.playback.stream_now_playing.is_none());
}
