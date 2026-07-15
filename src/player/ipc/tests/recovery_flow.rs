use super::*;

#[tokio::test]
async fn stop_cancels_a_hung_load_validation_without_reordering_prior_commands() {
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
        LoadValidationOutcome::Validated("never".to_owned())
    });
    let mut validation = Some(PendingLoadValidation {
        request_id: 11,
        file_generation: 1,
        task,
        resume: resume::ResumeLoad::None,
        source_context: super::super::super::MediaSourceContext::OnDemand,
    });
    let mut backlog = VecDeque::new();

    stage_actor_command(PlayerCmd::SetVolume(42), &mut validation, &mut backlog);
    assert!(validation.is_some());
    stage_actor_command(PlayerCmd::Stop, &mut validation, &mut backlog);

    assert!(validation.is_none());
    assert!(matches!(
        backlog.pop_front(),
        Some(PlayerCmd::SetVolume(42))
    ));
    assert!(matches!(backlog.pop_front(), Some(PlayerCmd::Stop)));
}

#[tokio::test]
async fn state_free_staging_does_not_guess_that_recovery_can_alias() {
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
        LoadValidationOutcome::Validated("never".to_owned())
    });
    let mut validation = Some(PendingLoadValidation {
        request_id: 11,
        file_generation: 8,
        task,
        resume: resume::ResumeLoad::RestoreOwned(recovery_request(3_600.25, true)),
        source_context: super::super::super::MediaSourceContext::OnDemand,
    });
    let mut backlog = VecDeque::new();

    stage_actor_command(
        PlayerCmd::interactive_seek(900.0),
        &mut validation,
        &mut backlog,
    );

    assert!(validation.is_some());
    assert!(matches!(
        backlog.pop_front(),
        Some(PlayerCmd::SeekAbsolute {
            seconds: 900.0,
            precision: super::super::super::SeekPrecision::InteractiveFast,
        })
    ));
}

#[tokio::test]
async fn superseding_seek_during_recovery_validation_keeps_current_generation_dispatchable() {
    let (_generation_tx, generation_rx) = tokio::sync::watch::channel(8u64);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(4);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = event_tx.try_send(event);
    });
    let mut cache = cache_runtime_for_test(crate::config::LongFormSeekOptimization::On);
    cache.test_force_disk_active(7);
    let shared_cache_status = Arc::new(std::sync::Mutex::new(SharedLongFormSeekStatus::new(
        cache.status(),
    )));
    let mut state = DispatchState {
        issued_file_generation: 7,
        active_file_generation: Some(7),
        file_loaded_generation: Some(7),
        playback_ready_generation: Some(7),
        cache: Some(cache),
        cache_status: Some(Arc::clone(&shared_cache_status)),
        ..DispatchState::default()
    };
    state
        .cache_actions
        .push_back(CacheAction::EmergencyCloseAndResume {
            file_generation: 7,
            position_secs: 15.0,
            paused: false,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        });
    let candidate = reserve_file_generation(&mut state);
    assert_eq!(candidate, 8);
    assert_eq!(*generation_rx.borrow(), candidate);
    assert_eq!(state.issued_file_generation, 7);
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
        LoadValidationOutcome::Validated("never".to_owned())
    });
    let mut validation = Some(PendingLoadValidation {
        request_id: 12,
        file_generation: candidate,
        task,
        resume: resume::ResumeLoad::RestoreOwned(recovery_request(3_600.25, false)),
        source_context: super::super::super::MediaSourceContext::OnDemand,
    });
    let mut backlog = VecDeque::new();
    let mut flight = None;
    let seek = PlayerCmd::interactive_seek(900.0);

    accept_actor_command(&mut state, seek, &mut validation, &mut backlog, &mut flight);

    assert!(validation.is_none());
    assert_eq!(state.issued_file_generation, 8);
    assert_eq!(state.active_file_generation, Some(8));
    assert_eq!(state.file_loaded_generation, Some(8));
    assert_eq!(state.playback_ready_generation, Some(8));
    let cache_status = state.cache.as_ref().expect("cache runtime").status();
    assert_eq!(cache_status.file_generation, Some(8));
    assert_eq!(
        cache_status.effective,
        crate::player::long_form_seek::CacheEffectiveState::DiskActive
    );
    assert!(matches!(
        state.cache_actions.front(),
        Some(CacheAction::EmergencyCloseAndResume {
            file_generation: 8,
            ..
        })
    ));
    assert_eq!(
        shared_cache_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime
            .status
            .file_generation,
        Some(8)
    );
    let command = backlog.front().expect("superseding seek remains queued");
    assert!(command_ready_for_dispatch(&state, command));
    assert!(matches!(
        backlog.pop_front(),
        Some(PlayerCmd::SeekAbsolute { .. })
    ));
    dispatch_incoming(
        r#"{"event":"property-change","name":"time-pos","data":15.0}"#,
        &emit,
        &mut state,
    );
    assert!(matches!(
        event_rx.try_recv(),
        Ok(PlayerEvent::FileScoped {
            file_generation: 8,
            event,
        }) if matches!(*event, PlayerEvent::TimePos(15.0))
    ));
}

#[tokio::test]
async fn emergency_recovery_validation_retains_load_and_force_ram_only_after_user_seek() {
    let mut state = DispatchState {
        issued_file_generation: 0,
        active_file_generation: None,
        file_loaded_generation: None,
        playback_ready_generation: None,
        ..DispatchState::default()
    };
    let candidate = reserve_file_generation(&mut state);
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
        LoadValidationOutcome::Validated("never".to_owned())
    });
    let mut validation = Some(PendingLoadValidation {
        request_id: 12,
        file_generation: candidate,
        task,
        resume: resume::ResumeLoad::RestoreOwned(emergency_request(900.0, true)),
        source_context: super::super::super::MediaSourceContext::OnDemand,
    });
    let mut backlog = VecDeque::new();
    let mut flight = None;

    accept_actor_command(
        &mut state,
        PlayerCmd::interactive_seek(120.0),
        &mut validation,
        &mut backlog,
        &mut flight,
    );
    accept_actor_command(
        &mut state,
        PlayerCmd::CyclePause,
        &mut validation,
        &mut backlog,
        &mut flight,
    );

    let pending = validation
        .as_ref()
        .expect("emergency physical load boundary is retained");
    assert!(matches!(
        &pending.resume,
        resume::ResumeLoad::UserTransportMerged(_)
    ));
    assert!(
        pending
            .resume
            .request()
            .is_some_and(super::super::super::recovery::LoadWithResume::forces_ram_only)
    );
    assert!(pending.resume.request().is_some_and(|resume| {
        (resume.position_secs - 120.0).abs() < f64::EPSILON && !resume.paused
    }));
    assert_eq!(state.issued_file_generation, 0);
    assert!(
        backlog.is_empty(),
        "merged seek must not be dispatched twice"
    );
    pending.task.abort();
}

#[tokio::test]
async fn validated_recovery_wait_keeps_physical_load_and_strips_only_resume_transport() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(PlayerCmd::interactive_seek(120.0)).unwrap();
    let mut boundary = Some(PendingLoadBoundary::Validated(ValidatedLoad {
        request_id: 21,
        file_generation: 8,
        url: "https://example.invalid/fresh-source".to_owned(),
        resume: resume::ResumeLoad::RestoreOwned(recovery_request(900.0, true)),
        source_context: super::super::super::MediaSourceContext::OnDemand,
        wait_for_cache_reset: true,
    }));
    let command = rx
        .try_recv()
        .expect("seek was admitted before cleanup completed");
    assert!(supersede_pending_load_boundary(&command, &mut boundary));
    assert!(supersede_pending_load_boundary(
        &PlayerCmd::CyclePause,
        &mut boundary,
    ));

    let Some(PendingLoadBoundary::Validated(load)) = boundary.as_ref() else {
        panic!("validated physical load boundary must remain")
    };
    assert!(matches!(
        &load.resume,
        resume::ResumeLoad::UserTransportMerged(_)
    ));
    assert!(load.wait_for_cache_reset);
    assert_eq!(load.file_generation, 8);
    assert!(load.resume.request().is_some_and(|resume| {
        (resume.position_secs - 120.0).abs() < f64::EPSILON && !resume.paused
    }));
}

#[test]
fn issued_recovery_merges_seek_and_play_pause_until_file_loaded() {
    let mut state = DispatchState {
        issued_file_generation: 8,
        active_file_generation: None,
        file_loaded_generation: None,
        playback_ready_generation: None,
        ..DispatchState::default()
    };
    state.resume.install(8, recovery_request(900.0, true));
    let mut validation = None;
    let mut backlog = VecDeque::new();
    let mut flight = None;

    for command in [
        PlayerCmd::interactive_seek(120.0),
        PlayerCmd::SeekRelative(30.0),
        PlayerCmd::CyclePause,
        PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(true),
        },
    ] {
        accept_actor_command(
            &mut state,
            command,
            &mut validation,
            &mut backlog,
            &mut flight,
        );
    }

    assert!(backlog.is_empty(), "issued recovery owns transport intent");
    let pending = state
        .resume
        .pending_request()
        .expect("recovery remains owned");
    assert!((pending.position_secs - 150.0).abs() < f64::EPSILON);
    assert!(pending.paused);

    state.active_file_generation = Some(8);
    state.file_loaded_generation = Some(8);
    state.playback_ready_generation = Some(8);
    state.resume.mark_file_loaded(Some(8), true, Some(8));
    release_pending_resume_if_ready(&mut state);

    assert!(matches!(
        state.resume.pop_command(),
        Some(PlayerCmd::SeekAbsolute {
            seconds,
            precision: super::super::super::SeekPrecision::Exact,
        }) if (seconds - 150.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
        state.resume.pop_command(),
        Some(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == serde_json::Value::Bool(true)
    ));
}

#[test]
fn released_recovery_merges_desired_transport_before_and_during_exact_flight() {
    let emit: EventSink = Arc::new(|_| {});
    for saved_paused in [false, true] {
        for exact_in_flight in [false, true] {
            let request = recovery_request(900.0, saved_paused);
            let mut state = DispatchState {
                admitted_file_generation: 8,
                issued_file_generation: 8,
                active_file_generation: Some(8),
                file_loaded_generation: Some(8),
                playback_ready_generation: Some(8),
                ..DispatchState::default()
            };
            state.last_confirmed_time = request.position_secs;
            state.resume.install(8, request.clone());
            state.resume.mark_file_loaded(Some(8), true, Some(8));
            release_pending_resume_if_ready(&mut state);
            let mut flight = if exact_in_flight {
                let exact = pop_next_actor_command(&mut state, &mut VecDeque::new())
                    .expect("released recovery owns an exact seek");
                assert!(matches!(exact, PlayerCmd::SeekAbsolute { .. }));
                begin_resume_seek_observation(&mut state, 8, 44);
                Some(SeekFlight::new(
                    1,
                    44,
                    8,
                    SeekPurpose::CompletionBarrier,
                    false,
                ))
            } else {
                None
            };
            let mut validation = None;
            let mut backlog = VecDeque::new();

            for command in [PlayerCmd::SeekRelative(30.0), PlayerCmd::CyclePause] {
                accept_actor_command(
                    &mut state,
                    command,
                    &mut validation,
                    &mut backlog,
                    &mut flight,
                );
            }

            assert!(
                backlog.is_empty(),
                "recovery lane consumes desired transport"
            );
            assert_eq!(flight.is_some(), exact_in_flight);
            assert!(matches!(
                state.resume.front_command(),
                Some(PlayerCmd::SeekAbsolute {
                    seconds,
                    precision: super::super::super::SeekPrecision::Exact,
                }) if (*seconds - 930.0).abs() < f64::EPSILON
            ));
            assert!(matches!(
                state.resume.back_command(),
                Some(PlayerCmd::SetProperty { name, value })
                    if name == "pause"
                        && value == &serde_json::Value::Bool(!saved_paused)
            ));
            assert!(state.resume.telemetry_is_prepared_for(8, 930.0));

            if exact_in_flight {
                let mut old_exact = flight.take().expect("old exact remains in flight");
                assert!(!old_exact.observe(
                    SeekObservation::CommandReply {
                        request_id: 44,
                        accepted: true,
                    },
                    Some(8),
                ));
                assert!(old_exact.observe(SeekObservation::PlaybackRestart, Some(8)));
                finish_seek_flight(&emit, &mut state, old_exact);
                assert!(state.resume.telemetry_is_prepared_for(8, 930.0));
            }

            accept_actor_command(
                &mut state,
                PlayerCmd::interactive_seek(120.0),
                &mut validation,
                &mut backlog,
                &mut flight,
            );
            assert!(matches!(
                state.resume.front_command(),
                Some(PlayerCmd::SeekAbsolute { seconds, .. })
                    if (*seconds - 120.0).abs() < f64::EPSILON
            ));
            assert!(state.resume.telemetry_is_prepared_for(8, 120.0));
        }
    }
}

#[tokio::test]
async fn rejected_load_commits_exact_generation_stop_before_newer_load() {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(2);
    let emit: EventSink = Arc::new(move |event| {
        let _ = event_tx.try_send(event);
    });
    let task = tokio::spawn(async { LoadValidationOutcome::Superseded });
    let pending = PendingLoadValidation {
        request_id: 18,
        file_generation: 8,
        task,
        resume: resume::ResumeLoad::None,
        source_context: super::super::super::MediaSourceContext::OnDemand,
    };
    let mut state = DispatchState {
        admitted_file_generation: 8,
        issued_file_generation: 7,
        active_file_generation: Some(7),
        file_loaded_generation: Some(7),
        playback_ready_generation: Some(7),
        ..DispatchState::default()
    };
    let mut boundary = finish_load_validation(
        &emit,
        &mut state,
        pending,
        LoadValidationOutcome::Rejected("embedded_credentials".to_owned()),
    );
    assert!(matches!(
        event_rx.try_recv(),
        Ok(PlayerEvent::FileScoped {
            file_generation: 8,
            event,
        }) if matches!(*event, PlayerEvent::Error(_))
    ));

    let newer = PlayerCmd::load(
        "https://example.invalid/c",
        super::super::super::MediaSourceContext::OnDemand,
    );
    supersede_pending_load_boundary(&newer, &mut boundary);
    assert!(matches!(
        boundary,
        Some(PendingLoadBoundary::RejectedStop {
            file_generation: 8,
            ..
        })
    ));
    install_rejected_load_stop_boundary(&mut state, 8);
    assert_eq!(state.issued_file_generation, 8);
    assert_eq!(state.admitted_file_generation, 8);
    assert_eq!(state.active_file_generation, None);
    assert_eq!(reserve_file_generation(&mut state), 9);
}

#[test]
fn actor_fifo_reservations_match_two_queued_load_admissions() {
    let mut state = DispatchState::default();
    let load_b = reserve_file_generation(&mut state);
    let load_c = reserve_file_generation(&mut state);
    assert_eq!((load_b, load_c), (1, 2));
    assert_eq!(state.admitted_file_generation, 2);
    assert_eq!(state.issued_file_generation, 0);
}

#[test]
fn actor_fifo_reservations_match_stop_then_load_batch() {
    let mut state = DispatchState::default();
    let stop = reserve_file_generation(&mut state);
    state.issued_file_generation = stop;
    let load = reserve_file_generation(&mut state);
    assert_eq!((stop, load), (1, 2));
    assert_eq!(state.issued_file_generation, 1);
    assert_eq!(state.admitted_file_generation, 2);
}

#[test]
fn end_file_atomically_drops_recovery_post_load_lane_before_new_stop() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 9,
        active_file_generation: Some(9),
        active_playlist_entry_id: Some(42),
        file_loaded_generation: Some(9),
        playback_ready_generation: Some(9),
        ..DispatchState::default()
    };
    state.resume.install(9, recovery_request(3_600.25, true));
    state.resume.mark_file_loaded(Some(9), true, Some(9));
    state.entry_generations.insert(42, 9);
    release_pending_resume_if_ready(&mut state);
    assert_eq!(state.resume.command_count(), 2);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"stop","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    assert!(state.resume.commands_is_empty());
    assert!(state.resume.dispatch_generation().is_none());

    let stop = PlayerCmd::Stop;
    cancel_resume_post_load_if_superseded(&mut state, &stop);
    let mut validation = None;
    let mut backlog = VecDeque::new();
    let mut flight = None;
    accept_actor_command(&mut state, stop, &mut validation, &mut backlog, &mut flight);
    let next = pop_next_actor_command(&mut state, &mut backlog).expect("Stop is not starved");
    assert!(matches!(next, PlayerCmd::Stop));
}

#[tokio::test]
async fn rejected_handoff_reports_only_a_stable_media_agnostic_reason() {
    let (_generation_tx, generation_rx) = tokio::sync::watch::channel(1u64);
    let outcome = validate_load_until_superseded(
        "https://listener:secret@example.invalid/live?token=signed-secret".to_owned(),
        1,
        generation_rx,
    )
    .await;

    match outcome {
        LoadValidationOutcome::Rejected(reason) => {
            assert_eq!(reason, "embedded_credentials");
            assert!(!reason.contains("secret"));
            assert!(!reason.contains("example.invalid"));
        }
        _ => panic!("credentialed playback URL must be rejected"),
    }
}

#[test]
fn recovery_waits_for_correlated_file_loaded_before_exact_seek_and_pause_restore() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 8,
        active_file_generation: Some(8),
        active_playlist_entry_id: Some(42),
        ..DispatchState::default()
    };
    state.resume.install(8, recovery_request(3_600.25, true));
    state.entry_generations.insert(42, 8);

    release_pending_resume_if_ready(&mut state);
    assert!(state.resume.commands_is_empty());

    dispatch_incoming(
        r#"{"event":"file-loaded","playlist_entry_id":41}"#,
        &emit,
        &mut state,
    );
    assert!(state.resume.commands_is_empty());

    dispatch_incoming(
        r#"{"event":"file-loaded","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );

    assert!(state.resume.pending_request().is_none());
    assert!(matches!(
        state.resume.pop_command(),
        Some(PlayerCmd::SeekAbsolute {
            seconds,
            precision: super::super::super::SeekPrecision::Exact,
        }) if (seconds - 3_600.25).abs() < f64::EPSILON
    ));
    assert!(matches!(
        state.resume.pop_command(),
        Some(PlayerCmd::SetProperty { name, value })
            if name == "pause" && value == serde_json::Value::Bool(true)
    ));
}

#[test]
fn newer_transport_intent_drops_deferred_recovery_commands() {
    let mut state = DispatchState {
        active_file_generation: Some(8),
        ..DispatchState::default()
    };
    state.resume.install(8, recovery_request(3_600.25, false));
    state.resume.mark_file_loaded(Some(8), true, Some(8));
    release_pending_resume_if_ready(&mut state);
    assert_eq!(state.resume.command_count(), 2);
    assert_eq!(state.resume.dispatch_generation(), Some(8));
    let mut validation = None;
    let mut backlog = VecDeque::new();
    let mut interactive_seek = None;

    let stop = PlayerCmd::Stop;
    cancel_resume_post_load_if_superseded(&mut state, &stop);
    accept_actor_command(
        &mut state,
        stop,
        &mut validation,
        &mut backlog,
        &mut interactive_seek,
    );

    assert!(state.resume.pending_request().is_none());
    assert!(state.resume.commands_is_empty());
    assert!(state.resume.dispatch_generation().is_none());
    assert!(matches!(backlog.pop_front(), Some(PlayerCmd::Stop)));
}

#[test]
fn newer_seek_supersedes_in_flight_recovery_exact_and_drops_pause_restore() {
    let mut state = DispatchState::default();
    state.resume.install_dispatching_for_test(
        8,
        VecDeque::from([PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(true),
        }]),
    );
    let request = emergency_request(3_600.25, true);
    install_resume_telemetry(&mut state, 8, &request);
    begin_resume_seek_observation(&mut state, 8, 44);
    let mut flight = Some(SeekFlight::new(
        1,
        44,
        8,
        SeekPurpose::CompletionBarrier,
        false,
    ));
    let mut validation = None;
    let mut backlog = VecDeque::new();
    let seek = PlayerCmd::interactive_seek(120.0);

    cancel_resume_post_load_if_superseded(&mut state, &seek);
    accept_actor_command(&mut state, seek, &mut validation, &mut backlog, &mut flight);

    assert!(state.resume.commands_is_empty());
    assert!(state.resume.dispatch_generation().is_none());
    assert!(!state.resume.telemetry_is_some());
    assert!(flight.as_ref().is_some_and(|flight| flight.superseded));
    assert!(matches!(
        backlog.front(),
        Some(PlayerCmd::SeekAbsolute { .. })
    ));
}

#[test]
fn actor_staging_keeps_only_the_latest_adjacent_interactive_target() {
    let mut validation = None;
    let mut backlog = VecDeque::new();
    for seconds in 0..100 {
        stage_actor_command(
            PlayerCmd::interactive_seek(f64::from(seconds)),
            &mut validation,
            &mut backlog,
        );
    }
    assert_eq!(backlog.len(), 1);
    assert!(matches!(
        backlog.front(),
        Some(PlayerCmd::SeekAbsolute {
            seconds,
            precision: super::super::super::SeekPrecision::InteractiveFast,
        }) if (*seconds - 99.0).abs() < f64::EPSILON
    ));
}

#[test]
fn exact_seek_closes_interactive_coalescing_segment() {
    let mut validation = None;
    let mut backlog = VecDeque::new();
    for cmd in [
        PlayerCmd::interactive_seek(10.0),
        PlayerCmd::interactive_seek(20.0),
        PlayerCmd::exact_seek(30.0),
        PlayerCmd::interactive_seek(40.0),
        PlayerCmd::interactive_seek(50.0),
    ] {
        stage_actor_command(cmd, &mut validation, &mut backlog);
    }
    assert_eq!(backlog.len(), 3);
    assert!(matches!(
        backlog[0],
        PlayerCmd::SeekAbsolute { seconds: 20.0, .. }
    ));
    assert!(matches!(
        backlog[1],
        PlayerCmd::SeekAbsolute {
            seconds: 30.0,
            precision: super::super::super::SeekPrecision::Exact,
        }
    ));
    assert!(matches!(
        backlog[2],
        PlayerCmd::SeekAbsolute { seconds: 50.0, .. }
    ));
}

#[test]
fn load_invalidates_in_flight_and_unsent_interactive_targets_but_keeps_exact_barriers() {
    let mut state = DispatchState::default();
    let mut validation = None;
    let mut backlog = VecDeque::from([
        PlayerCmd::interactive_seek(20.0),
        PlayerCmd::exact_seek(30.0),
        PlayerCmd::interactive_seek(40.0),
    ]);
    let mut flight = Some(SeekFlight::new(
        1,
        12,
        0,
        SeekPurpose::Interactive { target_secs: 20.0 },
        false,
    ));
    accept_actor_command(
        &mut state,
        PlayerCmd::load(
            "https://example.invalid/new",
            super::super::super::MediaSourceContext::OnDemand,
        ),
        &mut validation,
        &mut backlog,
        &mut flight,
    );

    assert!(flight.is_none());
    assert_eq!(backlog.len(), 2);
    assert!(matches!(
        backlog.front(),
        Some(PlayerCmd::SeekAbsolute {
            precision: super::super::super::SeekPrecision::Exact,
            ..
        })
    ));
    assert!(matches!(backlog.back(), Some(PlayerCmd::Load(_))));
}

#[tokio::test]
async fn admitted_new_generation_cancels_validation_before_channel_receive() {
    let (_generation_tx, generation_rx) = watch::channel(2);
    let result =
        validate_load_until_superseded("https://example.com/old".to_owned(), 1, generation_rx)
            .await;

    assert!(matches!(result, LoadValidationOutcome::Superseded));
}
