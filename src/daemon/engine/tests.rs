use super::*;
use std::time::Duration;

mod perf;

fn song(id: &str) -> Song {
    Song::remote(id, format!("title-{id}"), "artist".to_owned(), "3:00")
}

pub(super) fn personal_state_paths() -> crate::personal_state::PersonalStatePaths {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "yututui-daemon-personal-state-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("create daemon personal-state test root");
    crate::personal_state::PersonalStatePaths::for_data_root(root)
}

pub(in crate::daemon) fn radio_station(id: &str) -> Song {
    let mut song = Song::remote(id, format!("station-{id}"), "", "");
    song.playable = Some(crate::api::PlayableRef::RadioStream {
        url: format!("https://radio.example/{id}.mp3"),
    });
    song
}

/// Test-only manual queue replacement with player admission, mirroring what a CLI
/// `play` command does end-to-end (no provenance bookkeeping anymore).
pub(in crate::daemon) async fn replace_queue_with_songs(
    engine: &mut DaemonEngine,
    songs: Vec<Song>,
) -> RemoteResponse {
    if songs.is_empty() {
        return RemoteResponse::err("queue_empty");
    }
    let previous = engine.queue.snapshot();
    engine.queue.set(songs, 0);
    match engine.load_current_or_restore_queue(previous).await {
        Ok(()) => RemoteResponse::status(engine.status()),
        Err(error) => RemoteResponse::err(error.reason()),
    }
}

pub(in crate::daemon) fn engine_with_queue(ids: &[&str]) -> DaemonEngine {
    let mut queue = Queue::default();
    queue.set(ids.iter().map(|id| song(id)).collect(), 0);
    let personal_state = crate::personal_state::legacy_state(
        &Library::default(),
        &crate::playlists::Playlists::default(),
        &Signals::default(),
        &StationStore::default(),
    )
    .expect("default personal state");
    DaemonEngine {
        maintainer: crate::util::background_task::BackgroundTask::disabled("yt-dlp maintainer"),
        player: None,
        open_subsonic: Default::default(),
        open_subsonic_rating_identity: None,
        open_subsonic_pending_rating: None,
        open_subsonic_playlist_identity: None,
        open_subsonic_pending_playlist: None,
        open_subsonic_pending_scrobbles: VecDeque::new(),
        player_emit: Arc::new(|_| {}),
        queue,
        playback: DaemonPlayback {
            paused: true,
            volume: 50,
            time_pos: None,
            time_pos_at: None,
            position_epoch: 0,
            duration: None,
            speed: 1.0,
        },
        config: Config::default(),
        personal_state_revision_guard: crate::sync::OwnerRevisionGuard::new(
            personal_state.revision,
        ),
        personal_state,
        personal_state_device_id: None,
        personal_sync_in_progress: false,
        personal_state_paths: personal_state_paths(),
        library: Library::default(),
        playlists: crate::playlists::Playlists::default(),
        playlists_rev: 0,
        library_invalidations: 0,
        signals: Signals::default(),
        station: StationStore::default(),
        loaded_video_id: None,
        transport_recovery: TransportRecoveryState::Armed,
        transport_recovery_generation: 0,
        source_recovery: crate::player::recovery::RecoveryPlanner::default(),
        source_logical_generation: 0,
        source_file_generation: 0,
        test_player_starts: VecDeque::new(),
        streaming: false,
        streaming_pending: false,
        streaming_request_seq: 0,
        pending_streaming_request: None,
        last_extend: None,
        consecutive_streaming_failures: 0,
        last_error: None,
        remote_persistence_write_failed: false,
        remote_persistence_error: None,
        remote_persistence_command_active: false,
        remote_persistence_read_only: false,
        persistence_disabled_for_test: false,
        consecutive_play_errors: 0,
        heal_pending: None,
        heal_attempted: HashSet::new(),
        heal_last_check: None,
        last_mode: LastMode::Normal,
        inactive_normal_queue: None,
        inactive_radio_queue: None,
        inactive_local_queue: None,
        session_events: VecDeque::new(),
        media_art: None,
    }
}

#[tokio::test]
async fn dropping_engine_aborts_maintainer_instead_of_detaching() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let mut engine = engine_with_queue(&[]);
    engine.maintainer =
        crate::util::background_task::BackgroundTask::spawn("test daemon maintainer", async move {
            struct MarkDrop(Option<tokio::sync::oneshot::Sender<()>>);
            impl Drop for MarkDrop {
                fn drop(&mut self) {
                    if let Some(tx) = self.0.take() {
                        let _ = tx.send(());
                    }
                }
            }
            let _mark = MarkDrop(Some(dropped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
    started_rx.await.unwrap();

    drop(engine);

    tokio::time::timeout(Duration::from_millis(100), dropped_rx)
        .await
        .expect("engine drop must cancel maintainer")
        .unwrap();
}

pub(super) fn install_accepting_player(
    engine: &mut DaemonEngine,
) -> tokio::sync::mpsc::Receiver<PlayerCmd> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    engine.player = Some(PlayerRuntime {
        handle: PlayerHandle::test_handle(tx),
        _guard: None,
    });
    rx
}

#[test]
fn status_distinguishes_unknown_duration_from_genuine_live_stream() {
    let engine = engine_with_queue(&["loading"]);
    let unknown = engine.status();
    assert_eq!(unknown.duration_ms, None);
    assert!(!unknown.is_live);
    assert_eq!(unknown.queue_rev, Some(engine.queue.rev()));
    assert_eq!(unknown.track_id.as_deref(), Some("loading"));
    assert_eq!(unknown.position_epoch, engine.playback.position_epoch);

    let mut live_engine = engine_with_queue(&[]);
    live_engine.queue.set(vec![radio_station("station")], 0);
    let live = live_engine.status();
    assert_eq!(live.duration_ms, None);
    assert!(live.is_live);
    assert_eq!(live.queue_rev, Some(live_engine.queue.rev()));
    assert_eq!(live.track_id.as_deref(), Some("station"));
    assert_eq!(live.position_epoch, live_engine.playback.position_epoch);
}

#[tokio::test]
async fn stale_revision_checked_queue_commands_preserve_the_existing_error() {
    let mut engine = engine_with_queue(&["a", "b"]);
    for command in [
        RemoteCommand::QueuePlayIfRevision {
            position: 1,
            expected_rev: u64::MAX,
        },
        RemoteCommand::QueueRemoveIfRevision {
            position: 0,
            expected_rev: u64::MAX,
        },
    ] {
        engine.last_error = Some("existing playback failure".to_string());
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        assert_eq!(response.reason.as_deref(), Some("stale_rev"));
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_eq!(
            engine.last_error.as_deref(),
            Some("existing playback failure")
        );
    }
}

#[test]
fn status_artwork_only_matches_current_track() {
    let mut engine = engine_with_queue(&["seed"]);
    // Art for a *different* track is not surfaced (mirrors the media snapshot gate).
    engine.set_media_art(crate::media::artwork::MediaArtworkReady {
        key: "other".to_owned(),
        path: std::path::PathBuf::from("/tmp/other.jpg"),
    });
    assert!(engine.status().artwork.is_none());

    engine.set_media_art(crate::media::artwork::MediaArtworkReady {
        key: "seed".to_owned(),
        path: std::path::PathBuf::from("/tmp/seed.jpg"),
    });
    let art = engine.status().artwork.expect("artwork");
    assert_eq!(art.key, "seed");
    assert_eq!(art.path.as_deref(), Some("/tmp/seed.jpg"));
}

#[test]
fn one_shot_status_exposes_only_privacy_safe_daemon_cache_runtime_diagnostics() {
    let mut engine = engine_with_queue(&["seed"]);
    let _player_rx = install_accepting_player(&mut engine);
    engine
        .player
        .as_ref()
        .expect("test player")
        .handle
        .test_set_long_form_seek_runtime(
            crate::player::long_form_seek::CacheStatus {
                requested: crate::config::LongFormSeekOptimization::Auto,
                effective: crate::player::long_form_seek::CacheEffectiveState::DiskActive,
                reason: crate::player::long_form_seek::CacheReason::AutoUncachedSeek,
                file_generation: Some(9),
                policy_revision: 2,
                file_cache_bytes: 4_096,
                peak_file_cache_bytes: 8_192,
            },
            Some(crate::player::long_form_seek::CacheReason::ProbeFailed),
            Some(275),
        );

    let runtime = engine
        .status()
        .settings
        .long_form_seek
        .expect("daemon runtime diagnostics");
    assert_eq!(
        runtime.effective,
        crate::remote::proto::LongFormSeekEffective::DiskActive
    );
    assert_eq!(
        runtime.reason,
        crate::remote::proto::LongFormSeekReason::AutoUncachedSeek
    );
    assert_eq!(
        runtime.last_failure,
        Some(crate::remote::proto::LongFormSeekReason::ProbeFailed)
    );
    assert_eq!(runtime.last_cleanup_ms, Some(275));
}

#[tokio::test]
async fn player_events_normalize_transport_state_without_player_runtime() {
    let mut engine = engine_with_queue(&["seed"]);
    let epoch = engine.playback.position_epoch;

    assert!(
        engine
            .handle_player_event(PlayerEvent::TimePos(f64::NAN))
            .await
            .is_empty()
    );
    assert_eq!(engine.playback.time_pos, Some(0.0));
    assert_eq!(
        engine.playback.position_epoch, epoch,
        "ordinary progress must not masquerade as a seek discontinuity"
    );
    engine
        .handle_player_event(PlayerEvent::Duration(Some(f64::INFINITY)))
        .await;
    assert_eq!(engine.playback.duration, Some(0.0));
    engine.handle_player_event(PlayerEvent::Paused(false)).await;
    assert!(!engine.playback.paused);
    assert!(engine.playback.time_pos_at.is_some());
    engine
        .handle_player_event(PlayerEvent::Volume(f64::INFINITY))
        .await;
    assert_eq!(engine.playback.volume, 50);
    engine.handle_player_event(PlayerEvent::Volume(12.4)).await;
    assert_eq!(engine.playback.volume, 12);
    engine
        .handle_player_event(PlayerEvent::Metadata(serde_json::Value::Null))
        .await;
    engine
        .handle_player_event(PlayerEvent::CacheTime(None))
        .await;
    assert_eq!(engine.playback.position_epoch, epoch);
    engine
        .handle_player_event(PlayerEvent::AudioCodec(Some("aac".to_owned())))
        .await;
    engine
        .handle_player_event(PlayerEvent::FileFormat(Some("mp4".to_owned())))
        .await;
}

#[tokio::test]
async fn terminal_eof_stops_the_player_without_discarding_selected_metadata() {
    let mut engine = engine_with_queue(&["seed"]);
    let mut player_rx = install_accepting_player(&mut engine);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = Some(180.0);
    engine.playback.duration = Some(180.0);

    let effects = engine.handle_player_event(PlayerEvent::Eof).await;

    assert!(effects.is_empty());
    assert!(matches!(player_rx.try_recv(), Ok(PlayerCmd::Stop)));
    assert!(engine.player.is_none());
    assert!(engine.loaded_video_id.is_none());
    assert_eq!(
        engine.queue.current().map(|song| song.video_id.as_str()),
        Some("seed")
    );
    assert_eq!(engine.status().title.as_deref(), Some("title-seed"));
    assert!(engine.playback.paused);
    assert_eq!(engine.playback.time_pos, None);
}

#[tokio::test]
async fn media_commands_and_snapshot_mutate_only_supported_headless_state() {
    let mut engine = engine_with_queue(&["seed", "next"]);
    let _player_rx = install_accepting_player(&mut engine);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = Some(10.0);
    engine.playback.time_pos_at = Some(Instant::now());
    engine.playback.duration = Some(100.0);
    engine.set_media_art(crate::media::artwork::MediaArtworkReady {
        key: "seed".to_owned(),
        path: std::path::PathBuf::from("/tmp/seed.jpg"),
    });
    engine.library.toggle_favorite(&song("seed"));

    let snapshot = engine.media_snapshot();
    assert_eq!(snapshot.status, crate::media::MediaPlaybackStatus::Playing);
    assert!(snapshot.caps.can_next);
    assert!(snapshot.caps.can_seek);
    let track = snapshot.track.unwrap();
    assert_eq!(track.key, "seed");
    assert_eq!(track.duration, Some(100.0));
    assert!(track.liked);
    assert_eq!(
        track.art_file.as_deref(),
        Some(std::path::Path::new("/tmp/seed.jpg"))
    );

    let (_, effects) = engine
        .handle_media(crate::media::MediaCommand::SeekBy(5.0))
        .await;
    assert!(effects.is_empty());
    assert_eq!(engine.playback.time_pos, Some(15.0));
    let epoch_after_seek = engine.playback.position_epoch;

    let (_, effects) = engine
        .handle_media(crate::media::MediaCommand::SeekTo(150.0))
        .await;
    assert!(effects.is_empty());
    assert_eq!(engine.playback.position_epoch, epoch_after_seek);
    assert_eq!(engine.playback.time_pos, Some(15.0));

    let (_, effects) = engine
        .handle_media(crate::media::MediaCommand::SetVolume(0.37))
        .await;
    assert!(effects.is_empty());
    assert_eq!(engine.playback.volume, 37);

    let (_, effects) = engine
        .handle_media(crate::media::MediaCommand::SetRate(1.75))
        .await;
    assert!(effects.is_empty());
    assert_eq!(engine.playback.speed, 1.8);

    let (_, effects) = engine
        .handle_media(crate::media::MediaCommand::SetShuffle(true))
        .await;
    assert!(effects.is_empty());
    assert!(engine.queue.shuffle);

    let (_, effects) = engine
        .handle_media(crate::media::MediaCommand::SetRepeat(
            crate::queue::Repeat::All,
        ))
        .await;
    assert!(effects.is_empty());
    assert_eq!(engine.queue.repeat, crate::queue::Repeat::All);

    let (shutdown, effects) = engine.handle_media(crate::media::MediaCommand::Stop).await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(engine.loaded_video_id.is_none());
    assert_eq!(
        engine.media_snapshot().status,
        crate::media::MediaPlaybackStatus::Paused
    );
}

#[test]
fn status_core_view_and_media_snapshot_share_current_track_projection() {
    let mut engine = engine_with_queue(&["seed", "next"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.volume = 73;
    engine.playback.time_pos = Some(4.0);
    engine.playback.time_pos_at = Some(Instant::now() - Duration::from_millis(5));
    engine.playback.duration = Some(123.0);
    engine.playback.speed = 1.5;
    for _ in 0..7 {
        engine.bump_position_epoch(PositionEpochReason::Seek);
    }
    engine.streaming = true;
    engine.queue.set_shuffle(true);
    engine.queue.repeat = crate::queue::Repeat::All;
    engine.set_media_art(crate::media::artwork::MediaArtworkReady {
        key: "seed".to_owned(),
        path: std::path::PathBuf::from("/tmp/daemon-seed.jpg"),
    });
    engine.library.toggle_favorite(&song("seed"));
    engine.signals.toggle_dislike(
        "next",
        &signals::normalize_artist("artist"),
        signals::unix_now(),
    );

    let status = engine.status();
    assert_eq!(status.title.as_deref(), Some("title-seed"));
    assert_eq!(status.artist.as_deref(), Some("artist"));
    assert!(!status.paused);
    assert_eq!(status.volume, 73);
    assert_eq!(status.position, 1);
    assert_eq!(status.total, 2);
    assert!(status.streaming);
    assert!(status.shuffle);
    assert_eq!(status.repeat, crate::queue::Repeat::All);
    assert_eq!(status.duration_ms, Some(123_000));
    assert!(status.elapsed_ms.unwrap() >= 4_000);
    assert_eq!(
        status.artwork.as_ref().map(|art| art.key.as_str()),
        Some("seed")
    );
    assert_eq!(status.queue.len(), 2);
    assert!(status.queue[0].current);

    let core = engine.core_view();
    assert_eq!(core.volume, 73);
    assert_eq!(core.speed_tenths, 15);
    assert_eq!(core.duration_ms, Some(123_000));
    assert_eq!(core.position_epoch, 7);
    assert!(core.streaming);
    assert_eq!(core.owner_mode, InstanceMode::Daemon);
    assert_eq!(core.artwork.as_ref().map(|art| art.key), Some("seed"));

    let media = engine.media_snapshot();
    assert_eq!(media.status, crate::media::MediaPlaybackStatus::Playing);
    assert!(media.shuffle);
    assert_eq!(media.repeat, crate::queue::Repeat::All);
    assert!((media.volume - 0.73).abs() < f64::EPSILON);
    assert!(media.caps.can_next);
    assert!(media.caps.can_previous);
    assert!(media.caps.can_seek);
    let track = media.track.expect("current media track");
    assert_eq!(track.key, "seed");
    assert_eq!(track.duration, Some(123.0));
    assert!(track.liked);
    assert!(!track.disliked);
    assert_eq!(
        track.url.as_deref(),
        Some("https://music.youtube.com/watch?v=seed")
    );
    assert!(track.art_remote_url.is_some());
    assert!(matches!(
        track.art_query,
        Some(crate::media::artwork::ArtQuery::Youtube { ref id }) if id == "seed"
    ));
}

#[test]
fn media_snapshot_for_radio_stream_disables_track_specific_music_controls() {
    let mut engine = engine_with_queue(&[]);
    engine.queue.set(vec![radio_station("radio1")], 0);
    engine.loaded_video_id = Some("radio1".to_owned());
    engine.playback.paused = false;
    engine.playback.duration = Some(999.0);
    engine.set_media_art(crate::media::artwork::MediaArtworkReady {
        key: "radio1".to_owned(),
        path: std::path::PathBuf::from("/tmp/radio.jpg"),
    });

    let snapshot = engine.media_snapshot();

    assert_eq!(snapshot.status, crate::media::MediaPlaybackStatus::Playing);
    assert!(!snapshot.caps.can_next);
    assert!(snapshot.caps.can_previous);
    assert!(!snapshot.caps.can_seek);
    let track = snapshot.track.expect("radio track");
    assert_eq!(track.key, "radio1");
    assert!(track.is_live);
    assert_eq!(track.duration, None);
    assert_eq!(track.album, None);
    assert_eq!(
        track.url.as_deref(),
        Some("https://music.youtube.com/watch?v=radio1")
    );
    assert_eq!(track.art_remote_url, None);
    assert!(track.art_query.is_none());
    assert_eq!(
        track.art_file.as_deref(),
        Some(std::path::Path::new("/tmp/radio.jpg"))
    );
}

#[tokio::test]
async fn remote_commands_cover_no_load_branches() {
    let mut engine = engine_with_queue(&[]);
    engine.silence_remote_persistence_for_test();

    for command in [
        RemoteCommand::Next,
        RemoteCommand::Prev,
        RemoteCommand::TogglePause,
        RemoteCommand::SeekBack,
        RemoteCommand::SeekForward,
        RemoteCommand::QueuePlay { position: 1 },
        RemoteCommand::QueueRemove { position: 1 },
    ] {
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        assert!(!response.ok);
        assert!(!shutdown);
        assert!(effects.is_empty());
    }

    let _player_rx = install_accepting_player(&mut engine);
    engine.loaded_video_id = Some("queued-before-quit".to_owned());
    let generation = engine
        .handle_transport_closed("queued before quit".to_owned())
        .expect("loaded transport close should schedule recovery");
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 0
    ));
    let (response, shutdown, effects) = engine.handle_remote(RemoteCommand::Quit).await;
    assert!(response.ok);
    assert!(shutdown);
    assert!(effects.is_empty());
    assert!(engine.loaded_video_id.is_none());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Shutdown);
}

#[tokio::test]
async fn remote_repeat_and_streaming_guards_preserve_music_mode_invariant() {
    let mut engine = engine_with_queue(&["seed"]);
    engine.streaming = true;
    engine.queue.repeat = crate::queue::Repeat::Off;

    let (response, _, effects) = engine.handle_remote(RemoteCommand::CycleRepeat).await;

    assert!(!response.ok);
    assert_eq!(
        response.reason.as_deref(),
        Some("incompatible_playback_modes")
    );
    assert!(effects.is_empty());
    assert_eq!(engine.queue.repeat, crate::queue::Repeat::Off);

    engine.streaming = false;
    engine.queue.repeat = crate::queue::Repeat::All;
    engine.config.repeat = crate::queue::Repeat::All;
    engine.config.autoplay_streaming = Some(false);
    let (response, _, effects) = engine
        .handle_remote(RemoteCommand::Streaming {
            state: ToggleState::On,
        })
        .await;

    assert!(!response.ok);
    assert_eq!(
        response.reason.as_deref(),
        Some("incompatible_playback_modes")
    );
    assert!(effects.is_empty());
    assert!(!engine.streaming);
    assert_eq!(engine.config.autoplay_streaming, Some(false));
}

#[tokio::test]
async fn media_commands_ignore_invalid_or_disabled_operations() {
    let mut engine = engine_with_queue(&["seed"]);
    let _player_rx = install_accepting_player(&mut engine);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = Some(5.0);
    engine.playback.duration = Some(60.0);

    for cmd in [
        crate::media::MediaCommand::SeekBy(f64::NAN),
        crate::media::MediaCommand::SeekTo(f64::NAN),
        crate::media::MediaCommand::SeekTo(-1.0),
        crate::media::MediaCommand::OpenUri("https://example.com/not-youtube".to_owned()),
    ] {
        let (shutdown, effects) = engine.handle_media(cmd).await;
        assert!(!shutdown);
        assert!(effects.is_empty());
    }
    assert_eq!(engine.playback.time_pos, Some(5.0));
    let epoch = engine.playback.position_epoch;

    let (shutdown, effects) = engine
        .handle_media(crate::media::MediaCommand::SetRate(0.0))
        .await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(engine.playback.paused);
    assert_eq!(engine.playback.position_epoch, epoch);

    let generation = engine
        .handle_transport_closed("queued before media quit".to_owned())
        .expect("loaded transport close should schedule recovery");
    assert!(matches!(
        &engine.transport_recovery,
        TransportRecoveryState::Recovering(recovery)
            if recovery.generation == generation && recovery.attempts == 0
    ));
    let (shutdown, effects) = engine.handle_media(crate::media::MediaCommand::Quit).await;
    assert!(shutdown);
    assert!(effects.is_empty());
    assert!(engine.loaded_video_id.is_none());
    assert_eq!(engine.transport_recovery, TransportRecoveryState::Shutdown);
}

#[test]
fn session_event_bias_caps_and_classifies_recent_skips() {
    let mut engine = engine_with_queue(&["seed"]);

    for idx in 0..(SESSION_EVENTS_CAP + 5) {
        let outcome = match idx % 3 {
            0 => DaemonOutcome::FullPlay,
            1 => DaemonOutcome::Skip,
            _ => DaemonOutcome::QuickSkip,
        };
        engine.record_session_event(
            &format!("artist-{idx}"),
            outcome,
            if matches!(outcome, DaemonOutcome::FullPlay) {
                0.9
            } else {
                0.1
            },
        );
    }

    assert_eq!(engine.session_events.len(), SESSION_EVENTS_CAP);
    assert_eq!(
        engine
            .session_events
            .front()
            .map(|event| event.artist_key.as_str()),
        Some("artist-5")
    );
    assert_eq!(engine.streaming_skip_streak(), 0);

    engine.record_session_event("skip-a", DaemonOutcome::QuickSkip, 0.0);
    engine.record_session_event("skip-b", DaemonOutcome::Skip, 0.2);
    assert_eq!(engine.streaming_skip_streak(), 2);

    let bias = engine.session_artist_bias();
    assert!(bias.get("skip-a").copied().unwrap_or_default() < 0.0);
    assert!(bias.get("skip-b").copied().unwrap_or_default() < 0.0);

    engine.playback.time_pos = Some(15.0);
    engine.playback.duration = Some(60.0);
    assert!((engine.playback_completion() - 0.25).abs() < f32::EPSILON);
    engine.playback.duration = None;
    assert!((engine.playback_completion() - 0.5).abs() < f32::EPSILON);
}

#[test]
fn maybe_autoplay_extend_emits_real_streaming_request() {
    let mut engine = engine_with_queue(&["seed"]);
    engine.streaming = true;

    let effects = engine.maybe_autoplay_extend();

    assert_eq!(effects.len(), 1);
    match &effects[0] {
        EngineEffect::StreamingFallback {
            seed_video_id,
            limit,
            ..
        } => {
            assert_eq!(seed_video_id, "seed");
            assert_eq!(*limit, STREAMING_POOL_COUNT);
        }
        _ => panic!("expected streaming fallback"),
    }
    assert!(engine.streaming_pending);
}

#[tokio::test]
async fn streaming_on_forces_request_even_when_queue_is_not_low() {
    let mut engine = engine_with_queue(&["seed", "a", "b", "c", "d", "e"]);
    engine.last_extend = Some(Instant::now());
    assert!(engine.queue.remaining() > AUTOPLAY_THRESHOLD);

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::Streaming {
            state: ToggleState::On,
        })
        .await;

    assert!(response.ok);
    assert!(!shutdown);
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        &effects[0],
        EngineEffect::StreamingFallback { seed_video_id, .. } if seed_video_id == "seed"
    ));
}

#[tokio::test]
async fn remote_semantic_caps_reject_abuse() {
    // Over-long search query (via Play) is rejected before the search fan-out.
    let mut engine = engine_with_queue(&["seed"]);
    let (resp, _, _) = engine
        .handle_remote(RemoteCommand::Play {
            query: "x".repeat(REMOTE_MAX_QUERY_BYTES + 1),
        })
        .await;
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("query_too_long"));
}

#[tokio::test]
async fn remote_seek_to_is_clamped_when_duration_unknown() {
    let mut engine = engine_with_queue(&["seed"]);
    let _player_rx = install_accepting_player(&mut engine);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.duration = None; // live / not-yet-probed
    let (resp, _, _) = engine
        .handle_remote(RemoteCommand::SeekTo { ms: u64::MAX })
        .await;
    assert!(resp.ok);
    // The absurd target is capped at the day ceiling, not passed through to mpv.
    assert_eq!(
        engine.playback.time_pos,
        Some(crate::playback_policy::MAX_SEEK_SECONDS)
    );
}

#[tokio::test]
async fn streaming_on_forces_request_with_dj_gem_setting_off_too() {
    let mut engine = engine_with_queue(&["seed", "a", "b", "c", "d", "e"]);
    engine.config.ai_enabled = Some(false);
    assert!(engine.queue.remaining() > AUTOPLAY_THRESHOLD);

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::Streaming {
            state: ToggleState::On,
        })
        .await;

    assert!(response.ok);
    assert!(!shutdown);
    assert!(matches!(
        effects.as_slice(),
        [EngineEffect::StreamingFallback { seed_video_id, .. }] if seed_video_id == "seed"
    ));
}

#[tokio::test]
async fn media_shuffle_and_repeat_are_ignored_for_live_radio() {
    let mut engine = engine_with_queue(&[]);
    engine.queue.set(vec![radio_station("radio1")], 0);
    engine.loaded_video_id = Some("radio1".to_owned());

    let (shutdown, effects) = engine
        .handle_media(crate::media::MediaCommand::SetShuffle(true))
        .await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(!engine.queue.shuffle);
    assert_eq!(engine.config.shuffle, None);

    let (shutdown, effects) = engine
        .handle_media(crate::media::MediaCommand::SetRepeat(
            crate::queue::Repeat::All,
        ))
        .await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(engine.queue.repeat, crate::queue::Repeat::Off);
    assert_eq!(engine.config.repeat, crate::queue::Repeat::Off);
}

#[test]
fn plan_local_streaming_filters_existing_queue_ids() {
    let mut engine = engine_with_queue(&["seed"]);
    let candidates = (0..12)
        .map(|i| {
            (
                Song::remote(
                    format!("c{i}"),
                    format!("candidate {i}"),
                    format!("artist {i}"),
                    "3:00",
                ),
                CandidateSource::YtdlpStreaming,
            )
        })
        .collect();

    let picks = engine.plan_local_streaming("seed", candidates);

    assert!(!picks.is_empty());
    assert!(picks.iter().all(|song| song.video_id != "seed"));
}

#[test]
fn session_snapshot_preserves_active_queue() {
    let mut engine = engine_with_queue(&["a", "b"]);
    engine.queue.next(false);

    let cache = engine.session_cache_snapshot();
    let snapshot = cache.normal_queue.expect("normal queue saved");

    assert_eq!(snapshot.cursor, 1);
    assert_eq!(snapshot.songs.len(), 2);
}

// yt-dlp self-heal parity with the TUI reducer (src/app/tests.rs). Single-track
// queues on the skip paths keep these hermetic: with no next track the engine
// stops instead of calling `load_current` (which would spawn a real mpv).

const EXTRACTION_ERR: &str = "mpv could not play this track (unrecognized file format)";

#[tokio::test]
async fn extraction_error_triggers_self_heal_effect() {
    let mut engine = engine_with_queue(&["a", "b"]);
    let effects = engine
        .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
        .await;
    assert!(
        matches!(&effects[..], [EngineEffect::YtdlpSelfHeal { video_id, .. }] if video_id == "a"),
        "runs an update check instead of skipping"
    );
    assert_eq!(
        engine.queue.current().map(|s| s.video_id.as_str()),
        Some("a"),
        "cursor stays on the failed track while the heal runs"
    );
    assert_eq!(engine.consecutive_play_errors, 0, "heal is not a strike");
}

#[tokio::test]
async fn heal_without_update_falls_back_to_stop_on_single_track() {
    let mut engine = engine_with_queue(&["a"]);
    engine
        .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
        .await;
    let effects = engine.handle_heal_result("a".to_owned(), false).await;
    assert!(effects.is_empty());
    assert_eq!(
        engine.consecutive_play_errors, 1,
        "now it counts as a strike"
    );
    assert!(engine.last_error.is_some());
}

#[tokio::test]
async fn heal_runs_once_per_track_then_plain_error_path() {
    let mut engine = engine_with_queue(&["a"]);
    engine
        .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
        .await;
    engine.handle_heal_result("a".to_owned(), false).await;
    // The same track failing again must not heal again (no retry loops).
    let effects = engine
        .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
        .await;
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, EngineEffect::YtdlpSelfHeal { .. })),
        "one heal per track per session"
    );
}

#[tokio::test]
async fn stale_heal_result_is_dropped() {
    let mut engine = engine_with_queue(&["a", "b"]);
    engine
        .handle_player_event(PlayerEvent::Error(EXTRACTION_ERR.to_owned()))
        .await;
    // Playback moved on (remote Next) while the check ran.
    engine.queue.next(false);
    let effects = engine.handle_heal_result("a".to_owned(), true).await;
    assert!(effects.is_empty(), "stale heal result is dropped");
    assert_eq!(
        engine.queue.current().map(|s| s.video_id.as_str()),
        Some("b")
    );
}

#[tokio::test]
async fn non_extraction_error_skips_without_healing() {
    for error in [
        "mpv could not play this track (HTTP error 403 Forbidden)",
        "mpv could not play this track (HTTP Error 429: Too Many Requests)",
    ] {
        let mut engine = engine_with_queue(&["a"]);
        let effects = engine
            .handle_player_event(PlayerEvent::Error(error.to_owned()))
            .await;
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, EngineEffect::YtdlpSelfHeal { .. })),
            "HTTP rejection errors take the plain path: {error}"
        );
        assert_eq!(engine.consecutive_play_errors, 1);
        let last_error = engine.last_error.as_deref().unwrap_or_default();
        assert!(last_error.contains("YouTube rejected the stream"));
        assert!(last_error.contains("ytt doctor --verbose"));
        assert!(last_error.contains("JS runtime"));
    }
}
