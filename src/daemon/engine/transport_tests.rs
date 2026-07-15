use super::*;

fn test_player(capacity: usize) -> (PlayerRuntime, tokio::sync::mpsc::Receiver<PlayerCmd>) {
    let (tx, rx) = tokio::sync::mpsc::channel(capacity);
    (
        PlayerRuntime {
            handle: PlayerHandle::test_handle(tx),
            _guard: None,
        },
        rx,
    )
}

fn install_test_player(engine: &mut DaemonEngine) -> tokio::sync::mpsc::Receiver<PlayerCmd> {
    let (player, rx) = test_player(8);
    engine.player = Some(player);
    rx
}

fn queue_test_player_start(engine: &mut DaemonEngine) -> tokio::sync::mpsc::Receiver<PlayerCmd> {
    let (player, rx) = test_player(8);
    engine.test_player_starts.push_back(player);
    rx
}

async fn recv_player_command(rx: &mut tokio::sync::mpsc::Receiver<PlayerCmd>) -> PlayerCmd {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("player command timed out")
        .expect("player command lane closed")
}

async fn assert_setup_then_load(rx: &mut tokio::sync::mpsc::Receiver<PlayerCmd>, paused: bool) {
    assert!(matches!(
        recv_player_command(rx).await,
        PlayerCmd::SetVolume(50)
    ));
    assert!(matches!(
        recv_player_command(rx).await,
        PlayerCmd::SetAudioFilter(_)
    ));
    assert!(matches!(recv_player_command(rx).await, PlayerCmd::Load(_)));
    if paused {
        assert!(matches!(
            recv_player_command(rx).await,
            PlayerCmd::CyclePause
        ));
    }
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn load_current_carries_owner_known_live_context_for_finite_radio_sources() {
    let mut engine = super::tests::engine_with_queue(&[]);
    engine
        .queue
        .set(vec![super::tests::radio_station("finite-dvr")], 0);
    let mut rx = install_test_player(&mut engine);

    engine.load_current_loaded().expect("radio load admitted");

    let load = match recv_player_command(&mut rx).await {
        PlayerCmd::Load(load) => load,
        _ => panic!("radio queue item must issue a Load"),
    };
    assert_eq!(
        load.source_context(),
        crate::player::MediaSourceContext::Live
    );
}

#[tokio::test]
async fn actual_mpv_generic_midtrack_failure_reloads_once_without_resetting_position() {
    let mut engine = super::tests::engine_with_queue(&["seed", "next"]);
    let mut rx = install_test_player(&mut engine);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.time_pos = Some(3_600.25);
    engine.playback.time_pos_at = Some(Instant::now());
    engine.playback.duration = Some(7_200.0);
    engine.playback.paused = true;
    let epoch = engine.playback.position_epoch;
    let history_len = engine.library.history.len();

    let effects = engine
        .handle_player_event(PlayerEvent::Error(
            crate::player::recovery::GENERIC_LOADING_FAILURE.to_owned(),
        ))
        .await;
    assert!(effects.is_empty());
    let request = match recv_player_command(&mut rx).await {
        PlayerCmd::LoadWithResume(request) => request,
        _ => panic!("midtrack generic load failure must use LoadWithResume"),
    };
    assert_eq!(request.position_secs, 3_600.25);
    assert!(request.paused);
    assert_eq!(
        request.source_context,
        crate::player::MediaSourceContext::OnDemand
    );
    assert_eq!(engine.playback.time_pos, Some(3_600.25));
    assert_eq!(engine.playback.duration, Some(7_200.0));
    assert!(engine.playback.paused);
    assert_eq!(engine.playback.position_epoch, epoch + 1);
    assert_eq!(engine.library.history.len(), history_len);

    engine
        .handle_player_event(PlayerEvent::TimePos(3_600.5))
        .await;
    assert_eq!(engine.playback.position_epoch, epoch + 1);

    let effects = engine
        .handle_player_event(PlayerEvent::Error(
            "HTTP error 410 Gone while reading replacement".to_owned(),
        ))
        .await;
    assert!(effects.is_empty());
    assert!(matches!(
        recv_player_command(&mut rx).await,
        PlayerCmd::Load(_)
    ));
    assert_eq!(engine.loaded_video_id.as_deref(), Some("next"));
    assert!(
        rx.try_recv().is_err(),
        "a second recovery-origin failure must not issue LoadWithResume"
    );
}

#[tokio::test]
async fn actual_mpv_generic_initial_load_failure_never_uses_resume() {
    let mut engine = super::tests::engine_with_queue(&["seed", "next"]);
    let mut rx = install_test_player(&mut engine);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.time_pos = None;

    engine
        .handle_player_event(PlayerEvent::Error(
            crate::player::recovery::GENERIC_LOADING_FAILURE.to_owned(),
        ))
        .await;

    assert!(matches!(
        recv_player_command(&mut rx).await,
        PlayerCmd::Load(_)
    ));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn transport_terminal_automatically_restarts_and_replays_without_duplicate_effects() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    let song = engine.queue.current().cloned().expect("current song");
    engine.library.record_play(&song);
    engine.loaded_video_id = Some(song.video_id.clone());
    engine.playback.paused = true;
    engine.playback.time_pos = Some(42.0);
    engine.playback.time_pos_at = Some(Instant::now());
    engine.playback.duration = Some(180.0);
    let old_rx = install_test_player(&mut engine);
    let mut replacement_rx = queue_test_player_start(&mut engine);

    let history_len = engine.library.history.len();
    let play_count = engine.signals.play_count("seed");
    let artist_weight = engine.signals.artist_weight("artist");
    let epoch = engine.playback.position_epoch;

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed(
            "broken pipe access_token=secret1".to_owned(),
        ))
        .await;

    assert!(effects.is_empty());
    assert!(engine.player.is_some());
    assert!(old_rx.is_closed());
    assert_eq!(engine.loaded_video_id.as_deref(), Some("seed"));
    assert_eq!(engine.playback.time_pos, None);
    assert_eq!(engine.playback.duration, None);
    assert!(
        engine.playback.paused,
        "pause intent must survive transport loss"
    );
    assert_eq!(engine.playback.position_epoch, epoch + 1);
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);
    assert!(
        !engine
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("secret1")
    );
    assert_setup_then_load(&mut replacement_rx, true).await;
    assert_eq!(engine.library.history.len(), history_len);
    assert_eq!(engine.signals.play_count("seed"), play_count);
    assert_eq!(engine.signals.artist_weight("artist"), artist_weight);
}

#[tokio::test]
async fn normal_load_rearms_transport_recovery_after_a_stable_replacement() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    let _old_rx = install_test_player(&mut engine);
    let mut first_replacement_rx = queue_test_player_start(&mut engine);

    assert!(
        engine
            .handle_player_event(PlayerEvent::TransportClosed("first".to_owned()))
            .await
            .is_empty()
    );
    assert_setup_then_load(&mut first_replacement_rx, false).await;
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);

    engine
        .load_current_loaded()
        .expect("ordinary load should be admitted");
    assert!(matches!(
        recv_player_command(&mut first_replacement_rx).await,
        PlayerCmd::Load(_)
    ));
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Armed);

    let mut second_replacement_rx = queue_test_player_start(&mut engine);
    assert!(
        engine
            .handle_player_event(PlayerEvent::TransportClosed("second".to_owned()))
            .await
            .is_empty()
    );
    assert_setup_then_load(&mut second_replacement_rx, false).await;
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);
}

#[tokio::test]
async fn ordinary_load_supersedes_an_automatic_recovery_for_a_different_item() {
    let mut engine = super::tests::engine_with_queue(&["old", "new"]);
    engine.loaded_video_id = Some("old".to_owned());
    let old_rx = install_test_player(&mut engine);
    let mut replacement_rx = queue_test_player_start(&mut engine);

    let generation = engine
        .handle_transport_closed("old transport".to_owned())
        .expect("loaded transport close should schedule recovery");
    assert!(old_rx.is_closed());
    assert_eq!(
        engine.queue.next(false).map(|song| song.video_id.as_str()),
        Some("new")
    );

    let effects = engine.attempt_transport_recovery(generation).await;
    assert!(matches!(
        effects.as_slice(),
        [EngineEffect::TransportRecoveryRetry {
            generation: retry_generation,
            ..
        }] if *retry_generation == generation
    ));
    assert_eq!(engine.loaded_video_id, None);
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.video_id == "old" && recovery.attempts == 1
    ));
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetVolume(50)
    ));
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetAudioFilter(_)
    ));
    assert!(
        replacement_rx.try_recv().is_err(),
        "automatic recovery must not load a different queue item"
    );

    engine
        .load_current_loaded()
        .expect("ordinary load should supersede stale recovery");
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::Load(_)
    ));
    assert_eq!(engine.loaded_video_id.as_deref(), Some("new"));
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Armed);
    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert!(replacement_rx.try_recv().is_err());
}

#[tokio::test]
async fn cache_emergency_restarts_once_at_exact_position_forced_to_ram_only() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = true;
    engine.playback.time_pos = Some(3_600.25);
    engine.playback.duration = Some(7_200.0);
    let epoch = engine.playback.position_epoch;
    let old_rx = install_test_player(&mut engine);
    let mut replacement_rx = queue_test_player_start(&mut engine);

    let effects = engine
        .handle_player_event(PlayerEvent::CacheEmergency {
            file_generation: 0,
            position_secs: 3_600.25,
            paused: true,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        })
        .await;

    assert!(effects.is_empty());
    assert!(old_rx.is_closed());
    assert_eq!(engine.playback.time_pos, Some(3_600.25));
    assert_eq!(engine.playback.position_epoch, epoch + 1);
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetVolume(50)
    ));
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetAudioFilter(_)
    ));
    let request = match recv_player_command(&mut replacement_rx).await {
        PlayerCmd::LoadWithResume(request) => request,
        _ => panic!("cache emergency must use a correlated resume load"),
    };
    assert!((request.position_secs - 3_600.25).abs() < f64::EPSILON);
    assert!(request.paused);
    assert!(request.forces_ram_only());
    assert_eq!(
        request.source_context,
        crate::player::MediaSourceContext::OnDemand
    );
    assert!(replacement_rx.try_recv().is_err());
}

#[tokio::test]
async fn cache_emergency_cannot_overwrite_newer_same_generation_daemon_transport() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    // Model a seek/pause already admitted by the daemon while the actor's older emergency
    // action remained ahead of the command backlog.
    let original_epoch = engine.playback.position_epoch;
    engine.playback.time_pos = Some(3_630.0);
    engine.playback.paused = true;
    engine.bump_position_epoch(PositionEpochReason::Seek);
    let admitted_epoch = engine.playback.position_epoch;
    assert_eq!(admitted_epoch, original_epoch + 1);
    let old_rx = install_test_player(&mut engine);
    let mut replacement_rx = queue_test_player_start(&mut engine);

    let effects = engine
        .handle_player_event(PlayerEvent::CacheEmergency {
            file_generation: 0,
            position_secs: 3_600.0,
            paused: false,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        })
        .await;

    assert!(effects.is_empty());
    assert!(old_rx.is_closed());
    assert_eq!(engine.playback.time_pos, Some(3_630.0));
    assert!(engine.playback.paused);
    assert_eq!(engine.playback.position_epoch, admitted_epoch + 1);
    assert_eq!(engine.playback.position_epoch, original_epoch + 2);
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetVolume(50)
    ));
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetAudioFilter(_)
    ));
    let request = match recv_player_command(&mut replacement_rx).await {
        PlayerCmd::LoadWithResume(request) => request,
        _ => panic!("cache emergency must retain a correlated resume load"),
    };
    assert_eq!(request.position_secs, 3_630.0);
    assert!(request.paused);
    assert!(request.forces_ram_only());
    assert!(replacement_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn cache_emergency_freeze_bumps_epoch_even_when_recovery_delivery_exhausts() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = Some(3_599.0);
    engine.playback.time_pos_at = Some(Instant::now());
    let epoch = engine.playback.position_epoch;
    let _old_rx = install_test_player(&mut engine);

    let (tx, _replacement_rx) = tokio::sync::mpsc::channel(1);
    let handle = PlayerHandle::test_handle(tx);
    assert!(handle.send(PlayerCmd::Stop).is_ok(), "fill actor lane");
    // Leave exactly two semantic slots for player setup. The forced-RAM recovery load is then
    // rejected on both bounded attempts without ever becoming visible to the replacement actor.
    for _ in 0..crate::player::pending::PLAYER_PENDING_MAX - 2 {
        assert!(
            handle.send(PlayerCmd::Stop).is_ok(),
            "fill semantic backlog"
        );
    }
    engine.test_player_starts.push_back(PlayerRuntime {
        handle,
        _guard: None,
    });

    let effects = engine
        .handle_player_event(PlayerEvent::CacheEmergency {
            file_generation: 0,
            position_secs: 3_600.25,
            paused: false,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        })
        .await;
    let generation = match effects.as_slice() {
        [EngineEffect::TransportRecoveryRetry { generation, .. }] => *generation,
        _ => panic!("failed cache recovery must schedule one bounded retry"),
    };

    assert_eq!(engine.playback.time_pos, Some(3_599.0));
    assert_eq!(engine.playback.time_pos_at, None);
    assert_eq!(engine.playback.position_epoch, epoch + 1);

    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert_eq!(
        engine.playback.position_epoch,
        epoch + 1,
        "failed recovery attempts must not defer or duplicate the freeze epoch"
    );
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 2
    ));

    let exhausted = engine.transport_recovery.clone();
    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert_eq!(
        engine.transport_recovery, exhausted,
        "an exhausted recovery generation must be inert"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cache_emergency_retry_preserves_ram_only_position_and_pause() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = true;
    engine.playback.time_pos = Some(3_600.25);
    let epoch = engine.playback.position_epoch;
    let _old_rx = install_test_player(&mut engine);

    let (tx, mut replacement_rx) = tokio::sync::mpsc::channel(1);
    let handle = PlayerHandle::test_handle(tx);
    assert!(handle.send(PlayerCmd::Stop).is_ok(), "fill actor lane");
    for _ in 0..crate::player::pending::PLAYER_PENDING_MAX - 2 {
        assert!(
            handle.send(PlayerCmd::Stop).is_ok(),
            "leave only the two setup slots"
        );
    }
    engine.test_player_starts.push_back(PlayerRuntime {
        handle,
        _guard: None,
    });

    let effects = engine
        .handle_player_event(PlayerEvent::CacheEmergency {
            file_generation: 0,
            position_secs: 3_600.25,
            paused: true,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        })
        .await;
    let generation = match effects.as_slice() {
        [EngineEffect::TransportRecoveryRetry { generation, .. }] => *generation,
        _ => panic!("saturated RAM-only recovery must schedule one retry"),
    };
    assert_eq!(engine.playback.position_epoch, epoch + 1);
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 1
    ));

    loop {
        let command = recv_player_command(&mut replacement_rx).await;
        assert!(
            !matches!(command, PlayerCmd::LoadWithResume(_)),
            "failed attempt must not publish the recovery load"
        );
        if matches!(command, PlayerCmd::SetAudioFilter(_)) {
            break;
        }
    }

    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    let request = match recv_player_command(&mut replacement_rx).await {
        PlayerCmd::LoadWithResume(request) => request,
        _ => panic!("retry must retain the RAM-only resume request"),
    };
    assert_eq!(request.position_secs, 3_600.25);
    assert!(request.paused);
    assert!(request.forces_ram_only());
    assert_eq!(engine.playback.position_epoch, epoch + 1);
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);
}

#[tokio::test]
async fn replacement_cache_emergency_replays_new_item_without_old_position() {
    let mut engine = super::tests::engine_with_queue(&["old", "new"]);
    assert_eq!(
        engine.queue.next(false).map(|song| song.video_id.as_str()),
        Some("new")
    );
    engine.loaded_video_id = Some("new".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = None;
    let old_rx = install_test_player(&mut engine);
    assert!(
        engine
            .player
            .as_ref()
            .expect("old player")
            .handle
            .send(PlayerCmd::load(
                "https://example.invalid/new",
                crate::player::MediaSourceContext::OnDemand,
            ))
            .is_ok()
    );
    let mut replacement_rx = queue_test_player_start(&mut engine);

    let effects = engine
        .handle_player_event(PlayerEvent::CacheEmergency {
            file_generation: 0,
            position_secs: 3_600.25,
            paused: true,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        })
        .await;

    assert!(effects.is_empty());
    assert!(old_rx.is_closed());
    assert_eq!(engine.loaded_video_id.as_deref(), Some("new"));
    assert_eq!(engine.playback.time_pos, None);
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetVolume(50)
    ));
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::SetAudioFilter(_)
    ));
    let request = match recv_player_command(&mut replacement_rx).await {
        PlayerCmd::LoadWithResume(request) => request,
        _ => panic!("replacement cache emergency must use forced RAM recovery"),
    };
    assert_eq!(request.position_secs, 0.0);
    assert!(!request.paused);
    assert!(request.forces_ram_only());
    assert!(request.url.contains("new"), "must replay the new item");
}

#[tokio::test]
async fn stop_cache_emergency_retires_actor_without_replaying_queue_current() {
    let mut engine = super::tests::engine_with_queue(&["stopped"]);
    engine.loaded_video_id = None;
    engine.playback.time_pos = None;
    let old_rx = install_test_player(&mut engine);
    assert!(
        engine
            .player
            .as_ref()
            .expect("old player")
            .handle
            .send(PlayerCmd::Stop)
            .is_ok()
    );

    let effects = engine
        .handle_player_event(PlayerEvent::CacheEmergency {
            file_generation: 0,
            position_secs: 3_600.25,
            paused: true,
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        })
        .await;

    assert!(effects.is_empty());
    assert!(old_rx.is_closed());
    assert!(engine.player.is_none());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Armed);
    assert_eq!(engine.loaded_video_id, None);
}

#[tokio::test]
async fn shutdown_suppression_prevents_a_queued_transport_terminal_from_replacing_player() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    let old_rx = install_test_player(&mut engine);
    let _unused_replacement_rx = queue_test_player_start(&mut engine);

    // The external latch is handled before the already-queued TransportClosed owner event.
    engine.suppress_transport_recovery_for_shutdown();
    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("signal kill".to_owned()))
        .await;

    assert!(effects.is_empty());
    assert!(old_rx.is_closed());
    assert!(engine.player.is_none());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Shutdown);
    assert_eq!(
        engine.test_player_starts.len(),
        1,
        "the queued replacement must remain unused during shutdown"
    );
}

#[tokio::test]
async fn shutdown_suppression_cancels_an_already_scheduled_transport_retry() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    let old_rx = install_test_player(&mut engine);
    let _unused_replacement_rx = queue_test_player_start(&mut engine);

    let generation = engine
        .handle_transport_closed("scheduled before shutdown".to_owned())
        .expect("loaded transport close should schedule recovery");
    assert!(old_rx.is_closed());
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 0
    ));

    engine.suppress_transport_recovery_for_shutdown();
    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Shutdown);
    assert!(engine.player.is_none());
    assert_eq!(
        engine.test_player_starts.len(),
        1,
        "shutdown must not consume the queued replacement"
    );
}

#[tokio::test]
async fn transport_recovery_keeps_playing_state_without_pause_toggle() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    let _old_rx = install_test_player(&mut engine);
    let mut replacement_rx = queue_test_player_start(&mut engine);

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("EOF".to_owned()))
        .await;

    assert!(effects.is_empty());
    assert_setup_then_load(&mut replacement_rx, false).await;
    assert!(!engine.playback.paused);
}

#[tokio::test]
async fn closed_replacement_startup_is_retried_once_then_becomes_inert() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    let _old_rx = install_test_player(&mut engine);

    let (first_closed, first_rx) = test_player(1);
    drop(first_rx);
    engine.test_player_starts.push_back(first_closed);
    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("first".to_owned()))
        .await;
    let generation = match effects.as_slice() {
        [EngineEffect::TransportRecoveryRetry { generation, .. }] => *generation,
        _ => panic!("closed replacement setup must schedule exactly one retry"),
    };
    assert!(engine.player.is_none());
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 1
    ));

    let (second_closed, second_rx) = test_player(1);
    drop(second_rx);
    engine.test_player_starts.push_back(second_closed);
    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert!(engine.player.is_none());
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 2
    ));

    let exhausted = engine.transport_recovery.clone();
    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert_eq!(engine.transport_recovery, exhausted);
    assert!(engine.player.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn saturated_recovery_batch_is_retried_atomically_after_capacity_returns() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = true;
    let epoch = engine.playback.position_epoch;
    let _old_rx = install_test_player(&mut engine);

    let (tx, mut replacement_rx) = tokio::sync::mpsc::channel(1);
    let handle = PlayerHandle::test_handle(tx);
    assert!(handle.send(PlayerCmd::Stop).is_ok(), "fill actor lane");
    // Leave room for the two setup commands, but not for the atomic Load+pause replay.
    for _ in 0..crate::player::pending::PLAYER_PENDING_MAX - 3 {
        assert!(
            handle.send(PlayerCmd::Stop).is_ok(),
            "fill semantic backlog"
        );
    }
    engine.test_player_starts.push_back(PlayerRuntime {
        handle,
        _guard: None,
    });

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("broken pipe".to_owned()))
        .await;
    let (generation, retry_after) = match effects.as_slice() {
        [
            EngineEffect::TransportRecoveryRetry {
                generation,
                retry_after,
            },
        ] => (*generation, *retry_after),
        _ => panic!("saturated recovery must schedule exactly one retry"),
    };
    assert_eq!(retry_after, Duration::from_millis(75));
    assert_eq!(engine.loaded_video_id, None);
    assert_eq!(engine.playback.position_epoch, epoch + 1);
    assert_eq!(
        engine.transport_recovery,
        TransportRecoveryState::Recovering(TransportRecovery::reload_for_test(
            "seed".to_owned(),
            true,
            generation,
            1,
        ))
    );

    // Drain the admitted setup/backlog. No Load may appear: the failed two-command batch
    // must have published neither its prefix nor its pause suffix.
    loop {
        let command = recv_player_command(&mut replacement_rx).await;
        assert!(!matches!(command, PlayerCmd::Load(_)));
        if matches!(command, PlayerCmd::SetAudioFilter(_)) {
            break;
        }
    }

    let retry_effects = engine.attempt_transport_recovery(generation).await;
    assert!(retry_effects.is_empty());
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::Load(_)
    ));
    assert!(matches!(
        recv_player_command(&mut replacement_rx).await,
        PlayerCmd::CyclePause
    ));
    assert_eq!(engine.loaded_video_id.as_deref(), Some("seed"));
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);
    assert_eq!(engine.playback.position_epoch, epoch + 1);
}

#[tokio::test(flavor = "current_thread")]
async fn replacement_close_while_retry_is_pending_disarms_the_recovery_generation() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = true;
    let _old_rx = install_test_player(&mut engine);

    let (tx, replacement_rx) = tokio::sync::mpsc::channel(1);
    let handle = PlayerHandle::test_handle(tx);
    assert!(handle.send(PlayerCmd::Stop).is_ok(), "fill actor lane");
    for _ in 0..crate::player::pending::PLAYER_PENDING_MAX - 3 {
        assert!(
            handle.send(PlayerCmd::Stop).is_ok(),
            "leave setup capacity but saturate the recovery batch"
        );
    }
    engine.test_player_starts.push_back(PlayerRuntime {
        handle,
        _guard: None,
    });

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("first".to_owned()))
        .await;
    let generation = match effects.as_slice() {
        [EngineEffect::TransportRecoveryRetry { generation, .. }] => *generation,
        _ => panic!("failed first restore must schedule one retry"),
    };
    assert!(engine.player.is_some());
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 1
    ));
    drop(replacement_rx);

    let _unused_second_replacement = queue_test_player_start(&mut engine);
    assert!(
        engine
            .handle_player_event(PlayerEvent::TransportClosed(
                "replacement died before retry".to_owned(),
            ))
            .await
            .is_empty()
    );
    assert!(engine.player.is_none());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);
    assert_eq!(
        engine.test_player_starts.len(),
        1,
        "a replacement terminal must not consume another player start"
    );

    assert!(
        engine
            .attempt_transport_recovery(generation)
            .await
            .is_empty()
    );
    assert!(engine.player.is_none());
    assert_eq!(engine.test_player_starts.len(), 1);
}

#[tokio::test]
async fn replacement_that_closes_before_progress_cannot_enter_a_restart_loop() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    let _old_rx = install_test_player(&mut engine);
    let _replacement_rx = queue_test_player_start(&mut engine);

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("first".to_owned()))
        .await;
    assert!(effects.is_empty());
    let first_generation = engine.transport_recovery_generation;
    assert!(engine.player.is_some());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("replacement died".to_owned()))
        .await;
    assert!(effects.is_empty());
    assert!(engine.player.is_none());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Disarmed);
    assert!(engine.test_player_starts.is_empty());

    // A stale scheduled retry is inert too; it cannot recreate the replacement.
    assert!(
        engine
            .attempt_transport_recovery(first_generation)
            .await
            .is_empty()
    );
    assert!(engine.player.is_none());
}

#[tokio::test]
async fn stale_retry_cannot_mutate_a_newer_transport_recovery_generation() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    let old_rx = install_test_player(&mut engine);
    let _unused_replacement_rx = queue_test_player_start(&mut engine);
    let newer_generation = engine
        .handle_transport_closed("newer generation".to_owned())
        .expect("loaded transport close should schedule recovery");
    assert!(old_rx.is_closed());
    let newer_recovery = engine.transport_recovery.clone();

    assert!(
        engine
            .attempt_transport_recovery(newer_generation.wrapping_sub(1))
            .await
            .is_empty()
    );
    assert_eq!(engine.transport_recovery, newer_recovery);
    assert!(engine.player.is_none());
    assert_eq!(
        engine.test_player_starts.len(),
        1,
        "a stale retry must not start a player for the newer generation"
    );
}
