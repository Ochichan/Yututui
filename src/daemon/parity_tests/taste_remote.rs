use crate::api::Song;
use crate::queue::Queue;
use crate::remote::proto::{BanTarget, RemoteCommand};

use super::harness::*;

async fn enable_streaming(app: &mut crate::app::App, engine: &mut super::DaemonEngine) {
    let command = RemoteCommand::Streaming {
        state: crate::remote::proto::ToggleState::On,
    };
    let app_response = app_apply(app, command.clone());
    let (engine_response, shutdown, _) = engine.handle_remote(command).await;
    assert!(app_response.ok, "App streaming setup: {app_response:?}");
    assert!(
        engine_response.ok,
        "daemon streaming setup: {engine_response:?}"
    );
    assert!(!shutdown);
}

async fn status_counts(
    app: &mut crate::app::App,
    engine: &mut super::DaemonEngine,
) -> ((usize, usize, usize), (usize, usize, usize)) {
    let app_status = app_apply(app, RemoteCommand::Status)
        .status
        .expect("App status");
    let (engine_response, _, _) = engine.handle_remote(RemoteCommand::Status).await;
    let engine_status = engine_response.status.expect("daemon status");
    (
        (
            app_status.banned_tracks,
            app_status.banned_artists,
            app_status.seed_terms,
        ),
        (
            engine_status.banned_tracks,
            engine_status.banned_artists,
            engine_status.seed_terms,
        ),
    )
}

fn app_detached_refill(effects: &[crate::app::Cmd]) -> (u64, String) {
    effects
        .iter()
        .find_map(|effect| match effect {
            crate::app::Cmd::StreamingFallback {
                request_id,
                seed_video_id,
                ..
            } => Some((*request_id, seed_video_id.clone())),
            _ => None,
        })
        .expect("App detached refill request")
}

fn engine_detached_refill(effects: &[super::EngineEffect]) -> (u64, String) {
    effects
        .iter()
        .find_map(|effect| match effect {
            super::EngineEffect::StreamingFallback {
                request_id,
                seed_video_id,
                ..
            } => Some((*request_id, seed_video_id.clone())),
            _ => None,
        })
        .expect("daemon detached refill request")
}

async fn deliver_detached_candidate(
    app: &mut crate::app::App,
    app_refill: (u64, String),
    engine: &mut super::DaemonEngine,
    engine_refill: (u64, String),
    candidate: Song,
) {
    let app_commands = app.update(crate::app::StreamingMsg::Results {
        request_id: app_refill.0,
        seed_video_id: app_refill.1,
        candidates: vec![(
            candidate.clone(),
            crate::streaming::CandidateSource::WatchPlaylist,
        )],
    });
    admit_app_player_intents(app, app_commands);
    let _ = engine
        .handle_api_event(crate::api::ApiEvent::StreamingResults {
            request_id: engine_refill.0,
            seed_video_id: engine_refill.1,
            candidates: vec![(candidate, crate::streaming::CandidateSource::WatchPlaylist)],
        })
        .await;
}

#[tokio::test]
async fn ban_track_advances_and_purges_identically_while_streaming() {
    let (mut app, mut engine) = hermetic_pair();
    enable_streaming(&mut app, &mut engine).await;
    let _player_commands = engine.queue_transport_recovery_parity_player();
    let epochs = PositionEpochs::capture(&app, &engine);

    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, shutdown, _) = engine.handle_remote(command).await;

    assert_accepted(
        "ban current track",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    assert!(!shutdown);
    assert_parity("ban current track", &app, &engine);
    epochs.assert_delta(
        "ban current track",
        PositionEpochs::capture(&app, &engine),
        1,
    );
    let (app_counts, engine_counts) = status_counts(&mut app, &mut engine).await;
    assert_eq!(app_counts, engine_counts);
    assert_eq!(app_counts, (1, 0, 0));
}

#[tokio::test]
async fn ban_artist_purges_every_matching_queued_song() {
    let mut queue = Queue::default();
    queue.set(
        vec![
            Song::remote("same-1", "Same 1", "Same Artist", "3:00"),
            Song::remote("same-2", "Same 2", "Same Artist", "3:00"),
            Song::remote("keep", "Keep", "Other Artist", "3:00"),
            Song::remote("same-3", "Same 3", "Same Artist", "3:00"),
        ],
        1,
    );
    let snapshot = queue.snapshot();
    let (mut app, mut engine) = hermetic_pair();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    enable_streaming(&mut app, &mut engine).await;
    let _player_commands = engine.queue_transport_recovery_parity_player();

    let command = RemoteCommand::Ban {
        target: BanTarget::Artist,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;

    assert_accepted(
        "ban current artist",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    assert_parity("ban current artist", &app, &engine);
    assert_eq!(app.core_view().queue.len(), 1);
    let (app_counts, engine_counts) = status_counts(&mut app, &mut engine).await;
    assert_eq!(app_counts, engine_counts);
    assert_eq!(app_counts, (0, 1, 0));
}

#[tokio::test]
async fn ban_is_rejected_when_streaming_is_off() {
    let (mut app, mut engine) = hermetic_pair();
    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;

    assert_eq!(app_response.reason.as_deref(), Some("not_streaming"));
    assert_eq!(engine_response.reason.as_deref(), Some("not_streaming"));
    assert!(!app_response.ok);
    assert!(!engine_response.ok);
    assert_parity("rejected ban", &app, &engine);
}

#[tokio::test]
async fn ban_reports_missing_current_track_and_artist() {
    let (mut app, mut engine) = hermetic_pair();
    enable_streaming(&mut app, &mut engine).await;
    let empty = Queue::default().snapshot();
    app.queue.restore_snapshot(empty.clone());
    engine.restore_queue_snapshot(empty, RNG_SEED);
    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;
    assert_eq!(app_response.reason.as_deref(), Some("no_current_track"));
    assert_eq!(engine_response.reason.as_deref(), Some("no_current_track"));

    let mut queue = Queue::default();
    queue.set(vec![Song::remote("no-artist", "Untitled", "", "3:00")], 0);
    let snapshot = queue.snapshot();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    let command = RemoteCommand::Ban {
        target: BanTarget::Artist,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;
    assert_eq!(app_response.reason.as_deref(), Some("no_artist"));
    assert_eq!(engine_response.reason.as_deref(), Some("no_artist"));

    let mut queue = Queue::default();
    queue.set(vec![Song::remote("", "Untitled", "Artist", "3:00")], 0);
    let snapshot = queue.snapshot();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;
    assert_eq!(app_response.reason.as_deref(), Some("no_track_id"));
    assert_eq!(engine_response.reason.as_deref(), Some("no_track_id"));
}

#[tokio::test]
async fn ban_only_playing_track_stops_both_owners() {
    let mut queue = Queue::default();
    queue.set(vec![song("only")], 0);
    let snapshot = queue.snapshot();
    let (mut app, mut engine) = hermetic_pair();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    enable_streaming(&mut app, &mut engine).await;
    let epochs = PositionEpochs::capture(&app, &engine);

    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let (app_response, app_effects) = app_apply_with_cmds(&mut app, command.clone());
    let (engine_response, _, engine_effects) = engine.handle_remote(command).await;

    assert_accepted(
        "ban only track",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    assert_parity("ban only track", &app, &engine);
    epochs.assert_delta("ban only track", PositionEpochs::capture(&app, &engine), 1);
    assert!(app_effects.iter().any(|effect| matches!(
        effect,
        crate::app::Cmd::StreamingFallback { seed_video_id, .. } if seed_video_id == "only"
    )));
    assert!(engine_effects.iter().any(|effect| matches!(
        effect,
        super::EngineEffect::StreamingFallback { seed_video_id, .. } if seed_video_id == "only"
    )));

    let app_refill = app_detached_refill(&app_effects);
    let engine_refill = engine_detached_refill(&engine_effects);
    let candidate = Song::remote("dQw4w9WgXcQ", "New Song", "Other Artist", "3:00");
    let _player_commands = engine.queue_transport_recovery_parity_player();
    deliver_detached_candidate(
        &mut app,
        app_refill,
        &mut engine,
        engine_refill,
        candidate.clone(),
    )
    .await;
    assert!(app.queue.contains_video_id(&candidate.video_id));
    assert!(
        engine
            .core_view()
            .queue
            .contains_video_id(&candidate.video_id)
    );
    assert_parity("detached refill result", &app, &engine);
}

#[tokio::test]
async fn detached_ban_refill_is_dropped_after_queue_mutation() {
    let mut queue = Queue::default();
    queue.set(vec![song("only")], 0);
    let snapshot = queue.snapshot();
    let (mut app, mut engine) = hermetic_pair();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    enable_streaming(&mut app, &mut engine).await;

    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let (app_response, app_effects) = app_apply_with_cmds(&mut app, command.clone());
    let (engine_response, _, engine_effects) = engine.handle_remote(command).await;
    assert_accepted(
        "ban before queue mutation",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    let app_refill = app_detached_refill(&app_effects);
    let engine_refill = engine_detached_refill(&engine_effects);

    let mutation = RemoteCommand::ToggleShuffle;
    let app_response = app_apply(&mut app, mutation.clone());
    let (engine_response, _, _) = engine.handle_remote(mutation).await;
    assert_accepted(
        "queue mutation after detached ban",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    assert_parity("queue mutation after detached ban", &app, &engine);

    let candidate = Song::remote("dQw4w9WgXcQ", "Stale", "Other Artist", "3:00");
    deliver_detached_candidate(
        &mut app,
        app_refill,
        &mut engine,
        engine_refill,
        candidate.clone(),
    )
    .await;

    assert!(!app.queue.contains_video_id(&candidate.video_id));
    assert!(
        !engine
            .core_view()
            .queue
            .contains_video_id(&candidate.video_id)
    );
    assert_parity("detached refill after queue mutation", &app, &engine);
}

#[tokio::test]
async fn detached_ban_refill_is_dropped_after_streaming_is_disabled() {
    let mut queue = Queue::default();
    queue.set(vec![song("only")], 0);
    let snapshot = queue.snapshot();
    let (mut app, mut engine) = hermetic_pair();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    enable_streaming(&mut app, &mut engine).await;

    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let (app_response, app_effects) = app_apply_with_cmds(&mut app, command.clone());
    let (engine_response, _, engine_effects) = engine.handle_remote(command).await;
    assert_accepted(
        "ban before streaming off",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    let app_refill = app_detached_refill(&app_effects);
    let engine_refill = engine_detached_refill(&engine_effects);

    let command = RemoteCommand::Streaming {
        state: crate::remote::proto::ToggleState::Off,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;
    assert_accepted(
        "streaming off after ban",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );

    let candidate = Song::remote("dQw4w9WgXcQ", "Stale", "Other Artist", "3:00");
    deliver_detached_candidate(
        &mut app,
        app_refill,
        &mut engine,
        engine_refill,
        candidate.clone(),
    )
    .await;

    assert!(!app.queue.contains_video_id(&candidate.video_id));
    assert!(
        !engine
            .core_view()
            .queue
            .contains_video_id(&candidate.video_id)
    );
    assert_parity("detached refill after streaming off", &app, &engine);
}

#[tokio::test]
async fn banning_current_tail_stops_and_keeps_earlier_rows() {
    let mut queue = Queue::default();
    queue.set(vec![song("a"), song("b"), song("c")], 2);
    let snapshot = queue.snapshot();
    let (mut app, mut engine) = hermetic_pair();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    enable_streaming(&mut app, &mut engine).await;

    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;

    assert_accepted(
        "ban current tail",
        &app,
        &engine,
        &app_response,
        &engine_response,
    );
    let app_view = app.core_view();
    let engine_view = engine.core_view();
    let app_ids = app_view
        .queue
        .ordered_iter()
        .map(|song| song.video_id.as_str())
        .collect::<Vec<_>>();
    let engine_ids = engine_view
        .queue
        .ordered_iter()
        .map(|song| song.video_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(app_ids, vec!["a", "b"]);
    assert_eq!(engine_ids, app_ids);
    assert!(app_view.paused && engine_view.paused);
    assert_parity("ban current tail", &app, &engine);
}

#[tokio::test]
async fn ban_advance_skips_unplayable_rows_on_both_owners() {
    let mut queue = Queue::default();
    queue.set(
        vec![
            song("banned"),
            Song::remote("ytpl:PLabcdefgh1234", "Playlist", "Curator", "10 tracks"),
            Song::remote("dQw4w9WgXcQ", "Playable", "Artist", "3:00"),
        ],
        0,
    );
    let snapshot = queue.snapshot();
    let (mut app, mut engine) = hermetic_pair();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    enable_streaming(&mut app, &mut engine).await;
    let _player_commands = engine.queue_transport_recovery_parity_player();

    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let app_response = app_apply(&mut app, command.clone());
    let (engine_response, _, _) = engine.handle_remote(command).await;

    assert!(app_response.ok, "App ban: {app_response:?}");
    assert!(engine_response.ok, "daemon ban: {engine_response:?}");
    assert_eq!(
        app.queue.current().map(|song| song.video_id.as_str()),
        Some("dQw4w9WgXcQ")
    );
    assert_parity("ban skips unplayable rows", &app, &engine);
}

#[tokio::test]
async fn repeated_ban_is_successful_without_growing_taste_counts() {
    let (mut app, mut engine) = hermetic_pair();
    enable_streaming(&mut app, &mut engine).await;
    let snapshot = app.queue.snapshot();
    let _first_player = engine.queue_transport_recovery_parity_player();
    let command = RemoteCommand::Ban {
        target: BanTarget::Track,
    };
    let first_app = app_apply(&mut app, command.clone());
    let (first_engine, _, _) = engine.handle_remote(command.clone()).await;
    assert!(first_app.ok && first_engine.ok);

    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    let _second_player = engine.queue_transport_recovery_parity_player();
    let second_app = app_apply(&mut app, command.clone());
    let (second_engine, _, _) = engine.handle_remote(command).await;

    assert!(second_app.ok, "App repeat: {second_app:?}");
    assert!(second_engine.ok, "daemon repeat: {second_engine:?}");
    assert_parity("repeat ban", &app, &engine);
    let (app_counts, engine_counts) = status_counts(&mut app, &mut engine).await;
    assert_eq!(app_counts, engine_counts);
    assert_eq!(app_counts, (1, 0, 0));
}
