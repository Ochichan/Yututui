use super::*;

#[test]
fn interactive_flight_requires_reply_and_one_same_generation_completion_boundary() {
    let mut flight = SeekFlight::new(
        7,
        42,
        3,
        SeekPurpose::Interactive { target_secs: 900.0 },
        false,
    );
    assert!(!flight.observe(SeekObservation::PlaybackRestart, Some(2)));
    assert!(!flight.observe(SeekObservation::PlaybackRestart, Some(3)));
    assert!(flight.observe(
        SeekObservation::CommandReply {
            request_id: 42,
            accepted: true,
        },
        Some(3),
    ));
}

#[test]
fn rejected_interactive_reply_releases_the_lane_immediately() {
    let mut flight = SeekFlight::new(
        8,
        43,
        3,
        SeekPurpose::Interactive { target_secs: 900.0 },
        false,
    );
    assert!(flight.observe(
        SeekObservation::CommandReply {
            request_id: 43,
            accepted: false,
        },
        Some(3),
    ));
}

#[test]
fn coalesced_false_completion_uses_restart_on_current_and_032_event_models() {
    for sequence in [32, 41] {
        let mut flight = SeekFlight::new(
            sequence,
            sequence + 100,
            3,
            SeekPurpose::Interactive { target_secs: 900.0 },
            false,
        );
        assert!(!flight.observe(
            SeekObservation::CommandReply {
                request_id: sequence + 100,
                accepted: true,
            },
            Some(3),
        ));
        assert!(
            !flight.observe(SeekObservation::Seeking(false), Some(3)),
            "a coalesced false without an observed true is not standalone proof"
        );
        assert!(flight.observe(SeekObservation::PlaybackRestart, Some(3)));
    }
}

#[test]
fn lifecycle_before_reply_is_retained_but_wrong_reply_cannot_release_the_sequence() {
    let mut next = SeekFlight::new(
        2,
        202,
        9,
        SeekPurpose::Interactive {
            target_secs: 1_200.0,
        },
        false,
    );
    assert!(!next.observe(SeekObservation::PlaybackRestart, Some(9)));
    assert!(!next.observe(
        SeekObservation::CommandReply {
            request_id: 201,
            accepted: true,
        },
        Some(9),
    ));
    assert!(next.observe(
        SeekObservation::CommandReply {
            request_id: 202,
            accepted: true,
        },
        Some(9),
    ));
}

#[test]
fn wrong_generation_restart_before_reply_does_not_release_the_sequence() {
    let mut next = SeekFlight::new(
        2,
        202,
        9,
        SeekPurpose::Interactive {
            target_secs: 1_200.0,
        },
        false,
    );
    assert!(!next.observe(SeekObservation::PlaybackRestart, Some(8)));
    assert!(!next.observe(
        SeekObservation::CommandReply {
            request_id: 202,
            accepted: true,
        },
        Some(9),
    ));
    assert!(next.observe(SeekObservation::PlaybackRestart, Some(9)));
}

#[test]
fn acknowledged_seek_on_known_unseekable_media_completes_immediately_nonfatally() {
    let mut flight = SeekFlight::new(3, 303, 12, SeekPurpose::CompletionBarrier, true);
    assert!(flight.observe(
        SeekObservation::CommandReply {
            request_id: 303,
            accepted: true,
        },
        Some(12),
    ));
    assert!(!flight.cache_success_proven());
}

#[test]
fn known_unseekable_ack_cannot_prove_an_interactive_off_cache_seek() {
    let mut flight = SeekFlight::new(
        4,
        304,
        12,
        SeekPurpose::Interactive { target_secs: 900.0 },
        true,
    );
    assert!(flight.observe(
        SeekObservation::CommandReply {
            request_id: 304,
            accepted: true,
        },
        Some(12),
    ));
    assert_eq!(flight.interactive_target(), Some(900.0));
    assert!(!flight.cache_success_proven());
}

#[test]
fn rejected_recovery_exact_clears_quarantine_and_owned_pause() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        ..DispatchState::default()
    };
    state.resume.install_dispatching_for_test(
        9,
        VecDeque::from([PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(true),
        }]),
    );
    let request = recovery_request(3_600.25, true);
    install_resume_telemetry(&mut state, 9, &request);
    begin_resume_seek_observation(&mut state, 9, 304);
    let mut flight = SeekFlight::new(1, 304, 9, SeekPurpose::CompletionBarrier, false);
    assert!(flight.observe(
        SeekObservation::CommandReply {
            request_id: 304,
            accepted: false,
        },
        Some(9),
    ));

    finish_seek_flight(&emit, &mut state, flight);

    assert!(!state.resume.telemetry_is_some());
    assert!(state.resume.dispatch_generation().is_none());
    assert!(state.resume.commands_is_empty());
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":10.0}"#,
        &emit,
        &mut state,
    );
    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 9);
    assert!(matches!(event, PlayerEvent::TimePos(10.0)));
}

#[test]
fn rejected_old_seek_drops_mismatched_telemetry_without_clearing_new_dispatch() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState::default();
    state.resume.install_dispatching_for_test(
        10,
        VecDeque::from([PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(true),
        }]),
    );
    let request = recovery_request(3_600.25, true);
    install_resume_telemetry(&mut state, 10, &request);
    begin_resume_seek_observation(&mut state, 10, 400);
    let mut completed = SeekFlight::new(1, 304, 9, SeekPurpose::CompletionBarrier, false);
    assert!(completed.observe(
        SeekObservation::CommandReply {
            request_id: 304,
            accepted: false,
        },
        Some(9),
    ));

    finish_seek_flight(&emit, &mut state, completed);

    assert!(!state.resume.telemetry_is_some());
    assert_eq!(state.resume.dispatch_generation(), Some(10));
    assert!(matches!(
        state.resume.front_command(),
        Some(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == &serde_json::Value::Bool(true)
    ));
}

#[test]
fn rejected_recovery_exact_is_terminal_before_pause_restore_can_dispatch() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let exact = PlayerCmd::exact_seek(3_600.25);
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        ..DispatchState::default()
    };
    state.resume.install_dispatching_for_test(
        9,
        VecDeque::from([PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(false),
        }]),
    );
    assert_eq!(
        recovery_terminal_operation(&state, &exact),
        Some("recovery exact seek")
    );
    state.pending.insert(
        304,
        PendingCommand {
            label: "seek".to_owned(),
            file_generation: Some(9),
            acknowledgement: None,
            audio_output: None,
            terminal_contract: Some(PendingTerminalContract {
                operation: "recovery exact seek",
                deadline: Instant::now() + INTERNAL_COMMAND_REPLY_TIMEOUT,
            }),
        },
    );

    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":304}"#,
        &emit,
        &mut state,
    );

    assert!(matches!(
        state.terminal_failure,
        Some(ActorExit::InternalCommandFailed {
            operation: "recovery exact seek",
            rejected: true,
        })
    ));
    assert_eq!(state.resume.dispatch_generation(), Some(9));
    assert!(matches!(
        state.resume.front_command(),
        Some(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == &serde_json::Value::Bool(false)
    ));
}

#[test]
fn rejected_ordinary_seek_recycles_instead_of_leaving_optimistic_position() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let command = PlayerCmd::interactive_seek(900.0);
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        ..DispatchState::default()
    };
    assert_eq!(
        command_terminal_operation(&state, &command, None),
        Some("seek")
    );
    state.pending.insert(
        305,
        PendingCommand {
            label: "seek".to_owned(),
            file_generation: Some(9),
            acknowledgement: None,
            audio_output: None,
            terminal_contract: Some(PendingTerminalContract {
                operation: "seek",
                deadline: Instant::now() + INTERNAL_COMMAND_REPLY_TIMEOUT,
            }),
        },
    );

    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":305}"#,
        &emit,
        &mut state,
    );

    let failure = state
        .terminal_failure
        .take()
        .expect("seek rejection must be terminal");
    assert!(matches!(
        &failure,
        ActorExit::InternalCommandFailed {
            operation: "seek",
            rejected: true,
        }
    ));
    assert!(matches!(
        terminal_events(failure).as_slice(),
        [PlayerEvent::TransportClosed(reason)]
            if reason.contains("seek was rejected")
    ));
}

#[test]
fn interleaved_fast_completions_still_dispatch_only_first_and_latest_burst_targets() {
    let started = Instant::now();
    let mut gate = InteractiveBurstGate::default();
    let mut validation = None;
    let mut backlog = VecDeque::new();
    let mut wire_targets = Vec::new();

    let first = PlayerCmd::interactive_seek(0.0);
    gate.observe_command(&first, started);
    stage_actor_command(first, &mut validation, &mut backlog);
    assert!(gate.command_ready(backlog.front().expect("first target"), started));
    let first = backlog
        .pop_front()
        .expect("first target dispatches immediately");
    gate.dispatched(&first, started);
    wire_targets.push(0_u64);

    // Model mpv completing each possible seek before the next 25 ms arrival. The trailing gate
    // remains actor-owned after completion, so arrivals still collapse into one latest target.
    for target in 1_u64..20 {
        let arrived = started + Duration::from_millis(target * 25);
        let command = PlayerCmd::interactive_seek(target as f64);
        gate.observe_command(&command, arrived);
        stage_actor_command(command, &mut validation, &mut backlog);
        assert_eq!(backlog.len(), 1);
        assert!(!gate.command_ready(backlog.front().expect("latest target"), arrived));
    }

    let final_arrival = started + Duration::from_millis(19 * 25);
    assert!(!gate.command_ready(
        backlog.front().expect("latest target"),
        final_arrival + INTERACTIVE_SEEK_TRAILING_DEBOUNCE - Duration::from_millis(1),
    ));
    assert!(gate.command_ready(
        backlog.front().expect("latest target"),
        final_arrival + INTERACTIVE_SEEK_TRAILING_DEBOUNCE,
    ));
    let latest = backlog.pop_front().expect("latest target dispatches");
    assert!(matches!(
        latest,
        PlayerCmd::SeekAbsolute { seconds, .. } if (seconds - 19.0).abs() < f64::EPSILON
    ));
    wire_targets.push(19);
    assert_eq!(wire_targets, [0, 19]);
}

#[test]
fn exact_seek_resets_the_interactive_trailing_gate() {
    let started = Instant::now();
    let mut gate = InteractiveBurstGate::default();
    let first = PlayerCmd::interactive_seek(10.0);
    gate.dispatched(&first, started);

    let exact = PlayerCmd::exact_seek(20.0);
    gate.observe_command(&exact, started + Duration::from_millis(10));
    let next = PlayerCmd::interactive_seek(30.0);
    gate.observe_command(&next, started + Duration::from_millis(20));

    assert!(gate.command_ready(&next, started + Duration::from_millis(20)));
}

#[test]
fn seeking_completion_holds_the_flight_during_the_late_restart_drain_grace() {
    let mut flight = Some(SeekFlight::new(
        4,
        404,
        12,
        SeekPurpose::Interactive { target_secs: 90.0 },
        false,
    ));
    assert!(
        observe_seek_flight(
            &mut flight,
            SeekObservation::CommandReply {
                request_id: 404,
                accepted: true,
            },
            Some(12),
        )
        .is_none()
    );
    assert!(observe_seek_flight(&mut flight, SeekObservation::Seeking(true), Some(12)).is_none());
    assert!(observe_seek_flight(&mut flight, SeekObservation::Seeking(false), Some(12)).is_none());
    let settle_deadline = flight
        .as_ref()
        .and_then(|flight| flight.settle_deadline)
        .expect("seeking completion arms drain grace");
    assert!(take_settled_seek_flight(&mut flight, settle_deadline).is_some());
}

#[test]
fn restart_during_seeking_drain_grace_is_consumed_by_the_current_sequence() {
    let mut flight = Some(SeekFlight::new(
        5,
        405,
        12,
        SeekPurpose::Interactive { target_secs: 120.0 },
        false,
    ));
    for observation in [
        SeekObservation::CommandReply {
            request_id: 405,
            accepted: true,
        },
        SeekObservation::Seeking(true),
        SeekObservation::Seeking(false),
    ] {
        assert!(observe_seek_flight(&mut flight, observation, Some(12)).is_none());
    }
    assert!(
        observe_seek_flight(&mut flight, SeekObservation::PlaybackRestart, Some(12),).is_some()
    );
}

#[test]
fn initial_load_restart_cannot_release_interactive_seek_before_file_loaded() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 4,
        active_file_generation: Some(4),
        active_playlist_entry_id: Some(44),
        ..DispatchState::default()
    };
    state.entry_generations.insert(44, 4);
    let seek = PlayerCmd::interactive_seek(900.0);

    assert!(!command_ready_for_dispatch(&state, &seek));

    dispatch_incoming(
        r#"{"event":"file-loaded","playlist_entry_id":44}"#,
        &emit,
        &mut state,
    );
    assert!(!command_ready_for_dispatch(&state, &seek));
    dispatch_incoming(r#"{"event":"playback-restart"}"#, &emit, &mut state);
    assert!(state.seek_observation.is_none());
    assert!(command_ready_for_dispatch(&state, &seek));
}

#[test]
fn early_file_loaded_proof_is_released_only_after_entry_id_correlation() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 5,
        ..DispatchState::default()
    };
    assert!(remember_pending_load(&mut state, 51, 5, "loadfile"));
    let seek = PlayerCmd::interactive_seek(1_200.0);

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":55}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"file-loaded","playlist_entry_id":55}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"event":"playback-restart"}"#, &emit, &mut state);
    assert!(state.uncorrelated_file_loaded);
    assert!(!command_ready_for_dispatch(&state, &seek));

    dispatch_incoming(
        r#"{"error":"success","request_id":51,"data":{"playlist_entry_id":55}}"#,
        &emit,
        &mut state,
    );
    assert_eq!(state.file_loaded_generation, Some(5));
    assert_eq!(state.playback_ready_generation, Some(5));
    assert!(command_ready_for_dispatch(&state, &seek));
}

#[test]
fn mpv_032_early_file_loaded_proof_survives_legacy_identity_reply() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 6,
        ..DispatchState::default()
    };
    state.legacy_loads.push_back(LegacyLoad {
        generation: 6,
        url: "https://media.example/current".to_owned(),
        replied: false,
    });
    assert!(remember_pending_load(&mut state, 61, 6, "loadfile"));
    assert!(remember_pending_load(
        &mut state,
        62,
        6,
        "loadfile identity"
    ));
    let seek = PlayerCmd::interactive_seek(1_800.0);

    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"event":"file-loaded"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"event":"playback-restart"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"error":"success","request_id":61}"#, &emit, &mut state);
    assert!(!command_ready_for_dispatch(&state, &seek));

    dispatch_incoming(
        r#"{"error":"success","request_id":62,"data":[{"filename":"https://media.example/current","current":true,"playing":true}]}"#,
        &emit,
        &mut state,
    );
    assert_eq!(state.file_loaded_generation, Some(6));
    assert_eq!(state.playback_ready_generation, Some(6));
    assert!(command_ready_for_dispatch(&state, &seek));
}

#[test]
fn exact_seek_can_complete_from_observed_seeking_pair_without_restart() {
    let mut exact = Some(SeekFlight::new(
        1,
        70,
        7,
        SeekPurpose::for_command(&PlayerCmd::exact_seek(600.0)).expect("exact seek flight"),
        false,
    ));

    assert!(
        observe_seek_flight(
            &mut exact,
            SeekObservation::CommandReply {
                request_id: 70,
                accepted: true,
            },
            Some(7),
        )
        .is_none()
    );
    assert!(observe_seek_flight(&mut exact, SeekObservation::Seeking(true), Some(7)).is_none());
    assert!(observe_seek_flight(&mut exact, SeekObservation::Seeking(false), Some(7)).is_none());
    let settle_deadline = exact
        .as_ref()
        .and_then(|flight| flight.settle_deadline)
        .expect("seeking completion arms drain grace");
    assert!(take_settled_seek_flight(&mut exact, settle_deadline).is_some());

    let interactive = SeekPurpose::for_command(&PlayerCmd::interactive_seek(900.0));
    assert!(matches!(interactive, Some(SeekPurpose::Interactive { .. })));
}

#[test]
fn recovery_exact_command_is_a_completion_barrier_before_user_seek() {
    let mut post_load = VecDeque::from([
        PlayerCmd::exact_seek(3_600.25),
        PlayerCmd::interactive_seek(900.0),
    ]);
    let recovery_seek = post_load.pop_front().expect("recovery exact seek");

    assert_eq!(
        SeekPurpose::for_command(&recovery_seek),
        Some(SeekPurpose::CompletionBarrier)
    );
    assert!(matches!(
        post_load.front(),
        Some(PlayerCmd::SeekAbsolute {
            precision: super::super::super::SeekPrecision::InteractiveFast,
            ..
        })
    ));
}

#[test]
fn timed_out_seek_recycles_actor_before_late_a_events_or_b_can_be_observed() {
    let mut timed_out = SeekFlight::new(
        1,
        80,
        8,
        SeekPurpose::Interactive { target_secs: 900.0 },
        false,
    );
    assert!(!timed_out.observe(
        SeekObservation::CommandReply {
            request_id: 80,
            accepted: true,
        },
        Some(8),
    ));

    let events = terminal_events(ActorExit::SeekCausalityLost);
    assert!(matches!(
        events.as_slice(),
        [PlayerEvent::TransportClosed(reason)] if reason.contains("seek completion timed out")
    ));
}

#[test]
fn relative_seek_uses_the_same_completion_ownership() {
    assert_eq!(
        SeekPurpose::for_command(&PlayerCmd::SeekRelative(30.0)),
        Some(SeekPurpose::CompletionBarrier)
    );
}

#[test]
fn zero_position_emergency_resume_does_not_create_a_noop_seek_flight() {
    let mut state = DispatchState {
        active_file_generation: Some(9),
        last_confirmed_time: 91.0,
        ..DispatchState::default()
    };
    install_resume_state(&mut state, 9, recovery_request(0.0, false));
    state.resume.mark_file_loaded(Some(9), true, Some(9));

    release_pending_resume_if_ready(&mut state);

    assert_eq!(state.last_confirmed_time, 0.0);
    assert_eq!(state.resume.command_count(), 1);
    assert!(matches!(
        state.resume.front_command(),
        Some(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == &serde_json::Value::Bool(false)
    ));
}

#[test]
fn recovery_quarantines_initial_zero_until_exact_completion_releases_near_target() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        ..DispatchState::default()
    };
    let request = recovery_request(3_600.25, false);
    install_resume_telemetry(&mut state, 9, &request);

    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":0.0}"#,
        &emit,
        &mut state,
    );
    begin_resume_seek_observation(&mut state, 9, 77);
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":0.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(state.last_confirmed_time, 3_600.25);

    complete_resume_telemetry(&emit, &mut state, 9, 77);
    assert!(
        rx.try_recv().is_err(),
        "completion alone cannot release telemetry"
    );
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":3599.0}"#,
        &emit,
        &mut state,
    );
    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 9);
    assert!(matches!(event, PlayerEvent::TimePos(position) if position == 3_599.0));
    assert!(!state.resume.telemetry_is_some());
}

#[test]
fn short_recovery_target_never_accepts_transient_zero_as_resume_proof() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let request = recovery_request(1.0, false);
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        ..DispatchState::default()
    };
    install_resume_telemetry(&mut state, 9, &request);
    begin_resume_seek_observation(&mut state, 9, 78);

    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":0.0}"#,
        &emit,
        &mut state,
    );
    complete_resume_telemetry(&emit, &mut state, 9, 78);

    assert!(rx.try_recv().is_err(), "transient zero stays quarantined");
    assert!(resume_position_terminal_deadline(&state).is_some());
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":1.0}"#,
        &emit,
        &mut state,
    );
    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 9);
    assert!(matches!(event, PlayerEvent::TimePos(1.0)));
    assert!(!state.resume.telemetry_is_some());
}

#[test]
fn newly_merged_target_rejects_old_position_overshoot_as_resume_proof() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let request = recovery_request(3_600.0, false);
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        ..DispatchState::default()
    };
    state.resume.install_dispatching_for_test(
        9,
        VecDeque::from([PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(false),
        }]),
    );
    install_resume_telemetry(&mut state, 9, &request);
    begin_resume_seek_observation(&mut state, 9, 78);
    assert!(merge_post_load_resume_command(
        &mut state,
        &PlayerCmd::interactive_seek(120.0),
    ));
    begin_resume_seek_observation(&mut state, 9, 79);

    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":3600.0}"#,
        &emit,
        &mut state,
    );
    complete_resume_telemetry(&emit, &mut state, 9, 79);

    assert!(
        rx.try_recv().is_err(),
        "old target overshoot stays quarantined"
    );
    assert!(resume_position_terminal_deadline(&state).is_some());
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":120.0}"#,
        &emit,
        &mut state,
    );
    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 9);
    assert!(matches!(event, PlayerEvent::TimePos(120.0)));
    assert!(!state.resume.telemetry_is_some());
}

#[test]
fn dispatched_recovery_keeps_zero_quarantine_across_real_start_file_sequence() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let request = recovery_request(3_600.25, true);
    let mut state = DispatchState {
        issued_file_generation: 9,
        ..DispatchState::default()
    };
    install_resume_state(&mut state, 9, request.clone());
    state.pending.insert(
        55,
        PendingCommand {
            label: "loadfile".to_owned(),
            file_generation: Some(9),
            acknowledgement: None,
            audio_output: None,
            terminal_contract: None,
        },
    );

    dispatch_incoming(
        r#"{"error":"success","request_id":55,"data":{"playlist_entry_id":42}}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":0.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err(), "start-file must not release zero");
    assert_eq!(state.last_confirmed_time, 3_600.25);

    dispatch_incoming(
        r#"{"event":"file-loaded","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"event":"playback-restart"}"#, &emit, &mut state);
    let exact = pop_next_actor_command(&mut state, &mut VecDeque::new())
        .expect("correlated recovery releases exact seek");
    assert!(matches!(
        exact,
        PlayerCmd::SeekAbsolute {
            seconds,
            precision: super::super::super::SeekPrecision::Exact,
        } if (seconds - 3_600.25).abs() < f64::EPSILON
    ));

    let mut flight = Some(SeekFlight::new(
        1,
        56,
        9,
        SeekPurpose::CompletionBarrier,
        false,
    ));
    begin_resume_seek_observation(&mut state, 9, 56);
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":0.0}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":3599.0}"#,
        &emit,
        &mut state,
    );
    assert!(
        rx.try_recv().is_err(),
        "target proof waits for exact ACK and lifecycle completion"
    );
    dispatch_incoming(r#"{"error":"success","request_id":56}"#, &emit, &mut state);
    assert!(
        observe_seek_flight(
            &mut flight,
            state.seek_observation.take().expect("seek ACK observation"),
            state.active_file_generation,
        )
        .is_none()
    );
    dispatch_incoming(r#"{"event":"playback-restart"}"#, &emit, &mut state);
    let completed = observe_seek_flight(
        &mut flight,
        state
            .seek_observation
            .take()
            .expect("seek restart observation"),
        state.active_file_generation,
    )
    .expect("ACK plus restart completes exact recovery");
    finish_seek_flight(&emit, &mut state, completed);

    assert!(
        !state.resume.telemetry_is_some(),
        "pre-completion target proof releases at completion"
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 9);
    assert!(matches!(event, PlayerEvent::TimePos(position) if position == 3_599.0));
    assert!(rx.try_recv().is_err());
    assert!(matches!(
        state.resume.front_command(),
        Some(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == &serde_json::Value::Bool(true)
    ));
}

#[test]
fn replaced_old_end_file_cannot_clear_new_recovery_quarantine() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let request = recovery_request(3_600.25, false);
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(8),
        active_playlist_entry_id: Some(42),
        ..DispatchState::default()
    };
    install_resume_state(&mut state, 9, request.clone());
    state.entry_generations.insert(42, 8);
    state.entry_generations.insert(43, 9);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"stop","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    assert!(state.resume.telemetry_is_some());
    assert!(state.resume.pending_request().is_some());
    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":43}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":0.0}"#,
        &emit,
        &mut state,
    );

    assert!(rx.try_recv().is_err());
    assert_eq!(state.active_file_generation, Some(9));
    assert_eq!(state.last_confirmed_time, 3_600.25);
    assert!(state.resume.telemetry_is_some());
}

#[test]
fn paused_recovery_exact_holds_under_target_position_until_bounded_failure() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let request = recovery_request(3_600.25, true);
    let pause_restore = PlayerCmd::SetProperty {
        name: "pause".to_owned(),
        value: serde_json::Value::Bool(true),
    };
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        file_loaded_generation: Some(9),
        playback_ready_generation: Some(9),
        ..DispatchState::default()
    };
    state
        .resume
        .install_dispatching_for_test(9, VecDeque::from([pause_restore]));
    install_resume_telemetry(&mut state, 9, &request);
    begin_resume_seek_observation(&mut state, 9, 90);
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":3500.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());

    let mut flight = SeekFlight::new(1, 90, 9, SeekPurpose::CompletionBarrier, false);
    assert!(!flight.observe(
        SeekObservation::CommandReply {
            request_id: 90,
            accepted: true,
        },
        Some(9),
    ));
    assert!(flight.observe(SeekObservation::PlaybackRestart, Some(9)));
    finish_seek_flight(&emit, &mut state, flight);

    assert!(rx.try_recv().is_err());
    assert!(resume_position_terminal_deadline(&state).is_some());
    assert!(!command_ready_for_dispatch(
        &state,
        state.resume.front_command().expect("pause restore remains")
    ));
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":3500.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());
    assert!(resume_position_terminal_deadline(&state).is_some());
}

#[test]
fn accepted_recovery_exact_without_position_arms_bounded_terminal_and_holds_pause() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let request = recovery_request(3_600.25, true);
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        file_loaded_generation: Some(9),
        playback_ready_generation: Some(9),
        ..DispatchState::default()
    };
    state.resume.install_dispatching_for_test(
        9,
        VecDeque::from([PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(true),
        }]),
    );
    install_resume_telemetry(&mut state, 9, &request);
    begin_resume_seek_observation(&mut state, 9, 91);
    let mut flight = SeekFlight::new(1, 91, 9, SeekPurpose::CompletionBarrier, false);
    assert!(!flight.observe(
        SeekObservation::CommandReply {
            request_id: 91,
            accepted: true,
        },
        Some(9),
    ));
    assert!(flight.observe(SeekObservation::PlaybackRestart, Some(9)));
    finish_seek_flight(&emit, &mut state, flight);

    assert!(resume_position_terminal_deadline(&state).is_some());
    assert!(!command_ready_for_dispatch(
        &state,
        state
            .resume
            .front_command()
            .expect("pause restore remains held")
    ));
}

#[test]
fn pause_restore_is_not_held_by_another_generations_position_wait() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let pause_restore = PlayerCmd::SetProperty {
        name: "pause".to_owned(),
        value: serde_json::Value::Bool(true),
    };
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        file_loaded_generation: Some(9),
        playback_ready_generation: Some(9),
        ..DispatchState::default()
    };
    state
        .resume
        .install_dispatching_for_test(9, VecDeque::from([pause_restore]));
    let newer_request = recovery_request(3_600.25, true);
    install_resume_telemetry(&mut state, 10, &newer_request);
    begin_resume_seek_observation(&mut state, 10, 92);
    complete_resume_telemetry(&emit, &mut state, 10, 92);

    assert!(resume_position_terminal_deadline(&state).is_some());
    assert!(command_ready_for_dispatch(
        &state,
        state
            .resume
            .front_command()
            .expect("older pause restore remains dispatchable")
    ));
}
