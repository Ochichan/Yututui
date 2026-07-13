use super::*;

mod audio_output;
mod cache_facts;
mod cache_safety;
mod seek_dispatch;

fn recovery_request(position_secs: f64, paused: bool) -> super::super::recovery::LoadWithResume {
    let mut planner = super::super::recovery::RecoveryPlanner::default();
    let (episode_id, transport_epoch) = planner
        .begin_episode("HTTP error 403 Forbidden", 11, 7)
        .expect("fixture error is conservatively recoverable");
    super::super::recovery::LoadWithResume {
        url: "https://example.invalid/fresh-source".to_owned(),
        position_secs,
        paused,
        source_context: super::super::MediaSourceContext::OnDemand,
        episode_id,
        transport_epoch,
        force_ram_only: false,
    }
}

fn emergency_request(position_secs: f64, paused: bool) -> super::super::recovery::LoadWithResume {
    super::super::recovery::LoadWithResume::emergency(
        "https://example.invalid/emergency-source".to_owned(),
        position_secs,
        paused,
        super::super::MediaSourceContext::OnDemand,
    )
}

fn cache_runtime_for_test(requested: crate::config::LongFormSeekOptimization) -> CacheRuntime {
    CacheRuntime::new(
        crate::player::cache_support::CacheSpawnSupport {
            capability: crate::player::long_form_seek::ControllerCapability::Available(
                crate::player::long_form_seek::CacheOptionFamily::Modern,
            ),
            option_family: Some(crate::player::long_form_seek::CacheOptionFamily::Modern),
            override_source: None,
            cache_dir: None,
            spawn_args: Vec::new(),
        },
        requested,
    )
}

mod recovery_flow;

fn recv_file_event(rx: &mut tokio::sync::mpsc::Receiver<PlayerEvent>) -> (u64, PlayerEvent) {
    match rx.try_recv().expect("file-scoped player event") {
        PlayerEvent::FileScoped {
            file_generation,
            event,
        } => (file_generation, *event),
        _ => panic!("expected file-scoped player event"),
    }
}

fn terminal_events(exit: ActorExit) -> Vec<PlayerEvent> {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = std::sync::Arc::clone(&events);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    });
    finish_actor(exit, &emit);
    drop(emit);
    std::sync::Arc::try_unwrap(events)
        .ok()
        .expect("event sink released")
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn every_transport_failure_emits_exactly_one_sanitized_terminal_event() {
    for exit in [
        ActorExit::Eof,
        ActorExit::Read(io::Error::other("read access_token=secret1")),
        ActorExit::OversizedLine,
        ActorExit::Write {
            operation: "command",
            error: io::Error::new(io::ErrorKind::BrokenPipe, "Bearer secret2"),
        },
    ] {
        let events = terminal_events(exit);
        assert_eq!(events.len(), 1);
        match &events[0] {
            PlayerEvent::TransportClosed(reason) => {
                assert!(!reason.is_empty());
                assert!(!reason.contains("secret1"), "{reason}");
                assert!(!reason.contains("secret2"), "{reason}");
            }
            _ => panic!("expected transport terminal event"),
        }
    }
}

#[test]
fn cache_emergency_emits_one_process_scoped_resume_contract_without_generic_transport_loss() {
    let events = terminal_events(ActorExit::CacheEmergency {
        file_generation: 7,
        position_secs: 3_600.25,
        paused: true,
        reason: super::super::long_form_seek::CacheReason::DisableFailed,
    });

    assert_eq!(events.len(), 1);
    match &events[0] {
        PlayerEvent::CacheEmergency {
            file_generation: 7,
            position_secs,
            paused: true,
            reason: super::super::long_form_seek::CacheReason::DisableFailed,
        } if (*position_secs - 3_600.25).abs() < f64::EPSILON => {}
        _ => panic!("expected one process-scoped cache emergency"),
    }
    assert_eq!(events[0].file_generation(), None);
}

#[test]
fn intentional_command_channel_close_emits_no_terminal_event() {
    assert!(terminal_events(ActorExit::CommandChannelClosed).is_empty());
}

#[test]
fn explicit_close_intent_suppresses_eof_while_internal_sender_is_still_open() {
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let intentional_close = AtomicBool::new(true);
    let exit = transport_exit_or_shutdown(&rx, &intentional_close, ActorExit::Eof);
    assert!(terminal_events(exit).is_empty());
}

#[test]
fn metadata_property_change_is_forwarded() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    dispatch_incoming(
        r#"{"event":"property-change","id":5,"name":"metadata","data":{"icy-title":"Artist - Track"}}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 0);
    match event {
        PlayerEvent::Metadata(value) => {
            assert_eq!(value["icy-title"], "Artist - Track");
        }
        _ => panic!("expected metadata event"),
    }
}

#[test]
fn start_file_waits_for_the_matching_load_reply_before_releasing_properties() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    assert!(remember_pending_load(&mut state, 11, 7, "loadfile"));

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":33.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err(), "uncorrelated property must wait");
    dispatch_incoming(
        r#"{"error":"success","request_id":11,"data":{"playlist_entry_id":42}}"#,
        &emit,
        &mut state,
    );

    assert_eq!(state.active_file_generation, Some(7));
    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 7);
    assert!(matches!(event, PlayerEvent::Duration(Some(33.0))));
}

#[test]
fn playlist_snapshot_correlates_loads_when_entry_ids_are_available() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    assert!(remember_pending_load(&mut state, 11, 5, "loadfile"));
    assert!(remember_pending_load(
        &mut state,
        12,
        5,
        "loadfile identity"
    ));

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":91}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":55.0}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"error":"success","request_id":11}"#, &emit, &mut state);
    assert!(rx.try_recv().is_err());
    dispatch_incoming(
        r#"{"error":"success","request_id":12,"data":[{"filename":"track","current":true,"playing":true,"id":91}]}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 5);
    assert!(matches!(event, PlayerEvent::Duration(Some(55.0))));
}

#[test]
fn redirect_entry_cannot_consume_a_newer_load_generation() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_playlist_entry_id: Some(10),
        active_file_generation: Some(1),
        ..DispatchState::default()
    };
    state.entry_generations.insert(10, 1);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"redirect","playlist_entry_id":10,"playlist_insert_id":42,"playlist_insert_num_entries":2}"#,
        &emit,
        &mut state,
    );
    assert!(remember_pending_load(&mut state, 12, 2, "loadfile"));
    dispatch_incoming(
        r#"{"error":"success","request_id":12,"data":{"playlist_entry_id":77}}"#,
        &emit,
        &mut state,
    );

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"end-file","reason":"eof","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":77}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":44.0}"#,
        &emit,
        &mut state,
    );

    let (redirect_generation, redirect_terminal) = recv_file_event(&mut rx);
    assert_eq!(redirect_generation, 1);
    assert!(matches!(redirect_terminal, PlayerEvent::Eof));
    let (new_generation, duration) = recv_file_event(&mut rx);
    assert_eq!(new_generation, 2);
    assert!(matches!(duration, PlayerEvent::Duration(Some(44.0))));
}

#[test]
fn redirect_mapping_releases_events_when_child_started_first() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    state.entry_generations.insert(10, 1);

    dispatch_incoming(
        r#"{"event":"start-file","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":61.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());

    dispatch_incoming(
        r#"{"event":"end-file","reason":"redirect","playlist_entry_id":10,"playlist_insert_id":42,"playlist_insert_num_entries":1}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 1);
    assert!(matches!(event, PlayerEvent::Duration(Some(61.0))));
}

#[test]
fn unknown_old_entry_terminal_is_not_relabelled_as_current() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_playlist_entry_id: Some(77),
        active_file_generation: Some(2),
        ..DispatchState::default()
    };
    state.entry_generations.insert(77, 2);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"eof","playlist_entry_id":42}"#,
        &emit,
        &mut state,
    );

    assert!(rx.try_recv().is_err());
    assert_eq!(state.active_file_generation, Some(2));
}

#[test]
fn mpv_032_uses_selected_filename_after_id_less_start_file() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    state.legacy_loads.push_back(LegacyLoad {
        generation: 5,
        url: "https://media.example/track-a".to_owned(),
        replied: false,
    });
    assert!(remember_pending_load(&mut state, 11, 5, "loadfile"));
    assert!(remember_pending_load(
        &mut state,
        12,
        5,
        "loadfile identity"
    ));

    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":75.0}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"error":"success","request_id":11}"#, &emit, &mut state);
    assert!(rx.try_recv().is_err());
    dispatch_incoming(
        r#"{"error":"success","request_id":12,"data":[{"filename":"https://media.example/track-a","current":true,"playing":true}]}"#,
        &emit,
        &mut state,
    );

    assert_eq!(state.playlist_identity_mode, PlaylistIdentityMode::Legacy);
    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 5);
    assert!(matches!(event, PlayerEvent::Duration(Some(75.0))));
}

#[test]
fn mpv_032_eof_property_emits_one_natural_end_for_the_active_generation() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_file_generation: Some(5),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        ..DispatchState::default()
    };

    for data in [true, true, false, true] {
        dispatch_incoming(
            &format!(r#"{{"event":"property-change","id":10,"name":"eof-reached","data":{data}}}"#),
            &emit,
            &mut state,
        );
    }
    // mpv 0.32 sends a bare end-file and then becomes idle. The EOF latch keeps that ordered
    // compatibility boundary from being misclassified as an error.
    dispatch_incoming(r#"{"event":"end-file"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"event":"idle"}"#, &emit, &mut state);

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 5);
    assert!(matches!(event, PlayerEvent::Eof));
    assert!(rx.try_recv().is_err());
}

#[test]
fn keep_open_eof_property_also_advances_modern_entry_id_playback_once() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_file_generation: Some(7),
        playlist_identity_mode: PlaylistIdentityMode::EntryIds,
        ..DispatchState::default()
    };

    dispatch_incoming(
        r#"{"event":"property-change","id":10,"name":"eof-reached","data":true}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"event":"end-file","reason":"eof"}"#, &emit, &mut state);

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 7);
    assert!(matches!(event, PlayerEvent::Eof));
    assert!(rx.try_recv().is_err());
}

#[test]
fn explicit_modern_eof_latches_before_a_late_property_notification() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_file_generation: Some(7),
        playlist_identity_mode: PlaylistIdentityMode::EntryIds,
        ..DispatchState::default()
    };

    dispatch_incoming(r#"{"event":"end-file","reason":"eof"}"#, &emit, &mut state);
    dispatch_incoming(
        r#"{"event":"property-change","id":10,"name":"eof-reached","data":true}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 7);
    assert!(matches!(event, PlayerEvent::Eof));
    assert!(rx.try_recv().is_err());
}

#[test]
fn current_mpv_loading_failed_event_emits_url_free_generic_source_failure() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 7,
        active_file_generation: Some(7),
        active_playlist_entry_id: Some(70),
        playlist_identity_mode: PlaylistIdentityMode::EntryIds,
        ..DispatchState::default()
    };
    state.entry_generations.insert(70, 7);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"error","file_error":"loading failed","playlist_entry_id":70}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 7);
    assert!(matches!(
        event,
        PlayerEvent::Error(error)
            if error == super::super::recovery::GENERIC_LOADING_FAILURE
    ));
}

#[test]
fn mpv_032_reasonless_end_becomes_error_only_after_idle() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 5,
        active_file_generation: Some(5),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        ..DispatchState::default()
    };

    dispatch_incoming(r#"{"event":"end-file"}"#, &emit, &mut state);
    assert!(
        rx.try_recv().is_err(),
        "bare end waits for its lifecycle boundary"
    );
    dispatch_incoming(r#"{"event":"idle"}"#, &emit, &mut state);

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 5);
    assert!(matches!(
        event,
        PlayerEvent::Error(error)
            if error == super::super::recovery::GENERIC_LOADING_FAILURE
    ));
    assert!(rx.try_recv().is_err());
}

#[test]
fn mpv_032_late_eof_property_resolves_a_pending_reasonless_end() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 5,
        active_file_generation: Some(5),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        ..DispatchState::default()
    };

    dispatch_incoming(r#"{"event":"end-file"}"#, &emit, &mut state);
    dispatch_incoming(
        r#"{"event":"property-change","id":10,"name":"eof-reached","data":true}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"event":"idle"}"#, &emit, &mut state);

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 5);
    assert!(matches!(event, PlayerEvent::Eof));
    assert!(rx.try_recv().is_err());
}

#[test]
fn mpv_032_reasonless_end_followed_by_start_is_a_redirect_not_an_error() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 5,
        active_file_generation: Some(5),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        ..DispatchState::default()
    };

    dispatch_incoming(r#"{"event":"end-file"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"event":"idle"}"#, &emit, &mut state);

    assert!(rx.try_recv().is_err());
    assert_eq!(state.active_file_generation, Some(5));
}

#[test]
fn mpv_032_reasonless_end_after_replacement_is_not_a_playback_error() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        issued_file_generation: 6,
        active_file_generation: Some(5),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        ..DispatchState::default()
    };

    dispatch_incoming(r#"{"event":"end-file"}"#, &emit, &mut state);
    dispatch_incoming(r#"{"event":"idle"}"#, &emit, &mut state);

    assert!(rx.try_recv().is_err());
}

#[test]
fn mpv_032_load_reply_prunes_a_rapid_load_that_never_started() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    for (generation, url) in [
        (1, "https://media.example/skipped"),
        (2, "https://media.example/current"),
    ] {
        state.legacy_loads.push_back(LegacyLoad {
            generation,
            url: url.to_owned(),
            replied: false,
        });
    }
    assert!(remember_pending_load(&mut state, 11, 1, "loadfile"));
    assert!(remember_pending_load(&mut state, 13, 2, "loadfile"));

    dispatch_incoming(r#"{"error":"success","request_id":11}"#, &emit, &mut state);
    dispatch_incoming(r#"{"error":"success","request_id":13}"#, &emit, &mut state);
    state.legacy_latest_playlist_filename = Some("https://media.example/current".to_owned());
    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":80.0}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 2);
    assert!(matches!(event, PlayerEvent::Duration(Some(80.0))));
    assert!(state.legacy_loads.is_empty());
}

#[test]
fn mpv_032_playlist_change_waits_for_matching_load_reply() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_file_generation: Some(1),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        legacy_latest_playlist_filename: Some("https://media.example/new".to_owned()),
        ..DispatchState::default()
    };
    state.legacy_loads.push_back(LegacyLoad {
        generation: 2,
        url: "https://media.example/new".to_owned(),
        replied: false,
    });
    assert!(remember_pending_load(&mut state, 21, 2, "loadfile"));

    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":90.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());

    dispatch_incoming(r#"{"error":"success","request_id":21}"#, &emit, &mut state);

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 2);
    assert!(matches!(event, PlayerEvent::Duration(Some(90.0))));
}

#[test]
fn mpv_032_does_not_reactivate_old_generation_between_rapid_starts() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        active_file_generation: Some(0),
        playlist_identity_mode: PlaylistIdentityMode::Legacy,
        ..DispatchState::default()
    };
    for (generation, url, request_id) in [
        (1, "https://media.example/one", 31),
        (2, "https://media.example/two", 32),
    ] {
        state.legacy_loads.push_back(LegacyLoad {
            generation,
            url: url.to_owned(),
            replied: false,
        });
        assert!(remember_pending_load(
            &mut state, request_id, generation, "loadfile"
        ));
    }

    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    dispatch_incoming(
        r#"{"event":"property-change","id":9,"name":"playlist","data":[{"filename":"https://media.example/one","current":true,"playing":true}]}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(r#"{"error":"success","request_id":31}"#, &emit, &mut state);
    assert_eq!(state.active_file_generation, None);

    dispatch_incoming(r#"{"event":"start-file"}"#, &emit, &mut state);
    assert_eq!(state.active_file_generation, None);
    dispatch_incoming(
        r#"{"event":"property-change","id":9,"name":"playlist","data":[{"filename":"https://media.example/two","current":true,"playing":true}]}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":95.0}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());
    dispatch_incoming(r#"{"error":"success","request_id":32}"#, &emit, &mut state);

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 2);
    assert!(matches!(event, PlayerEvent::Duration(Some(95.0))));
}

#[test]
fn ordinary_reply_tracking_never_evicts_load_identity() {
    let mut state = DispatchState::default();
    for request_id in 1..=128 {
        assert!(remember_pending_load(
            &mut state,
            request_id,
            request_id,
            "protected load identity"
        ));
    }

    remember_pending_command(&mut state, 500, "ordinary diagnostic");
    assert_eq!(state.pending.len(), 128);
    assert!(!state.pending.contains_key(&500));
    assert!(!remember_pending_load(
        &mut state,
        501,
        501,
        "new protected identity"
    ));
    assert!((1..=128).all(|request_id| state.pending.contains_key(&request_id)));
}

#[test]
fn audio_device_control_replies_are_not_evicted_by_diagnostics() {
    let mut state = DispatchState::default();
    for request_id in 1..=128 {
        remember_pending_command(&mut state, request_id, "ordinary");
    }
    assert!(remember_pending_audio_device_refresh(&mut state, 200));
    remember_pending_audio_device_selection(
        &mut state,
        201,
        PendingAudioDeviceSelection {
            correlation_id: 9,
            device: None,
            phase: AudioDeviceSelectionPhase::ClearOutput,
        },
    )
    .unwrap();
    remember_pending_command(&mut state, 202, "new ordinary");

    assert_eq!(state.pending.len(), 128);
    assert!(state.pending.contains_key(&200));
    assert!(state.pending.contains_key(&201));
}

#[test]
fn tracked_reply_is_non_evictable_and_resolves_exact_success_or_failure() {
    let mut state = DispatchState::default();
    let success = crate::util::command_barrier::CommandBarrier::pending();
    remember_pending_tracked(
        &mut state,
        1,
        "set_property stream-record".to_owned(),
        success.signal(),
    )
    .unwrap();
    for request_id in 2..=128 {
        remember_pending_command(&mut state, request_id, "ordinary");
    }
    remember_pending_command(&mut state, 500, "new ordinary");
    assert!(state.pending.contains_key(&1));
    let emit: EventSink = std::sync::Arc::new(|_| {});
    dispatch_incoming(r#"{"error":"success","request_id":1}"#, &emit, &mut state);
    assert!(success.wait_for_test(std::time::Duration::ZERO).is_ok());

    let failure = crate::util::command_barrier::CommandBarrier::pending();
    remember_pending_tracked(
        &mut state,
        501,
        "set_property stream-record".to_owned(),
        failure.signal(),
    )
    .unwrap();
    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":501}"#,
        &emit,
        &mut state,
    );
    assert!(
        failure
            .wait_for_test(std::time::Duration::ZERO)
            .unwrap_err()
            .contains("invalid parameter")
    );
}

#[test]
fn internal_terminal_contract_resolves_success_and_recycles_on_rejection() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState {
        issued_file_generation: 9,
        ..DispatchState::default()
    };
    remember_pending_terminal(
        &mut state,
        1,
        9,
        "set_property pause".to_owned(),
        "recovery pause restore",
    )
    .unwrap();

    dispatch_incoming(r#"{"error":"success","request_id":1}"#, &emit, &mut state);

    assert!(state.pending.is_empty());
    assert!(state.terminal_failure.is_none());

    remember_pending_terminal(&mut state, 2, 9, "stop".to_owned(), "rejected-load stop").unwrap();
    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":2}"#,
        &emit,
        &mut state,
    );

    assert!(matches!(
        state.terminal_failure.take(),
        Some(ActorExit::InternalCommandFailed {
            operation: "rejected-load stop",
            rejected: true,
        })
    ));
}

#[test]
fn ordinary_command_rejection_has_no_internal_terminal_contract() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let mut state = DispatchState::default();
    remember_pending_command(&mut state, 1, "set volume");

    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":1}"#,
        &emit,
        &mut state,
    );

    assert!(state.terminal_failure.is_none());
}

#[test]
fn expired_internal_terminal_contract_recycles_with_timeout_reason() {
    let mut state = DispatchState::default();
    state.pending.insert(
        1,
        PendingCommand {
            label: "set_property pause".to_owned(),
            file_generation: Some(9),
            acknowledgement: None,
            audio_output: None,
            terminal_contract: Some(PendingTerminalContract {
                operation: "recovery pause restore",
                deadline: Instant::now() - Duration::from_millis(1),
            }),
        },
    );

    let failure = expired_terminal_failure(&state, Instant::now())
        .expect("expired internal reply must be terminal");
    assert!(matches!(
        &failure,
        ActorExit::InternalCommandFailed {
            operation: "recovery pause restore",
            rejected: false,
        }
    ));
    assert!(matches!(
        terminal_events(failure).as_slice(),
        [PlayerEvent::TransportClosed(reason)]
            if reason.contains("recovery pause restore") && reason.contains("timed out")
    ));
}

#[test]
fn actor_exit_fails_every_pending_tracked_reply() {
    let mut state = DispatchState::default();
    let first = crate::util::command_barrier::CommandBarrier::pending();
    let second = crate::util::command_barrier::CommandBarrier::pending();
    remember_pending_tracked(&mut state, 1, "first".to_owned(), first.signal()).unwrap();
    remember_pending_tracked(&mut state, 2, "second".to_owned(), second.signal()).unwrap();

    let emit: EventSink = std::sync::Arc::new(|_| {});
    fail_pending_commands(&state, &emit, "actor cancelled");

    assert_eq!(
        first.wait_for_test(std::time::Duration::ZERO).unwrap_err(),
        "actor cancelled"
    );
    assert_eq!(
        second.wait_for_test(std::time::Duration::ZERO).unwrap_err(),
        "actor cancelled"
    );
}

#[test]
fn actor_exit_and_timeout_finish_correlated_audio_device_requests() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    remember_pending_audio_device_selection(
        &mut state,
        70,
        PendingAudioDeviceSelection {
            correlation_id: 700,
            device: Some("wasapi/{headphones}".to_owned()),
            phase: AudioDeviceSelectionPhase::SetDevice,
        },
    )
    .unwrap();
    fail_pending_commands(&state, &emit, "mpv IPC closed access_token=secret");

    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceSelectionResult {
            correlation_id: 700,
            result: Err(error),
            ..
        }) if error.contains("<redacted>") && !error.contains("secret")
    ));

    let mut state = DispatchState::default();
    remember_pending_audio_device_selection(
        &mut state,
        71,
        PendingAudioDeviceSelection {
            correlation_id: 701,
            device: None,
            phase: AudioDeviceSelectionPhase::ClearOutput,
        },
    )
    .unwrap();
    state.audio_output.selection_request_id = Some(71);
    state.audio_output.selection_deadline = Some(Instant::now());
    timeout_audio_device_selection(&mut state, &emit);

    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceSelectionResult {
            correlation_id: 701,
            device: None,
            result: Err(error),
        }) if error.contains("timed out")
    ));
    assert!(state.pending.is_empty());
    assert!(state.audio_output.selection_request_id.is_none());
}

#[test]
fn playlist_correlation_maps_are_bounded_and_keep_the_active_entry() {
    let active_entry = 10_000;
    let mut state = DispatchState {
        active_playlist_entry_id: Some(active_entry),
        ..DispatchState::default()
    };
    insert_entry_generation(&mut state, active_entry, 9);
    for entry_id in 1..=(ENTRY_GENERATION_CAPACITY as u64 + 64) {
        insert_entry_generation(&mut state, entry_id, entry_id);
    }
    assert_eq!(state.entry_generations.len(), ENTRY_GENERATION_CAPACITY);
    assert_eq!(state.entry_generations.get(&active_entry), Some(&9));

    for old_entry_id in 1..=(PENDING_REDIRECT_CAPACITY as u64 + 32) {
        remember_pending_redirect(&mut state, old_entry_id, old_entry_id + 1_000, 1);
    }
    assert_eq!(state.pending_redirects.len(), PENDING_REDIRECT_CAPACITY);
}

#[test]
fn time_pos_dedups_to_whole_seconds() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    for data in ["1.1", "1.4", "1.9", "2.0"] {
        let line =
            format!(r#"{{"event":"property-change","id":1,"name":"time-pos","data":{data}}}"#);
        dispatch_incoming(&line, &emit, &mut state);
    }

    // 1.1 emits (second 1), 1.4/1.9 dedup away, 2.0 emits (second 2).
    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::TimePos(t) if t == 1.1));
    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::TimePos(t) if t == 2.0));
    assert!(rx.try_recv().is_err());
}

#[test]
fn numeric_perf_window_counts_borrowed_fallback_and_forwarded_lines() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        numeric_perf: Some(NumericPerfWindow::new()),
        ..DispatchState::default()
    };

    for line in [
        r#"{"event":"property-change","name":"time-pos","data":1.1}"#,
        r#"{"event":"property-change","name":"time-pos","data":1.8}"#,
        r#"{"event":"property-change","name":"demuxer-cache-time","data":100.2}"#,
        r#"{"event":"property-change","name":"demuxer-cache-time","data":null}"#,
        // An escaped property name cannot be borrowed by the fast-path struct, but the
        // allocating generic parser still recognizes it and must be represented in stats.
        r#"{"event":"property-change","name":"time-\u0070os","data":2.0}"#,
    ] {
        dispatch_incoming(line, &emit, &mut state);
    }

    let perf = state.numeric_perf.as_ref().expect("perf counters enabled");
    assert_eq!(perf.raw_time_pos, 3);
    assert_eq!(perf.raw_cache_time, 2);
    assert_eq!(perf.borrowed_fast_path, 4);
    assert_eq!(perf.generic_fallback, 1);
    assert_eq!(perf.forwarded_time_pos, 2);
    assert_eq!(perf.forwarded_cache_time, 2);
    assert_eq!(std::iter::from_fn(|| rx.try_recv().ok()).count(), 4);
}

#[test]
fn null_time_pos_emits_nothing() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        last_sent_time_sec: Some(3),
        ..DispatchState::default()
    };

    dispatch_incoming(
        r#"{"event":"property-change","id":1,"name":"time-pos","data":null}"#,
        &emit,
        &mut state,
    );

    assert!(rx.try_recv().is_err());
    assert_eq!(state.last_sent_time_sec, Some(3));
}

#[test]
fn invalid_non_finite_time_pos_emits_nothing() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    for raw in ["NaN", "Infinity", "-Infinity"] {
        let line =
            format!(r#"{{"event":"property-change","id":1,"name":"time-pos","data":{raw}}}"#);
        dispatch_incoming(&line, &emit, &mut state);
    }

    assert!(rx.try_recv().is_err());
    assert_eq!(state.last_sent_time_sec, None);
}

#[test]
fn negative_time_pos_is_normalized_before_emit() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    dispatch_incoming(
        r#"{"event":"property-change","id":1,"name":"time-pos","data":-4.25}"#,
        &emit,
        &mut state,
    );

    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::TimePos(t) if t == 0.0));
    assert_eq!(state.last_sent_time_sec, Some(0));
}

#[test]
fn cache_time_forwards_and_dedups_to_whole_seconds() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    for data in ["100.2", "100.7", "101.3"] {
        let line = format!(
            r#"{{"event":"property-change","id":6,"name":"demuxer-cache-time","data":{data}}}"#
        );
        dispatch_incoming(&line, &emit, &mut state);
    }

    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::CacheTime(Some(t)) if t == 100.2));
    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::CacheTime(Some(t)) if t == 101.3));
    assert!(rx.try_recv().is_err());
}

#[test]
fn null_cache_time_reports_loss_once() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        last_sent_cache_sec: Some(42),
        ..DispatchState::default()
    };

    let line = r#"{"event":"property-change","id":6,"name":"demuxer-cache-time","data":null}"#;
    // First null after a real value → the loss is reported and the dedup resets…
    dispatch_incoming(line, &emit, &mut state);
    assert!(matches!(
        recv_file_event(&mut rx).1,
        PlayerEvent::CacheTime(None)
    ));
    assert_eq!(state.last_sent_cache_sec, None);
    // …and repeated nulls (a stream that never has a cache) stay silent.
    dispatch_incoming(line, &emit, &mut state);
    assert!(rx.try_recv().is_err());
}

#[test]
fn null_duration_reports_loss_once_after_a_real_value() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    let null_line = r#"{"event":"property-change","id":2,"name":"duration","data":null}"#;
    // A null before any real value (observe echo on an unloaded player) stays silent.
    dispatch_incoming(null_line, &emit, &mut state);
    assert!(rx.try_recv().is_err());

    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":180.5}"#,
        &emit,
        &mut state,
    );
    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::Duration(Some(d)) if d == 180.5));

    // First null after a real value → the loss is forwarded once, then silence.
    dispatch_incoming(null_line, &emit, &mut state);
    assert!(matches!(
        recv_file_event(&mut rx).1,
        PlayerEvent::Duration(None)
    ));
    dispatch_incoming(null_line, &emit, &mut state);
    assert!(rx.try_recv().is_err());
}

#[test]
fn negative_duration_is_normalized_without_latching_known_duration() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":-10}"#,
        &emit,
        &mut state,
    );
    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::Duration(Some(d)) if d == 0.0));
    assert!(!state.duration_known);

    dispatch_incoming(
        r#"{"event":"property-change","id":2,"name":"duration","data":null}"#,
        &emit,
        &mut state,
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn negative_cache_time_is_normalized_before_emit() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    dispatch_incoming(
        r#"{"event":"property-change","id":6,"name":"demuxer-cache-time","data":-1}"#,
        &emit,
        &mut state,
    );

    assert!(matches!(recv_file_event(&mut rx).1, PlayerEvent::CacheTime(Some(t)) if t == 0.0));
    assert_eq!(state.last_sent_cache_sec, Some(0));
}

#[test]
fn end_file_stop_resets_dedup_state_without_events() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState {
        last_sent_time_sec: Some(30),
        last_sent_cache_sec: Some(42),
        duration_known: true,
        ..DispatchState::default()
    };

    // Externally-caused stop/quit/redirect must clear the per-file dedup state (or the
    // next load's first second is swallowed) while emitting nothing — our own Stop
    // command already drives the reducers' stop paths.
    for reason in ["stop", "quit", "redirect", "some-future-reason"] {
        let line = format!(r#"{{"event":"end-file","reason":"{reason}"}}"#);
        state.last_sent_time_sec = Some(30);
        dispatch_incoming(&line, &emit, &mut state);
        assert!(rx.try_recv().is_err(), "reason {reason} must emit nothing");
        assert_eq!(state.last_sent_time_sec, None);
        assert_eq!(state.last_sent_cache_sec, None);
        assert!(!state.duration_known);
    }
}

#[test]
fn failed_loadfile_reply_emits_url_free_generic_source_failure() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    assert!(remember_pending_load(&mut state, 11, 1, "loadfile"));

    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":11}"#,
        &emit,
        &mut state,
    );

    let (generation, event) = recv_file_event(&mut rx);
    assert_eq!(generation, 1);
    assert!(matches!(
        event,
        PlayerEvent::Error(error)
            if error == crate::player::recovery::GENERIC_LOADING_FAILURE
    ));
    assert!(!state.pending.contains_key(&11));
}

#[test]
fn failed_af_command_reply_emits_nothing() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    remember_pending_command(&mut state, 12, "af-command");

    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":12}"#,
        &emit,
        &mut state,
    );

    assert!(rx.try_recv().is_err());
    assert!(!state.pending.contains_key(&12));
}

#[test]
fn success_reply_emits_nothing_and_removes_pending() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    remember_pending_command(&mut state, 13, "seek");

    dispatch_incoming(r#"{"error":"success","request_id":13}"#, &emit, &mut state);

    assert!(rx.try_recv().is_err());
    assert!(!state.pending.contains_key(&13));
}

#[test]
fn unknown_reply_id_is_ignored() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    dispatch_incoming(
        r#"{"error":"invalid parameter","request_id":99}"#,
        &emit,
        &mut state,
    );

    assert!(rx.try_recv().is_err());
    assert!(state.pending.is_empty());
}
