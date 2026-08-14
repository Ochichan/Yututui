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

fn synced_state() -> (
    crate::personal_state::PersonalStateV2,
    crate::personal_state::DeviceId,
) {
    use crate::personal_state::{
        CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
        VersionVector,
    };

    let mut state =
        crate::personal_state::PersonalStateV2::empty("daemon-sync-test".to_owned()).unwrap();
    let author = DeviceId::new("membership").unwrap();
    let mut local_device = None;
    for (index, raw_device_id) in ["device-a", "device-b"].into_iter().enumerate() {
        let secrets = crate::sync::DeviceSecretMaterial::generate_for(raw_device_id).unwrap();
        let device_id = DeviceId::new(raw_device_id).unwrap();
        let dot = Dot {
            device_id: author.clone(),
            sequence: index as u64 + 1,
        };
        state.operations.push(OperationEnvelope {
            operation_id: format!("add-{raw_device_id}"),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: VersionVector::default(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: DeviceRecord {
                    device_id: device_id.clone(),
                    name: raw_device_id.to_owned(),
                    revoked: false,
                    public_identity: Some(secrets.public_identity()),
                },
            },
        });
        state.version_vector.observe(&dot);
        local_device.get_or_insert(device_id);
    }
    crate::personal_state::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    (state, local_device.unwrap())
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
    ]
}

#[test]
fn synced_daemon_persistence_authors_changes_as_the_bound_device() {
    let mut engine = engine_with_queue(&[]);
    let (state, device_id) = synced_state();
    engine.install_personal_state(state);
    engine.personal_state_device_id = Some(device_id.clone());
    engine.library.toggle_favorite(&crate::api::Song::remote(
        "daemon-bound-rating",
        "Daemon bound rating",
        "Artist",
        "3:00",
    ));

    engine.save_library("bound device test");

    assert!(engine.remote_persistence_error.is_none());
    let rating = engine
        .personal_state
        .operations
        .iter()
        .find(|operation| {
            matches!(
                operation.operation,
                crate::personal_state::Operation::SetRating { .. }
            )
        })
        .expect("rating operation");
    assert_eq!(rating.stamp.dot.device_id, device_id);
}

#[test]
fn durable_personal_commit_immediately_retires_a_detached_worker() {
    let mut engine = engine_with_queue(&[]);
    let expected_revision = engine.personal_state_revision();
    let worker_guard = engine.personal_state_revision_guard();
    let (release_worker, wait_for_commit) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        wait_for_commit.recv().unwrap();
        crate::sync::manual::LocalRevisionGuard::ensure_current(&worker_guard, expected_revision)
    });

    engine.library.toggle_favorite(&crate::api::Song::remote(
        "guarded-after-commit",
        "Guarded after commit",
        "Artist",
        "3:00",
    ));
    engine.save_library("revision fence test");

    assert!(engine.personal_state_revision() > expected_revision);
    // Stay inside this owner handler: no outer-loop `PersonalSync::observe` runs before the
    // detached worker resumes and checks the fence published by the persistence gate.
    release_worker.send(()).unwrap();
    assert_eq!(
        worker.join().unwrap(),
        Err(crate::sync::VaultError::RevisionConflict)
    );
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
async fn normalize_apply_reaches_no_player_before_persistence_admission() {
    let _guard = fail_recovery_for_test(recovery_error());
    let mut engine = engine_with_queue(&["seed"]);
    let mut player_rx = install_accepting_player(&mut engine);

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::Normalize { value: true },
        })
        .await;

    assert_eq!(response.reason.as_deref(), Some("persistence_unavailable"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(player_rx.try_recv().is_err());
    assert_eq!(engine.config.normalize, None);
}

#[tokio::test(flavor = "current_thread")]
async fn normalize_apply_reports_unconfirmed_durability_after_live_admission() {
    let _guard = fail_store_saves_for_test(StoreKind::Config);
    let mut engine = engine_with_queue(&["seed"]);
    let mut player_rx = install_accepting_player(&mut engine);

    let (response, shutdown, effects) = engine
        .handle_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::Normalize { value: true },
        })
        .await;

    assert_eq!(response.reason.as_deref(), Some("durability_unconfirmed"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(engine.config.normalize, Some(true));
    assert!(matches!(
        player_rx.try_recv(),
        Ok(PlayerCmd::SetAudioFilter(_))
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
        .handle_remote(RemoteCommand::SetSetting {
            change: RemoteSettingChange::Normalize { value: true },
        })
        .await;

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("durability_unconfirmed"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_ne!(serde_json::to_vec(&engine.config).unwrap(), before_config);
    assert_eq!(engine.config.normalize, Some(true));
    assert_eq!(engine.last_error.as_deref(), Some("mpv transport closed"));
    assert!(
        engine
            .remote_persistence_error
            .as_deref()
            .is_some_and(|error| error.contains("failed to save daemon normalize setting"))
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
