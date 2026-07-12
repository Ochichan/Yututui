use super::*;
use std::time::Duration;

use serde_json::json;

mod perf;

fn song(id: &str) -> Song {
    Song::remote(id, format!("title-{id}"), "artist".to_owned(), "3:00")
}

fn radio_station(id: &str) -> Song {
    let mut song = Song::remote(id, format!("station-{id}"), "", "");
    song.playable = Some(crate::api::PlayableRef::RadioStream {
        url: format!("https://radio.example/{id}.mp3"),
    });
    song
}

pub(super) fn engine_with_queue(ids: &[&str]) -> DaemonEngine {
    let mut queue = Queue::default();
    queue.set(ids.iter().map(|id| song(id)).collect(), 0);
    DaemonEngine {
        maintainer: crate::util::background_task::BackgroundTask::disabled("yt-dlp maintainer"),
        player: None,
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
        library: Library::default(),
        playlists: crate::playlists::Playlists::default(),
        playlists_rev: 0,
        library_invalidations: 0,
        signals: Signals::default(),
        station: StationStore::default(),
        loaded_video_id: None,
        transport_recovery: None,
        transport_recovery_generation: 0,
        transport_auto_recovery_armed: true,
        test_player_starts: VecDeque::new(),
        streaming: false,
        streaming_pending: false,
        last_extend: None,
        consecutive_streaming_failures: 0,
        last_error: None,
        remote_persistence_write_failed: false,
        remote_persistence_error: None,
        remote_persistence_command_active: false,
        remote_persistence_read_only: false,
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
        gui_search_index: GuiSearchIndex::default(),
        why_gem: Vec::new(),
        why_gem_rev: 0,
        video_overlay: None,
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

fn gui_change(
    group: &str,
    field: &str,
    value: serde_json::Value,
) -> crate::remote::proto::GuiSettingChange {
    crate::remote::proto::GuiSettingChange {
        group: group.to_owned(),
        field: field.to_owned(),
        value,
    }
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

fn apply_gui_ok(
    engine: &mut DaemonEngine,
    group: &str,
    field: &str,
    value: serde_json::Value,
) -> Vec<EngineEffect> {
    let (response, effects) = engine.apply_gui_setting(gui_change(group, field, value));
    assert!(response.ok, "{group}.{field} should be accepted");
    effects
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
fn gui_apply_routes_settings_to_live_daemon_state() {
    let mut engine = engine_with_queue(&["seed"]);

    apply_gui_ok(&mut engine, "playback", "speed_tenths", json!(25));
    apply_gui_ok(&mut engine, "playback", "seek_seconds", json!(99));
    apply_gui_ok(&mut engine, "playback", "gapless", json!(true));
    apply_gui_ok(&mut engine, "playback", "enqueue_next", json!(true));
    apply_gui_ok(&mut engine, "playback", "autoplay_on_start", json!(true));
    apply_gui_ok(&mut engine, "playback", "mouse_wheel_volume", json!(true));
    apply_gui_ok(&mut engine, "playback", "media_controls", json!(false));
    apply_gui_ok(&mut engine, "playback", "volume", json!(123));
    apply_gui_ok(&mut engine, "playback", "shuffle", json!(true));
    apply_gui_ok(
        &mut engine,
        "playback",
        "repeat",
        serde_json::to_value(crate::queue::Repeat::Off).unwrap(),
    );

    assert_eq!(engine.playback.speed, crate::config::SPEED_MAX);
    assert_eq!(
        engine.config.seek_seconds,
        Some(crate::config::SEEK_SECONDS_MAX)
    );
    assert_eq!(engine.config.gapless, Some(true));
    assert_eq!(engine.config.enqueue_next, Some(true));
    assert_eq!(engine.config.autoplay_on_start, Some(true));
    assert_eq!(engine.config.mouse_wheel_volume, Some(true));
    assert_eq!(engine.config.media_controls, Some(false));
    assert!(!super::super::daemon_media_enabled(&engine, true));
    apply_gui_ok(&mut engine, "playback", "media_controls", json!(true));
    assert!(super::super::daemon_media_enabled(&engine, true));
    assert!(!super::super::daemon_media_enabled(&engine, false));
    assert_eq!(engine.playback.volume, VOLUME_MAX);
    assert!(engine.queue.shuffle);

    engine.config.audio.mpv.cache_defaults_revision = 0;
    apply_gui_ok(&mut engine, "audio", "mpv_cache_forward", json!("64MiB"));
    apply_gui_ok(&mut engine, "audio", "mpv_cache_back", json!("16MiB"));
    assert_eq!(engine.config.audio.mpv.cache_forward, "64MiB");
    assert_eq!(engine.config.audio.mpv.cache_back, "16MiB");
    assert_eq!(
        engine.config.audio.mpv.cache_defaults_revision,
        crate::config::MPV_CACHE_DEFAULTS_REVISION
    );
    engine.config.audio.mpv.cache_defaults_revision = u64::MAX;
    apply_gui_ok(&mut engine, "audio", "mpv_cache_forward", json!("80MiB"));
    assert_eq!(engine.config.audio.mpv.cache_forward, "80MiB");
    assert_eq!(engine.config.audio.mpv.cache_defaults_revision, u64::MAX);

    apply_gui_ok(&mut engine, "eq", "preset", json!("rock"));
    apply_gui_ok(
        &mut engine,
        "eq",
        "bands",
        json!([0.0, 1.0, 2.0, 3.0, 4.0, 5.0, -1.0, -2.0, -3.0, -4.0]),
    );
    apply_gui_ok(&mut engine, "eq", "normalize", json!(true));
    assert_eq!(engine.config.eq_preset, crate::eq::EqPreset::Custom);
    assert_eq!(engine.config.eq_bands.unwrap()[5], 5.0);
    assert!(engine.current_audio_filter().contains("dynaudnorm"));

    let effects = apply_gui_ok(&mut engine, "streaming", "autoplay", json!(true));
    assert!(engine.streaming);
    assert!(matches!(
        effects.as_slice(),
        [EngineEffect::StreamingFallback { seed_video_id, .. }] if seed_video_id == "seed"
    ));
    apply_gui_ok(
        &mut engine,
        "streaming",
        "mode",
        serde_json::to_value(crate::streaming::StreamingMode::Discovery).unwrap(),
    );
    apply_gui_ok(
        &mut engine,
        "streaming",
        "gemini_model",
        json!("gemini-2.5-flash"),
    );
    apply_gui_ok(&mut engine, "streaming", "ai_enabled", json!(false));
    assert_eq!(
        engine.config.streaming.mode,
        crate::streaming::StreamingMode::Discovery
    );
    assert_eq!(engine.config.ai_enabled, Some(false));

    apply_gui_ok(
        &mut engine,
        "search",
        "default_source",
        serde_json::to_value(crate::search_source::SearchSource::All).unwrap(),
    );
    apply_gui_ok(&mut engine, "search", "soundcloud_enabled", json!(false));
    apply_gui_ok(&mut engine, "search", "audius_enabled", json!(false));
    apply_gui_ok(&mut engine, "search", "jamendo_enabled", json!(false));
    apply_gui_ok(
        &mut engine,
        "search",
        "internet_archive_enabled",
        json!(false),
    );
    apply_gui_ok(&mut engine, "search", "radio_browser_enabled", json!(false));
    apply_gui_ok(
        &mut engine,
        "search",
        "audius_app_name",
        json!("  daemon app  "),
    );
    apply_gui_ok(
        &mut engine,
        "search",
        "jamendo_client_id",
        serde_json::Value::Null,
    );
    assert_eq!(
        engine.config.search.audius_app_name.as_deref(),
        Some("daemon app")
    );
    assert_eq!(engine.config.search.jamendo_client_id, None);

    apply_gui_ok(&mut engine, "ui", "language", json!("ko"));
    apply_gui_ok(&mut engine, "ui", "mouse", json!(true));
    apply_gui_ok(&mut engine, "ui", "album_art", json!(true));
    apply_gui_ok(&mut engine, "ui", "romanized_titles", json!(true));
    assert_eq!(engine.config.language, crate::i18n::Language::Korean);
    assert_eq!(engine.config.mouse, Some(true));
    assert_eq!(engine.config.album_art, Some(true));
    assert_eq!(engine.config.romanized_titles, Some(true));

    apply_gui_ok(
        &mut engine,
        "storage",
        "download_dir",
        json!("/tmp/ytm-downloads"),
    );
    apply_gui_ok(
        &mut engine,
        "storage",
        "cookies_file",
        serde_json::Value::Null,
    );
    apply_gui_ok(&mut engine, "storage", "download_concurrency", json!(16));
    assert_eq!(
        engine.config.download_dir.as_deref(),
        Some(std::path::Path::new("/tmp/ytm-downloads"))
    );
    assert_eq!(engine.config.cookies_file, None);
    assert_eq!(engine.config.download_concurrency, Some(16));

    apply_gui_ok(&mut engine, "animations", "fps", json!(999));
    apply_gui_ok(&mut engine, "animations", "master", json!(true));
    apply_gui_ok(&mut engine, "animations", "bounce", json!(true));
    assert_eq!(engine.config.animations.fps, crate::config::FPS_MAX);
    assert!(engine.config.animations.master);
    assert!(engine.config.animations.bounce);

    apply_gui_ok(&mut engine, "theme", "preset", json!("light"));
    apply_gui_ok(&mut engine, "theme", "retro", json!(true));
    apply_gui_ok(&mut engine, "theme", "accent", json!("#112233"));
    assert_eq!(engine.config.theme.preset, "light");
    assert!(engine.config.retro_mode);
    assert_eq!(
        engine
            .config
            .theme
            .effective_hex(crate::theme::ThemeRole::Accent),
        "#112233"
    );
}

#[test]
fn gui_apply_rejects_bad_values_and_unknown_fields() {
    let mut engine = engine_with_queue(&["seed"]);

    for (group, field, value, reason) in [
        ("playback", "speed_tenths", json!("fast"), "bad_value"),
        ("eq", "preset", json!("not-a-preset"), "bad_value"),
        ("streaming", "mode", json!("invalid"), "bad_value"),
        ("search", "audius_app_name", json!(42), "bad_value"),
        ("ui", "language", json!("fr"), "bad_value"),
        ("storage", "download_concurrency", json!(0), "bad_value"),
        ("animations", "nope", json!(true), "bad_value"),
        ("theme", "accent", json!("not-hex"), "bad_value"),
        ("theme", "not_a_role", json!("#ffffff"), "unknown_setting"),
        ("nope", "field", json!(true), "unknown_setting"),
    ] {
        let (response, effects) = engine.apply_gui_setting(gui_change(group, field, value));
        assert!(!response.ok, "{group}.{field} should be rejected");
        assert_eq!(response.reason.as_deref(), Some(reason));
        assert!(effects.is_empty());
    }
}

#[test]
fn gui_search_index_resolution_prefers_visible_rows_then_library_then_safe_fallback() {
    let mut engine = engine_with_queue(&[]);
    let requester = RequesterKey::new(1, Some("page-a".to_owned()));
    let searched = Song::from_source(
        crate::search_source::SearchSource::Jamendo,
        "jam-1",
        "Jam title",
        "Jam artist",
        "2:00",
        crate::api::PlayableRef::DirectUrl {
            source: crate::search_source::SearchSource::Jamendo,
            url: "https://cdn.example/audio.mp3".to_owned(),
        },
    );
    engine.index_gui_search(
        &requester,
        &[crate::api::GuiSearchGroup {
            source: crate::search_source::SearchSource::Jamendo,
            songs: vec![searched.clone()],
            error: None,
        }],
    );
    let searched_row_id = crate::api::gui_search_row_id(&searched);
    assert_eq!(
        engine
            .resolve_video_id(Some(&requester), &searched_row_id)
            .unwrap()
            .watch_url(),
        "https://cdn.example/audio.mp3"
    );

    engine.library.favorites.push(song("dQw4w9WgXcQ"));
    assert_eq!(
        engine
            .resolve_video_id(Some(&requester), "dQw4w9WgXcQ")
            .unwrap()
            .title,
        "title-dQw4w9WgXcQ"
    );
    let fallback = engine
        .resolve_video_id(Some(&requester), "TAfHyXrULiM")
        .unwrap();
    assert_eq!(fallback.title, "TAfHyXrULiM");
    assert!(
        engine
            .resolve_video_id(Some(&requester), "bad/not/video")
            .is_none()
    );
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
async fn remote_commands_cover_no_load_branches_and_gui_search_dispatch() {
    let mut engine = engine_with_queue(&[]);

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

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::RunSearch {
            ticket: 1,
            query: "   ".to_owned(),
            source: crate::search_source::SearchSource::Youtube,
        })
        .await;
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("empty_query"));
    assert!(!shutdown);
    assert!(effects.is_empty());

    let (response, _, effects) = engine
        .handle_remote(RemoteCommand::RunSearch {
            ticket: 2,
            query: "x".repeat(REMOTE_MAX_QUERY_BYTES + 1),
            source: crate::search_source::SearchSource::Youtube,
        })
        .await;
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("query_too_long"));
    assert!(effects.is_empty());

    let requester = RequesterKey::new(1, Some("engine-page".to_owned()));
    let (response, _, effects) = engine
        .handle_session_remote(
            RemoteCommand::RunSearch {
                ticket: 3,
                query: "  city pop  ".to_owned(),
                source: crate::search_source::SearchSource::SoundCloud,
            },
            requester,
        )
        .await;
    assert!(response.ok);
    assert!(matches!(
        effects.as_slice(),
        [EngineEffect::GuiSearch {
            ticket: 3,
            query,
            source: crate::search_source::SearchSource::SoundCloud,
            ..
        }] if query == "city pop"
    ));

    let (response, _, effects) = engine
        .handle_remote(RemoteCommand::SetGeminiKey {
            key: "  key-123  ".to_owned(),
        })
        .await;
    assert!(response.ok);
    assert!(effects.is_empty());
    assert_eq!(engine.config.gemini_api_key.as_deref(), Some("key-123"));

    let (response, _, _) = engine
        .handle_remote(RemoteCommand::SetGeminiKey {
            key: "   ".to_owned(),
        })
        .await;
    assert!(response.ok);
    assert!(engine.config.gemini_api_key.is_none());

    engine.transport_recovery = Some(TransportRecovery {
        video_id: "queued-before-quit".to_owned(),
        paused: false,
        generation: 9,
        attempts: 0,
    });
    engine.transport_auto_recovery_armed = true;
    let (response, shutdown, effects) = engine.handle_remote(RemoteCommand::Quit).await;
    assert!(response.ok);
    assert!(shutdown);
    assert!(effects.is_empty());
    assert!(engine.loaded_video_id.is_none());
    assert!(engine.transport_recovery.is_none());
    assert!(!engine.transport_auto_recovery_armed);
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

    let (response, effects) = engine.apply_gui_setting(gui_change(
        "playback",
        "repeat",
        serde_json::to_value(crate::queue::Repeat::All).unwrap(),
    ));
    assert!(!response.ok);
    assert_eq!(
        response.reason.as_deref(),
        Some("incompatible_playback_modes")
    );
    assert!(effects.is_empty());
    assert_eq!(engine.queue.repeat, crate::queue::Repeat::Off);

    engine.streaming = false;
    engine.queue.repeat = crate::queue::Repeat::All;
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

    engine.transport_recovery = Some(TransportRecovery {
        video_id: "queued-before-media-quit".to_owned(),
        paused: false,
        generation: 11,
        attempts: 0,
    });
    engine.transport_auto_recovery_armed = true;
    let (shutdown, effects) = engine.handle_media(crate::media::MediaCommand::Quit).await;
    assert!(shutdown);
    assert!(effects.is_empty());
    assert!(engine.loaded_video_id.is_none());
    assert!(engine.transport_recovery.is_none());
    assert!(!engine.transport_auto_recovery_armed);
}

#[tokio::test]
async fn api_streaming_events_extend_clear_pending_and_trip_circuit_breaker() {
    let mut engine = engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.streaming = true;
    engine.streaming_pending = true;
    engine.consecutive_streaming_failures = 2;

    let additions = vec![song("fresh-a"), song("fresh-b")];
    let effects = engine
        .handle_api_event(ApiEvent::StreamingPreflighted {
            seed_video_id: "seed".to_owned(),
            songs: additions,
        })
        .await;
    assert!(effects.is_empty());
    assert!(!engine.streaming_pending);
    assert_eq!(engine.consecutive_streaming_failures, 0);
    assert!(
        engine
            .queue
            .ordered_iter()
            .any(|song| song.video_id == "fresh-a")
    );

    engine.streaming_pending = true;
    let effects = engine
        .handle_api_event(ApiEvent::StreamingResults {
            seed_video_id: "not-in-queue".to_owned(),
            candidates: vec![(song("ignored"), CandidateSource::YtdlpStreaming)],
        })
        .await;
    assert!(effects.is_empty());
    assert!(!engine.streaming_pending);
    assert!(
        !engine
            .queue
            .ordered_iter()
            .any(|song| song.video_id == "ignored")
    );

    for idx in 0..AUTOPLAY_MAX_FAILURES {
        engine.streaming = true;
        engine
            .handle_api_event(ApiEvent::StreamingError {
                seed_video_id: "seed".to_owned(),
                error: format!("failure-{idx}"),
            })
            .await;
    }
    assert!(!engine.streaming);
    assert_eq!(engine.config.autoplay_streaming, Some(false));
    assert!(
        engine
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("autoplay streaming failed")
    );

    for inert in [
        ApiEvent::TrackResolved {
            seq: 1,
            result: Ok(Vec::new()),
        },
        ApiEvent::SearchError {
            request_id: 1,
            source: crate::search_source::SearchSource::Youtube,
            error: "offline".to_owned(),
        },
        ApiEvent::PlaylistTracksError {
            title: "mix".to_owned(),
            error: "private".to_owned(),
        },
    ] {
        assert!(engine.handle_api_event(inert).await.is_empty());
    }
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

    // Over-long Gemini key is rejected and does not overwrite the stored key.
    let (resp, _, _) = engine
        .handle_remote(RemoteCommand::SetGeminiKey {
            key: "k".repeat(REMOTE_MAX_GEMINI_KEY_BYTES + 1),
        })
        .await;
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("key_too_long"));
    assert!(engine.config.gemini_api_key.is_none());

    // A request containing an unknown row is rejected as an indivisible stale selection.
    let (resp, _, _) = engine
        .handle_remote(RemoteCommand::EnqueueTracks {
            video_ids: vec!["not-a-valid-id".into(), "also/bad".into()],
        })
        .await;
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("stale_results"));
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

#[test]
fn session_snapshot_preserves_local_mode_queue() {
    let mut engine = engine_with_queue(&["local-a", "local-b"]);
    engine.last_mode = LastMode::Local;
    engine.queue.next(false);
    engine.inactive_normal_queue = Some({
        let mut queue = Queue::default();
        queue.set(vec![song("normal")], 0);
        queue.snapshot()
    });
    engine.inactive_radio_queue = Some({
        let mut queue = Queue::default();
        queue.set(vec![radio_station("radio")], 0);
        queue.snapshot()
    });

    let cache = engine.session_cache_snapshot();

    assert_eq!(cache.last_mode, LastMode::Local);
    assert_eq!(cache.local_queue.as_ref().map(|s| s.cursor), Some(1));
    assert_eq!(cache.normal_queue.as_ref().map(|s| s.songs.len()), Some(1));
    assert_eq!(cache.radio_queue.as_ref().map(|s| s.songs.len()), Some(1));
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
