use super::persistence_gate::{fail_recovery_for_test, fail_store_saves_for_test};
use super::tests::{engine_with_queue, install_accepting_player};
use super::*;
use crate::persist::{StartupRecoveryError, StartupRecoveryFailure, StoreKind};

fn recovery_error() -> StartupRecoveryError {
    StartupRecoveryError {
        store: StoreKind::Config,
        failure: StartupRecoveryFailure::LockFailure {
            kind: std::io::ErrorKind::WouldBlock,
            error: "injected recovery ownership loss".to_owned(),
        },
    }
}

fn mutating_commands() -> Vec<RemoteCommand> {
    vec![
        RemoteCommand::Next,
        RemoteCommand::Prev,
        RemoteCommand::TogglePause,
        RemoteCommand::Play {
            query: "query".to_owned(),
        },
        RemoteCommand::Enqueue {
            query: "query".to_owned(),
        },
        RemoteCommand::VolumeUp,
        RemoteCommand::VolumeDown,
        RemoteCommand::SetVolume { percent: 75 },
        RemoteCommand::SeekBack,
        RemoteCommand::SeekForward,
        RemoteCommand::SeekTo { ms: 1_000 },
        RemoteCommand::ToggleShuffle,
        RemoteCommand::CycleRepeat,
        RemoteCommand::QueuePlay { position: 0 },
        RemoteCommand::QueueRemove { position: 0 },
        RemoteCommand::Streaming {
            state: ToggleState::On,
        },
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::SeekSeconds { seconds: 9 },
        },
        RemoteCommand::ResumeSession,
        RemoteCommand::PlayTracks {
            video_ids: vec!["video-id".to_owned()],
        },
        RemoteCommand::EnqueueTracks {
            video_ids: vec!["video-id".to_owned()],
        },
        RemoteCommand::Apply {
            change: crate::remote::proto::GuiSettingChange {
                group: "playback".to_owned(),
                field: "enqueue_next".to_owned(),
                value: serde_json::Value::Bool(true),
            },
        },
        RemoteCommand::SetGeminiKey {
            key: "secret".to_owned(),
        },
        RemoteCommand::ResetAllSettings,
    ]
}

#[test]
fn startup_recovery_failure_is_preserved_as_typed_engine_error() {
    let expected = recovery_error();
    let error = EngineError::from(expected.clone());

    assert!(matches!(
        &error,
        EngineError::StartupRecovery(actual) if actual == &expected
    ));
    assert_eq!(error.reason(), "persistence_unavailable");
}

#[tokio::test(flavor = "current_thread")]
async fn late_recovery_failure_rejects_every_mutating_remote_command_before_mutation() {
    let _guard = fail_recovery_for_test(recovery_error());

    for command in mutating_commands() {
        let mut engine = engine_with_queue(&["seed"]);
        engine.last_error = Some("mpv transport closed".to_owned());
        let before_status = engine.status();
        let before_config = serde_json::to_vec(&engine.config).unwrap();

        let (response, shutdown, effects) = engine.handle_remote(command).await;

        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("persistence_unavailable"));
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_eq!(engine.status(), before_status);
        assert_eq!(serde_json::to_vec(&engine.config).unwrap(), before_config);
        assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
        assert!(
            engine
                .remote_persistence_error
                .as_deref()
                .is_some_and(|error| error.contains("injected recovery ownership loss"))
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn long_form_seek_apply_reaches_no_player_before_persistence_admission() {
    let _guard = fail_recovery_for_test(recovery_error());
    let mut engine = engine_with_queue(&["seed"]);
    let mut player_rx = install_accepting_player(&mut engine);

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::Apply {
            change: crate::remote::proto::GuiSettingChange {
                group: "audio".to_owned(),
                field: "long_form_seek_optimization".to_owned(),
                value: serde_json::json!("on"),
            },
        })
        .await;

    assert_eq!(response.reason.as_deref(), Some("persistence_unavailable"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(player_rx.try_recv().is_err());
    assert_eq!(
        engine.config.audio.mpv.long_form_seek_optimization,
        crate::config::LongFormSeekOptimization::Off
    );
}

#[tokio::test(flavor = "current_thread")]
async fn long_form_seek_apply_reports_unconfirmed_durability_after_live_admission() {
    let _guard = fail_store_saves_for_test(StoreKind::Config);
    let mut engine = engine_with_queue(&["seed"]);
    let mut player_rx = install_accepting_player(&mut engine);

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::Apply {
            change: crate::remote::proto::GuiSettingChange {
                group: "audio".to_owned(),
                field: "long_form_seek_optimization".to_owned(),
                value: serde_json::json!("on"),
            },
        })
        .await;

    assert_eq!(response.reason.as_deref(), Some("durability_unconfirmed"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(
        engine.config.audio.mpv.long_form_seek_optimization,
        crate::config::LongFormSeekOptimization::On
    );
    assert!(matches!(
        player_rx.try_recv(),
        Ok(PlayerCmd::SetLongFormSeekOptimization(
            crate::config::LongFormSeekOptimization::On
        ))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn read_only_whitelist_continues_while_recovery_is_unavailable() {
    let _guard = fail_recovery_for_test(recovery_error());
    let _save_guard = fail_store_saves_for_test(StoreKind::Session);
    let mut engine = engine_with_queue(&[]);
    engine.last_error = Some("mpv transport closed".to_owned());

    let (status, shutdown, effects) = engine.handle_remote(RemoteCommand::Status).await;
    assert!(status.ok);
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
    assert!(engine.remote_persistence_error.is_some());

    let (search, shutdown, effects) = engine
        .handle_session_remote(
            RemoteCommand::RunSearch {
                ticket: 1,
                query: "city pop".to_owned(),
                source: crate::search_source::SearchSource::All,
            },
            RequesterKey::new(7, Some("page".to_owned())),
        )
        .await;
    assert!(search.ok);
    assert!(!shutdown);
    assert!(matches!(
        effects.as_slice(),
        [EngineEffect::GuiSearch { .. }]
    ));

    let (quit, shutdown, effects) = engine.handle_remote(RemoteCommand::Quit).await;
    assert!(quit.ok);
    assert!(shutdown);
    assert!(effects.is_empty());
    assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
    assert!(
        engine
            .remote_persistence_error
            .as_deref()
            .is_some_and(|error| error.contains("injected recovery ownership loss"))
    );
    assert!(!engine.remote_persistence_write_failed);
}

#[tokio::test(flavor = "current_thread")]
async fn resume_rechecks_recovery_after_loading_before_mutating_queue_or_player() {
    let _guard = fail_recovery_for_test(recovery_error());
    let mut engine = engine_with_queue(&["seed"]);
    engine.last_error = Some("mpv transport closed".to_owned());
    let before = engine.status();

    let response = engine.resume_session().await;

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("persistence_unavailable"));
    assert_eq!(engine.status(), before);
    assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
    assert!(
        engine
            .remote_persistence_error
            .as_deref()
            .is_some_and(|error| error.contains("injected recovery ownership loss"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn direct_config_save_failure_reports_unconfirmed_durability_after_applying_state() {
    let _guard = fail_store_saves_for_test(StoreKind::Config);
    let mut engine = engine_with_queue(&[]);
    engine.last_error = Some("mpv transport closed".to_owned());
    let before_config = serde_json::to_vec(&engine.config).unwrap();

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::SetGeminiKey {
            key: "secret".to_owned(),
        })
        .await;

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("durability_unconfirmed"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_ne!(serde_json::to_vec(&engine.config).unwrap(), before_config);
    assert_eq!(engine.config.gemini_api_key.as_deref(), Some("secret"));
    assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
    assert!(
        engine
            .remote_persistence_error
            .as_deref()
            .is_some_and(|error| error.contains("failed to save daemon gemini key"))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn healthy_preflight_preserves_non_persistence_last_error() {
    let mut engine = engine_with_queue(&[]);
    engine.last_error = Some("mpv transport closed".to_owned());

    let (response, _, _) = engine.handle_remote(RemoteCommand::Status).await;

    assert!(response.ok);
    assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
    assert!(engine.remote_persistence_error.is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn next_library_save_failure_preserves_applied_player_and_owner_state() {
    let _guard = fail_store_saves_for_test(StoreKind::Library);
    let mut engine = engine_with_queue(&["a", "b"]);
    let mut player_rx = install_accepting_player(&mut engine);
    engine.loaded_video_id = Some("a".to_owned());
    engine.playback.paused = false;
    let before_status = engine.status();
    let before_library = serde_json::to_vec(&engine.library).unwrap();
    let before_signals = serde_json::to_vec(&engine.signals).unwrap();
    let before_session = engine.session_cache_snapshot();

    let (response, shutdown, effects) = engine.handle_remote(RemoteCommand::Next).await;

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("durability_unconfirmed"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_ne!(engine.status(), before_status);
    assert_ne!(serde_json::to_vec(&engine.library).unwrap(), before_library);
    assert_ne!(serde_json::to_vec(&engine.signals).unwrap(), before_signals);
    assert_ne!(
        serde_json::to_vec(&engine.session_cache_snapshot()).unwrap(),
        serde_json::to_vec(&before_session).unwrap()
    );
    assert_eq!(engine.status().position, 2);
    assert_eq!(engine.loaded_video_id.as_deref(), Some("b"));
    assert!(matches!(
        player_rx.try_recv(),
        Ok(crate::player::PlayerCmd::Load(_))
    ));
    assert!(
        engine
            .remote_persistence_error
            .as_deref()
            .is_some_and(|error| error.contains("failed to save daemon library history"))
    );
}
