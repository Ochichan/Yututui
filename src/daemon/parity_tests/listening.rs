use crate::remote::proto::{ListeningRemoteAction, RemoteCommand};

use super::harness::{app_apply, hermetic_pair};

#[tokio::test]
async fn listening_activation_and_empty_projection_have_owner_parity() {
    let (mut app, mut engine) = hermetic_pair();

    for action in [ListeningRemoteAction::Enable, ListeningRemoteAction::List] {
        let command = RemoteCommand::Listening { action };
        let app_response = app_apply(&mut app, command.clone());
        let (daemon_response, shutdown, effects) = engine.handle_remote(command).await;

        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_eq!(app_response.ok, daemon_response.ok);
        assert_eq!(app_response.reason, daemon_response.reason);
        assert_eq!(app_response.message, daemon_response.message);
    }
}

#[tokio::test]
async fn bookmark_rejections_match_without_playback_or_confirmed_position() {
    for has_track in [false, true] {
        let (mut app, mut engine) = hermetic_pair();
        let enable = RemoteCommand::Listening {
            action: ListeningRemoteAction::Enable,
        };
        assert!(app_apply(&mut app, enable.clone()).ok);
        assert!(engine.handle_remote(enable).await.0.ok);
        if has_track {
            let mut song = super::harness::song("position-pending");
            song.duration = "30:00".to_owned();
            song.duration_secs = Some(1_800);
            app.seed_listening_for_test(&song, f64::NAN, 1_800.0);
            engine.seed_listening_for_test(&song, f64::NAN, 1_800.0);
        }
        let list = RemoteCommand::Listening {
            action: ListeningRemoteAction::List,
        };
        let before = app_apply(&mut app, list.clone()).message;
        let command = RemoteCommand::Listening {
            action: ListeningRemoteAction::BookmarkAdd {
                label: "Unconfirmed point".to_owned(),
            },
        };
        let app_response = app_apply(&mut app, command.clone());
        let (daemon_response, shutdown, effects) = engine.handle_remote(command).await;
        let expected = if has_track {
            "position_unknown"
        } else {
            "nothing_playing"
        };
        assert!(!app_response.ok && !daemon_response.ok);
        assert_eq!(app_response.reason.as_deref(), Some(expected));
        assert_eq!(daemon_response.reason.as_deref(), Some(expected));
        assert!(!shutdown && effects.is_empty());
        assert_eq!(app_apply(&mut app, list.clone()).message, before);
        assert_eq!(engine.handle_remote(list).await.0.message, before);
    }
}

#[tokio::test]
async fn bookmark_add_has_owner_parity_for_the_same_confirmed_playback() {
    let (mut app, mut engine) = hermetic_pair();
    let enable = RemoteCommand::Listening {
        action: ListeningRemoteAction::Enable,
    };
    assert!(app_apply(&mut app, enable.clone()).ok);
    assert!(engine.handle_remote(enable).await.0.ok);

    let mut song = super::harness::song("long-form");
    song.duration = "30:00".to_owned();
    song.duration_secs = Some(1_800);
    app.seed_listening_for_test(&song, 125.0, 1_800.0);
    engine.seed_listening_for_test(&song, 125.0, 1_800.0);

    let command = RemoteCommand::Listening {
        action: ListeningRemoteAction::BookmarkAdd {
            label: "Chapter two".to_owned(),
        },
    };
    let app_response = app_apply(&mut app, command.clone());
    let (daemon_response, shutdown, effects) = engine.handle_remote(command).await;

    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(app_response.ok, daemon_response.ok);
    assert_eq!(app_response.reason, daemon_response.reason);
    assert_eq!(app_response.message, daemon_response.message);
}

#[tokio::test]
async fn restart_clears_resume_after_both_owners_admit_the_seek() {
    let (mut app, mut engine) = hermetic_pair();
    let enable = RemoteCommand::Listening {
        action: ListeningRemoteAction::Enable,
    };
    assert!(app_apply(&mut app, enable.clone()).ok);
    assert!(engine.handle_remote(enable).await.0.ok);

    let mut song = super::harness::song("long-restart");
    song.duration = "30:00".to_owned();
    song.duration_secs = Some(1_800);
    app.seed_listening_for_test(&song, 600.0, 1_800.0);
    engine.seed_listening_for_test(&song, 600.0, 1_800.0);
    let _player_rx = engine.install_seek_parity_player(&song.video_id, 600.0, 1_800.0);

    let restart = RemoteCommand::Listening {
        action: ListeningRemoteAction::Restart,
    };
    let app_response = app_apply(&mut app, restart.clone());
    let (daemon_response, _, _) = engine.handle_remote(restart).await;
    assert_eq!(app_response.ok, daemon_response.ok);
    assert_eq!(app_response.reason, daemon_response.reason);
    assert_eq!(app_response.message, daemon_response.message);

    let list = RemoteCommand::Listening {
        action: ListeningRemoteAction::List,
    };
    let app_list = app_apply(&mut app, list.clone());
    let daemon_list = engine.handle_remote(list).await.0;
    assert_eq!(app_list.message, daemon_list.message);
    assert!(app_list.message.unwrap().contains("resume-clear"));
}

#[tokio::test]
async fn shorter_replacement_media_clears_an_outdated_automatic_point() {
    let (mut app, mut engine) = hermetic_pair();
    let enable = RemoteCommand::Listening {
        action: ListeningRemoteAction::Enable,
    };
    assert!(app_apply(&mut app, enable.clone()).ok);
    assert!(engine.handle_remote(enable).await.0.ok);

    let mut song = super::harness::song("duration-changed");
    song.duration = "30:00".to_owned();
    song.duration_secs = Some(1_800);
    app.seed_listening_for_test(&song, 600.0, 1_800.0);
    engine.seed_listening_for_test(&song, 600.0, 1_800.0);

    app.reopen_listening_for_test(&song, 300.0);
    engine.reopen_listening_for_test(&song, 300.0);

    let list = RemoteCommand::Listening {
        action: ListeningRemoteAction::List,
    };
    let app_list = app_apply(&mut app, list.clone());
    let daemon_list = engine.handle_remote(list).await.0;
    assert_eq!(app_list.message, daemon_list.message);
    assert!(app_list.message.unwrap().contains("resume-clear"));
}

#[tokio::test]
async fn invalid_record_mutations_fail_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    let enable = RemoteCommand::Listening {
        action: ListeningRemoteAction::Enable,
    };
    assert!(app_apply(&mut app, enable.clone()).ok);
    assert!(engine.handle_remote(enable).await.0.ok);

    let mut song = super::harness::song("long-invalid");
    song.duration_secs = Some(1_800);
    app.seed_listening_for_test(&song, 120.0, 1_800.0);
    engine.seed_listening_for_test(&song, 120.0, 1_800.0);
    let command = RemoteCommand::Listening {
        action: ListeningRemoteAction::BookmarkAdd {
            label: "https://private.invalid/path".to_owned(),
        },
    };

    let app_response = app_apply(&mut app, command.clone());
    let daemon_response = engine.handle_remote(command).await.0;
    assert!(!app_response.ok);
    assert!(!daemon_response.ok);
    assert_eq!(app_response.reason, daemon_response.reason);
}

#[tokio::test]
async fn loading_a_new_track_replaces_a_stale_pending_seek_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    let enable = RemoteCommand::Listening {
        action: ListeningRemoteAction::Enable,
    };
    assert!(app_apply(&mut app, enable.clone()).ok);
    assert!(engine.handle_remote(enable).await.0.ok);

    let stale = super::harness::song("stale-pending");
    let mut target = super::harness::song("new-resume");
    target.duration = "30:00".to_owned();
    target.duration_secs = Some(1_800);

    assert!(app.stale_pending_allows_resume_for_test(&stale, &target));
    assert!(engine.stale_pending_allows_resume_for_test(&stale, &target));
}

#[tokio::test]
async fn rejected_daemon_restart_keeps_the_saved_resume_point() {
    let (_, mut engine) = hermetic_pair();
    let enable = RemoteCommand::Listening {
        action: ListeningRemoteAction::Enable,
    };
    assert!(engine.handle_remote(enable).await.0.ok);
    let mut song = super::harness::song("rejected-restart");
    song.duration = "30:00".to_owned();
    song.duration_secs = Some(1_800);
    engine.seed_listening_for_test(&song, 600.0, 1_800.0);

    let response = engine
        .handle_remote(RemoteCommand::Listening {
            action: ListeningRemoteAction::Restart,
        })
        .await
        .0;
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("mpv_unavailable"));

    let list = engine
        .handle_remote(RemoteCommand::Listening {
            action: ListeningRemoteAction::List,
        })
        .await
        .0;
    let message = list.message.expect("listening projection");
    assert!(message.contains("600000ms"));
    assert!(!message.contains("resume-clear"));
}
