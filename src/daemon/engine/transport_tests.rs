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
    assert_eq!(engine.playback.position_epoch, epoch + 2);
    assert_eq!(engine.transport_recovery, None);
    assert!(!engine.transport_auto_recovery_armed);
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
    assert!(engine.transport_recovery.is_none());
    assert!(!engine.transport_auto_recovery_armed);
    assert_eq!(
        engine.test_player_starts.len(),
        1,
        "the queued replacement must remain unused during shutdown"
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
        Some(TransportRecovery {
            video_id: "seed".to_owned(),
            paused: true,
            generation,
            attempts: 1,
        })
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
    assert_eq!(engine.transport_recovery, None);
    assert_eq!(engine.playback.position_epoch, epoch + 2);
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
    assert!(!engine.transport_auto_recovery_armed);

    let effects = engine
        .handle_player_event(PlayerEvent::TransportClosed("replacement died".to_owned()))
        .await;
    assert!(effects.is_empty());
    assert!(engine.player.is_none());
    assert_eq!(engine.transport_recovery, None);
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

#[tokio::test(flavor = "current_thread")]
async fn paused_transport_recovery_rejects_the_whole_batch_before_load_is_visible() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = true;
    let epoch = engine.playback.position_epoch;

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let handle = PlayerHandle::test_handle(tx);
    assert!(handle.send(PlayerCmd::Stop).is_ok(), "fill actor lane");
    // Keep the drainer blocked on the full actor lane and leave room for only one more
    // semantic command. Recovery needs two, so `send_batch` must publish neither.
    for _ in 0..crate::player::pending::PLAYER_PENDING_MAX - 1 {
        assert!(
            handle.send(PlayerCmd::Stop).is_ok(),
            "fill semantic backlog"
        );
    }
    engine.player = Some(PlayerRuntime {
        handle,
        _guard: None,
    });
    engine.transport_recovery = Some(TransportRecovery {
        video_id: "seed".to_owned(),
        paused: true,
        generation: 1,
        attempts: 1,
    });
    engine.loaded_video_id = None;

    assert!(engine.load_current_loaded().is_err());
    assert_eq!(engine.loaded_video_id, None);
    assert_eq!(engine.playback.position_epoch, epoch);
    assert_eq!(
        engine.transport_recovery,
        Some(TransportRecovery {
            video_id: "seed".to_owned(),
            paused: true,
            generation: 1,
            attempts: 1,
        })
    );
    assert!(matches!(rx.try_recv(), Ok(PlayerCmd::Stop)));
    assert!(
        rx.try_recv().is_err(),
        "recovery Load prefix must not be visible"
    );
}
