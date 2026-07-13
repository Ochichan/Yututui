//! Publisher unit tests, split out of publish.rs to keep the module under the
//! size ratchet as the per-topic lanes accumulate.

use super::super::proto::PROTOCOL_VERSION;
use super::super::sessions::{
    RemoteSessionScope, SessionLine, SessionTuning, SubscribeIngress, test_register,
    test_register_next,
};
use super::*;

fn view(queue: &Queue) -> CoreView<'_> {
    test_view(queue)
}

#[test]
fn standalone_settings_snapshot_omits_daemon_only_long_form_seek_fields() {
    let queue = Queue::default();
    let settings = settings_model(&view(&queue), 1);

    assert!(settings.audio.long_form_seek_optimization.is_none());
    assert!(settings.audio.long_form_seek_effective.is_none());
    assert!(settings.audio.long_form_seek_reason.is_none());
    let json = serde_json::to_value(settings).unwrap();
    assert!(json["audio"].get("long_form_seek_optimization").is_none());
    assert!(json["audio"].get("long_form_seek_effective").is_none());
    assert!(json["audio"].get("long_form_seek_reason").is_none());
}

#[test]
fn daemon_settings_snapshot_maps_live_player_long_form_seek_status() {
    use crate::config::LongFormSeekOptimization;
    use crate::player::long_form_seek::{CacheEffectiveState, CacheReason, CacheStatus};

    let queue = Queue::default();
    let mut config = crate::config::Config::default();
    config.audio.mpv.long_form_seek_optimization = LongFormSeekOptimization::On;
    let mut daemon = view(&queue);
    daemon.owner_mode = InstanceMode::Daemon;
    daemon.config = Box::leak(Box::new(config));
    daemon.long_form_seek_status = Some(CacheStatus {
        // The actor may still report the previous request during the immediate confirmation
        // snapshot. Persisted config remains the authoritative requested value.
        requested: LongFormSeekOptimization::Auto,
        effective: CacheEffectiveState::DiskActive,
        reason: CacheReason::AutoUncachedSeek,
        file_generation: Some(7),
        policy_revision: 11,
        file_cache_bytes: 42,
        peak_file_cache_bytes: 84,
    });

    let settings = settings_model(&daemon, 2);
    assert_eq!(
        settings.audio.long_form_seek_optimization,
        Some(LongFormSeekOptimization::On)
    );
    assert_eq!(
        settings.audio.long_form_seek_effective,
        Some(super::super::proto::LongFormSeekEffective::DiskActive)
    );
    assert_eq!(
        settings.audio.long_form_seek_reason,
        Some(super::super::proto::LongFormSeekReason::AutoUncachedSeek)
    );
}

fn song(id: &str) -> Song {
    Song::remote(id, format!("t-{id}"), "a", "3:45")
}

fn drain(rx: &mut tokio::sync::mpsc::Receiver<SessionLine>) -> Vec<SessionLine> {
    let mut out = Vec::new();
    while let Ok(line) = rx.try_recv() {
        out.push(line);
    }
    out
}

fn kinds(lines: &[SessionLine]) -> Vec<String> {
    lines
        .iter()
        .map(|line| match line {
            SessionLine::Raw(bytes) | SessionLine::TrackedRaw { bytes, .. } => format!(
                "raw:{}",
                String::from_utf8_lossy(bytes)
                    .split_once('"')
                    .map(|_| "frame")
                    .unwrap_or("?")
            ),
            SessionLine::Event { topic, .. } => format!("event:{}", topic.wire_str()),
        })
        .collect()
}

fn admit(session: &RemoteSessionRef, page_id: Option<&str>) {
    assert_eq!(
        session.admit_subscribe(page_id, || Some(true)),
        SubscribeIngress::Accepted
    );
}

fn settings_revs(lines: &[SessionLine]) -> Vec<u64> {
    lines
        .iter()
        .filter_map(|line| match line {
            SessionLine::Event {
                topic: Topic::Settings,
                payload,
                ..
            } => serde_json::from_slice::<serde_json::Value>(payload)
                .ok()
                .and_then(|event| event["model"]["rev"].as_u64()),
            _ => None,
        })
        .collect()
}

#[test]
fn subscribe_emits_snapshots_before_reply_and_only_for_new_topics() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();
    queue.set(vec![song("a"), song("b")], 0);

    publisher.handle_subscribe(
        &view(&queue),
        &session,
        None,
        1,
        &[Topic::Player, Topic::Queue, Topic::System],
    );
    let lines = drain(&mut rx);
    assert_eq!(
        kinds(&lines),
        vec!["event:player", "event:queue", "raw:frame"],
        "snapshots strictly precede the reply; system has no snapshot"
    );

    // Duplicate subscribe: idempotent — no second snapshot stream, just the reply.
    publisher.handle_subscribe(&view(&queue), &session, None, 2, &[Topic::Player]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["raw:frame"]);
}

#[test]
fn delayed_subscribe_is_page_gated_and_new_page_gets_a_fresh_snapshot() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();

    admit(&session, Some("page-a"));
    // Hold A's owner event while the reader admits the newer page generation.
    admit(&session, Some("page-b"));
    queue.set(vec![song("fresh-b")], 0);

    assert!(!publisher.handle_subscribe(
        &view(&queue),
        &session,
        Some("page-a"),
        1,
        &[Topic::Queue],
    ));
    assert!(
        drain(&mut rx).is_empty(),
        "stale A must be completely inert"
    );

    assert!(publisher.handle_subscribe(
        &view(&queue),
        &session,
        Some("page-b"),
        2,
        &[Topic::Queue],
    ));
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:queue", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("B must receive a fresh queue snapshot");
    };
    let PushEvent::QueueSnapshot { model } = serde_json::from_slice::<PushEvent>(payload).unwrap()
    else {
        panic!("B must receive a queue snapshot");
    };
    assert_eq!(model.items[0].video_id, "fresh-b");
}

#[test]
fn rejected_page_admission_rolls_back_without_losing_old_subscriptions() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();
    queue.set(vec![song("a")], 0);

    admit(&session, Some("page-a"));
    assert!(publisher.handle_subscribe(
        &view(&queue),
        &session,
        Some("page-a"),
        1,
        &[Topic::Player],
    ));
    drain(&mut rx);

    assert_eq!(
        session.admit_subscribe(Some("page-b"), || Some(false)),
        SubscribeIngress::Busy
    );
    assert!(publisher.handle_subscribe(
        &view(&queue),
        &session,
        Some("page-a"),
        2,
        &[Topic::Player, Topic::Queue],
    ));
    assert_eq!(
        kinds(&drain(&mut rx)),
        vec!["event:queue", "raw:frame"],
        "the rejected B transition must retain A and its Player subscription"
    );
}

#[test]
fn saturated_new_page_cannot_make_an_older_accepted_event_disappear() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut queue = Queue::default();
    queue.set(vec![song("page-a")], 0);
    admit(&session, Some("page-a"));

    let (busy_started_tx, busy_started_rx) = std::sync::mpsc::channel();
    let (release_busy_tx, release_busy_rx) = std::sync::mpsc::channel();
    let busy_session = session.clone();
    let busy = std::thread::spawn(move || {
        busy_session.admit_subscribe(Some("page-b"), || {
            busy_started_tx.send(()).unwrap();
            release_busy_rx.recv().unwrap();
            Some(false)
        })
    });
    busy_started_rx.recv().unwrap();

    let owner_session = session.clone();
    let (owner_started_tx, owner_started_rx) = std::sync::mpsc::channel();
    let (owner_done_tx, owner_done_rx) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        let mut publisher = Publisher::new(hub);
        owner_started_tx.send(()).unwrap();
        let applied = publisher.handle_subscribe(
            &view(&queue),
            &owner_session,
            Some("page-a"),
            1,
            &[Topic::Queue],
        );
        owner_done_tx.send(applied).unwrap();
    });
    owner_started_rx.recv().unwrap();
    assert!(
        owner_done_rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err(),
        "A's owner transaction must wait for B's ingress linearization"
    );

    release_busy_tx.send(()).unwrap();
    assert_eq!(busy.join().unwrap(), SubscribeIngress::Busy);
    assert!(owner_done_rx.recv().unwrap());
    owner.join().unwrap();
    assert_eq!(
        kinds(&drain(&mut rx)),
        vec!["event:queue", "raw:frame"],
        "busy B must leave accepted A able to publish its snapshot and reply"
    );
}

#[test]
fn owner_can_park_projection_work_without_model_subscribers() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();

    assert!(
        publisher.should_observe(false),
        "first turn primes baselines"
    );
    publisher.observe(&view(&queue));
    assert!(
        !publisher.should_observe(false),
        "an empty queue at revision zero must not force perpetual observation"
    );

    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Player]);
    drain(&mut rx);
    assert!(
        publisher.should_observe(false),
        "active model subscriber stays observed"
    );
}

#[test]
fn search_completion_targets_only_its_live_subscribed_session_and_page() {
    let (hub, session_a, mut rx_a) = test_register(SessionTuning::default());
    let (session_b, mut rx_b) = test_register_next(&hub);
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();
    admit(&session_a, Some("page-a"));
    admit(&session_b, Some("page-b"));
    publisher.handle_subscribe(
        &view(&queue),
        &session_a,
        Some("page-a"),
        1,
        &[Topic::Search],
    );
    publisher.handle_subscribe(
        &view(&queue),
        &session_b,
        Some("page-b"),
        1,
        &[Topic::Search],
    );
    drain(&mut rx_a);
    drain(&mut rx_b);

    let requester = RemoteSessionScope::new(session_a.clone(), Some("page-a".to_owned()));
    assert!(publisher.search_completed(
        &requester,
        1,
        "alpha",
        crate::search_source::SearchSource::Youtube,
        &[crate::api::GuiSearchGroup {
            source: crate::search_source::SearchSource::Youtube,
            songs: vec![song("a")],
            error: None,
        }],
        RatingStores {
            library: &crate::library::Library::default(),
            signals: &crate::signals::Signals::default(),
        }
    ));
    let lines = drain(&mut rx_a);
    assert_eq!(lines.len(), 1);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("search completion must be an event");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::SearchCompleted {
            ticket, page_id, ..
        } => {
            assert_eq!(ticket, 1);
            assert_eq!(page_id.as_deref(), Some("page-a"));
        }
        other => panic!("unexpected event {other:?}"),
    }
    assert!(
        drain(&mut rx_b).is_empty(),
        "session B must not see A's result"
    );

    admit(&session_a, Some("page-new"));
    assert!(publisher.handle_subscribe(
        &view(&queue),
        &session_a,
        Some("page-new"),
        2,
        &[Topic::Search],
    ));
    drain(&mut rx_a);
    assert!(!publisher.search_completed(
        &requester,
        2,
        "stale page",
        crate::search_source::SearchSource::Youtube,
        &[],
        RatingStores {
            library: &crate::library::Library::default(),
            signals: &crate::signals::Signals::default(),
        }
    ));
    let current = RemoteSessionScope::new(session_a.clone(), Some("page-new".to_owned()));
    assert!(publisher.search_completed(
        &current,
        2,
        "current page",
        crate::search_source::SearchSource::Youtube,
        &[],
        RatingStores {
            library: &crate::library::Library::default(),
            signals: &crate::signals::Signals::default(),
        }
    ));
    drain(&mut rx_a);

    assert!(!session_a.unsubscribe_if_current(Some("page-a"), &[Topic::Search]));
    assert!(session_a.unsubscribe_if_current(Some("page-new"), &[Topic::Search]));
    assert!(!publisher.search_completed(
        &current,
        3,
        "late",
        crate::search_source::SearchSource::Youtube,
        &[],
        RatingStores {
            library: &crate::library::Library::default(),
            signals: &crate::signals::Signals::default(),
        }
    ));
    assert!(drain(&mut rx_a).is_empty());
}

#[test]
fn cheap_baselines_refresh_while_settings_projection_stays_subscriber_gated() {
    let queue = Queue::default();

    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    assert_eq!(
        publisher.observe_work(&view(&queue)),
        ProjectionWork {
            player_fingerprint: true,
            queue_revision: true,
            settings_model: true,
        },
        "the first owner turn primes each baseline exactly once"
    );
    assert_eq!(
        publisher.observe_work(&view(&queue)),
        ProjectionWork {
            player_fingerprint: true,
            queue_revision: true,
            settings_model: false,
        },
        "origin-compatible cheap baselines refresh without rebuilding settings"
    );
    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Settings]);
    drain(&mut rx);
    assert_eq!(
        publisher.observe_work(&view(&queue)),
        ProjectionWork {
            settings_model: true,
            player_fingerprint: true,
            queue_revision: true,
        },
        "settings projection is added only for a settings subscriber"
    );

    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    publisher.observe(&view(&queue));
    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Player]);
    drain(&mut rx);
    assert_eq!(
        publisher.observe_work(&view(&queue)),
        ProjectionWork {
            player_fingerprint: true,
            queue_revision: true,
            settings_model: false,
        },
        "a player-only client does not build settings"
    );

    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    publisher.observe(&view(&queue));
    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Queue]);
    drain(&mut rx);
    assert_eq!(
        publisher.observe_work(&view(&queue)),
        ProjectionWork {
            player_fingerprint: true,
            queue_revision: true,
            settings_model: false,
        },
        "a queue-only client does not build settings"
    );
}

#[test]
fn time_tick_only_turns_emit_nothing_frozen() {
    // THE frozen no-tick rule (docs/gui/02 §14): elapsed_ms is outside the player
    // fingerprint, so a PlayerTimePos-only turn (elapsed advanced, nothing else)
    // must emit zero events. Do not weaken this test; fix the fingerprint instead.
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();
    queue.set(vec![song("a")], 0);

    // Prime the baseline the way a real host does: observe runs from the first
    // loop turn, long before any subscriber exists.
    publisher.observe(&view(&queue));
    publisher.handle_subscribe(
        &view(&queue),
        &session,
        None,
        1,
        &[Topic::Player, Topic::Queue],
    );
    drain(&mut rx);

    let mut v = view(&queue);
    publisher.observe(&v);
    assert!(drain(&mut rx).is_empty(), "no-change turn emits nothing");

    for tick in 0..30 {
        v.elapsed_ms = Some(1_000 + tick * 1_000);
        publisher.observe(&v);
    }
    assert!(
        drain(&mut rx).is_empty(),
        "30 time-tick turns must emit nothing"
    );

    // A real discontinuity still pushes exactly once (and carries fresh elapsed).
    v.paused = true;
    publisher.observe(&v);
    publisher.observe(&v);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:player"], "once per change");
}

#[test]
fn queue_changes_push_queue_snapshots_but_cursor_moves_do_not() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();
    queue.set(vec![song("a"), song("b"), song("c")], 0);

    publisher.observe(&view(&queue));
    publisher.handle_subscribe(
        &view(&queue),
        &session,
        None,
        1,
        &[Topic::Player, Topic::Queue],
    );
    drain(&mut rx);

    // Membership change → one queue event (and no player event: fingerprint's
    // queue_len changed too, so actually both. Assert the queue event exists and
    // dedup on a second observe).
    queue.extend(vec![song("d")]);
    publisher.observe(&view(&queue));
    let lines = drain(&mut rx);
    assert!(
        kinds(&lines).contains(&"event:queue".to_string()),
        "membership change pushes a queue snapshot: {:?}",
        kinds(&lines)
    );
    publisher.observe(&view(&queue));
    assert!(drain(&mut rx).is_empty(), "no re-push without changes");

    // Cursor move (track advance): player push only — never a queue snapshot.
    queue.next(false);
    publisher.observe(&view(&queue));
    let lines = drain(&mut rx);
    assert_eq!(
        kinds(&lines),
        vec!["event:player"],
        "a track advance is a small player push, not a queue re-push"
    );
}

#[test]
fn late_settings_subscriber_still_sees_changes() {
    // The settings path in `observe` is gated on an actual Settings subscriber (perf: skip
    // the model build + JSON serialize on every keypress when nobody is listening — the
    // default standalone config). This guards the two properties that gate must not break:
    // (1) a subscriber that connects *after* a settings change still learns the current
    // value, and (2) a change made while subscribed still pushes.
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();
    queue.set(vec![song("a")], 0);

    // First observe primes the baseline (no subscriber yet), as the real host does.
    publisher.observe(&view(&queue));

    // A setting changes while nobody is subscribed: the gate skips the serialize and pushes
    // nothing, but the change must not be lost to a future subscriber.
    let mut v = view(&queue);
    v.eq_preset = "Rock";
    publisher.observe(&v);
    assert!(
        drain(&mut rx).is_empty(),
        "no subscriber yet → nothing pushed"
    );

    // Subscriber connects: `handle_subscribe` sends the current settings snapshot.
    publisher.handle_subscribe(&v, &session, None, 1, &[Topic::Settings]);
    let initial = drain(&mut rx);
    assert!(
        kinds(&initial).contains(&"event:settings".to_string()),
        "a new Settings subscriber receives the current snapshot despite the serialize gate"
    );
    assert_eq!(
        settings_revs(&initial),
        vec![1],
        "subscribe reports the existing owner revision without mutating observer baselines"
    );

    // Preserve the established wire sequence: subscription itself never advances the parked
    // observer baseline, so the next owner observation publishes the already-current model.
    publisher.observe(&v);
    let follow_up = drain(&mut rx);
    assert_eq!(settings_revs(&follow_up), vec![2]);

    // A further change while subscribed still pushes a settings snapshot.
    v.eq_preset = "Jazz";
    publisher.observe(&v);
    let changed = drain(&mut rx);
    assert!(
        kinds(&changed).contains(&"event:settings".to_string()),
        "a subscribed settings change still pushes"
    );
    assert_eq!(settings_revs(&changed), vec![3]);
}

#[test]
fn lyrics_publish_retains_for_subscribe_and_broadcasts_only_when_subscribed() {
    use super::super::proto::LyricLineModel;
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();

    assert!(!publisher.lyrics_subscribed());
    // No subscriber: publish retains silently.
    publisher.publish_lyrics(
        Some("v1".to_owned()),
        vec![LyricLineModel {
            ms: Some(5_000),
            text: "line".to_owned(),
        }],
    );
    assert!(drain(&mut rx).is_empty(), "no subscriber → no broadcast");

    // Subscribe: the retained payload is the initial snapshot, before the reply.
    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Lyrics]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:lyrics", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected the retained lyrics snapshot");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::LyricsSnapshot { video_id, lines } => {
            assert_eq!(video_id.as_deref(), Some("v1"));
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].ms, Some(5_000));
        }
        other => panic!("unexpected event {other:?}"),
    }
    assert!(publisher.lyrics_subscribed());

    // Subscribed: a publish broadcasts (e.g. the track-change clearing push).
    publisher.publish_lyrics(Some("v2".to_owned()), Vec::new());
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:lyrics"]);
}

#[test]
fn playlists_publish_retains_for_subscribe_and_broadcasts_only_when_subscribed() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();
    let item = PlaylistSummaryModel {
        id: "mix".to_owned(),
        name: "Mix".to_owned(),
        count: 2,
        description: None,
    };

    assert!(!publisher.playlists_subscribed());
    publisher.publish_playlists(vec![item.clone()]);
    assert!(drain(&mut rx).is_empty(), "no subscriber → no broadcast");

    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Playlists]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:playlists", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected retained playlists snapshot");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::PlaylistsSnapshot { items } => assert_eq!(items, vec![item]),
        other => panic!("unexpected event {other:?}"),
    }
    assert!(publisher.playlists_subscribed());

    publisher.publish_playlists(Vec::new());
    assert_eq!(kinds(&drain(&mut rx)), vec!["event:playlists"]);
}

#[test]
fn downloads_publish_retains_for_subscribe_and_broadcasts_only_when_subscribed() {
    use super::super::proto::{DownloadStateModel, DownloadStatusModel};

    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();
    let item = DownloadStatusModel {
        video_id: "v1".to_owned(),
        title: "Track".to_owned(),
        state: DownloadStateModel::Running,
        pct: 10.0,
        error: None,
    };

    assert!(!publisher.downloads_subscribed());
    publisher.publish_downloads(vec![item.clone()]);
    assert!(drain(&mut rx).is_empty(), "no subscriber → no broadcast");

    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Downloads]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:downloads", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected retained downloads snapshot");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::DownloadsSnapshot { items } => assert_eq!(items, vec![item]),
        other => panic!("unexpected event {other:?}"),
    }
    assert!(publisher.downloads_subscribed());

    publisher.publish_downloads(Vec::new());
    assert_eq!(kinds(&drain(&mut rx)), vec!["event:downloads"]);
}

#[test]
fn transfer_publish_retains_for_subscribe_and_broadcasts_only_when_subscribed() {
    use super::super::proto::{
        SpotifyPlaylistModel, TransferJobModel, TransferPhaseModel, TransferReportModel,
    };

    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();
    let sources = vec![SpotifyPlaylistModel {
        id: "spotify:liked".to_owned(),
        name: "Liked Songs".to_owned(),
        count: 12,
    }];
    let job = TransferJobModel {
        done: 3,
        total: 12,
        matched: 2,
        failed: 1,
    };

    assert!(!publisher.transfer_subscribed());
    publisher.publish_transfer(
        TransferPhaseModel::Running,
        sources.clone(),
        Some(job.clone()),
        None,
        None,
    );
    assert!(drain(&mut rx).is_empty(), "no subscriber → no broadcast");

    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Transfer]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:transfer", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected retained transfer state");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::TransferState {
            phase,
            sources: actual_sources,
            job: actual_job,
            report,
            error,
        } => {
            assert_eq!(phase, TransferPhaseModel::Running);
            assert_eq!(actual_sources, sources);
            assert_eq!(actual_job, Some(job));
            assert_eq!(report, None::<TransferReportModel>);
            assert_eq!(error, None);
        }
        other => panic!("unexpected event {other:?}"),
    }
    assert!(publisher.transfer_subscribed());

    publisher.publish_transfer(TransferPhaseModel::Idle, Vec::new(), None, None, None);
    assert_eq!(kinds(&drain(&mut rx)), vec!["event:transfer"]);
}

#[test]
fn ai_publish_retains_for_subscribe_and_broadcasts_only_when_subscribed() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();
    let message = AiMessageModel {
        role: crate::remote::proto::AiRoleModel::User,
        text: "play something".to_owned(),
    };
    let library = crate::library::Library::default();
    let signals = crate::signals::Signals::default();
    let suggestion = track_model(&song("pick"), &library, &signals);

    publisher.publish_ai(vec![message.clone()], true, vec![suggestion.clone()]);
    assert!(drain(&mut rx).is_empty(), "no subscriber → no broadcast");

    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Ai]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:ai", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected retained AI state");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::AiState {
            messages,
            thinking,
            suggestions,
        } => {
            assert_eq!(messages, vec![message]);
            assert!(thinking);
            assert_eq!(suggestions, vec![suggestion]);
        }
        other => panic!("unexpected event {other:?}"),
    }

    publisher.publish_ai(Vec::new(), false, Vec::new());
    assert_eq!(kinds(&drain(&mut rx)), vec!["event:ai"]);
}

#[test]
fn library_invalidated_is_gated_and_has_no_subscribe_snapshot() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();

    publisher.publish_library_invalidated();
    assert!(drain(&mut rx).is_empty(), "no subscriber → no broadcast");

    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Library]);
    assert_eq!(
        kinds(&drain(&mut rx)),
        vec!["raw:frame"],
        "library has no initial snapshot"
    );

    publisher.publish_library_invalidated();
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:library"]);
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected library invalidation");
    };
    assert_eq!(
        serde_json::from_slice::<PushEvent>(payload).unwrap(),
        PushEvent::LibraryInvalidated
    );
}

#[test]
fn rating_change_on_the_current_track_pushes_a_player_snapshot() {
    // A `rate` mutation changes neither the queue revision nor any transport facet;
    // the fingerprint's favorite/disliked halves are what make it observable.
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let mut queue = Queue::default();
    queue.set(vec![song("a")], 0);

    publisher.observe(&view(&queue));
    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Player]);
    drain(&mut rx);

    let mut favorites = crate::library::Library::default();
    favorites.favorites.push(song("a"));
    let mut v = view(&queue);
    v.library = Box::leak(Box::new(favorites));
    publisher.observe(&v);
    let lines = drain(&mut rx);
    assert_eq!(
        kinds(&lines),
        vec!["event:player"],
        "the favorite flip must re-push the player snapshot"
    );
    let SessionLine::Event { payload, .. } = &lines[0] else {
        panic!("expected a player snapshot");
    };
    let PushEvent::PlayerSnapshot { model } = serde_json::from_slice::<PushEvent>(payload).unwrap()
    else {
        panic!("expected a player snapshot");
    };
    assert!(model.track.expect("current track").favorite);

    publisher.observe(&v);
    assert!(drain(&mut rx).is_empty(), "no re-push without a change");
}

#[test]
fn why_gem_provenance_is_retained_beside_the_transcript_on_the_ai_topic() {
    let (hub, session, mut rx) = test_register(SessionTuning::default());
    let mut publisher = Publisher::new(hub);
    let queue = Queue::default();

    // No subscriber: both ai-topic snapshots retain silently.
    publisher.publish_ai(Vec::new(), false, Vec::new());
    publisher.publish_why_gem(vec!["v1".to_owned()]);
    assert!(publisher.whygem_recorded());
    assert!(drain(&mut rx).is_empty());

    // Subscribing the ai topic serves BOTH retained snapshots before the reply:
    // transcript state first, provenance second.
    publisher.handle_subscribe(&view(&queue), &session, None, 1, &[Topic::Ai]);
    let lines = drain(&mut rx);
    assert_eq!(kinds(&lines), vec!["event:ai", "event:ai", "raw:frame"]);
    let SessionLine::Event { payload, .. } = &lines[1] else {
        panic!("expected the provenance snapshot");
    };
    match serde_json::from_slice::<PushEvent>(payload).unwrap() {
        PushEvent::WhyGemProvenance { video_ids } => assert_eq!(video_ids, vec!["v1"]),
        other => panic!("unexpected event {other:?}"),
    }

    // Subscribed: a provenance change broadcasts.
    publisher.publish_why_gem(vec!["v1".to_owned(), "v2".to_owned()]);
    assert_eq!(kinds(&drain(&mut rx)), vec!["event:ai"]);
}

#[test]
fn duration_parses_from_display_string_when_secs_absent() {
    let s = song("a"); // "3:45", no duration_secs
    assert_eq!(song_duration_ms(&s), Some(225_000));
    let mut hms = song("b");
    hms.duration = "1:02:03".to_string();
    assert_eq!(song_duration_ms(&hms), Some(3_723_000));
    let mut none = song("c");
    none.duration = String::new();
    assert_eq!(song_duration_ms(&none), None);
    let mut secs = song("d");
    secs.duration_secs = Some(20);
    assert_eq!(song_duration_ms(&secs), Some(20_000));
    // A colon-free garbage duration parses into a huge value; the seconds→ms scale must
    // not overflow (debug panic / wrong value) — it returns None (unknown) instead.
    let mut huge = song("e");
    huge.duration = "18446744073709552".to_string(); // parses to u64, but *1000 overflows
    assert_eq!(song_duration_ms(&huge), None);
}

#[test]
fn track_model_sanitizes_persisted_metadata() {
    let mut song = Song::remote("id", "title", "artist", "3:45");
    song.video_id = format!(
        "{}\n{}",
        "x".repeat(crate::api::MAX_PROVIDER_ID_CHARS + 20),
        '\u{202e}'
    );
    song.title = format!(
        "{}{}",
        "t".repeat(crate::api::MAX_TITLE_CHARS + 20),
        '\u{202e}'
    );
    song.artist = "a\nb".to_owned();
    song.album = Some(format!(
        "{}{}",
        "z".repeat(crate::api::MAX_ALBUM_CHARS + 20),
        '\u{202e}'
    ));

    let track = track_model(
        &song,
        &crate::library::Library::default(),
        &crate::signals::Signals::default(),
    );

    assert_eq!(
        track.video_id.chars().count(),
        crate::api::MAX_PROVIDER_ID_CHARS
    );
    assert_eq!(track.title.chars().count(), crate::api::MAX_TITLE_CHARS);
    assert_eq!(track.artist, "ab");
    assert_eq!(
        track.album.as_ref().unwrap().chars().count(),
        crate::api::MAX_ALBUM_CHARS
    );
    assert!(!track.video_id.contains('\u{202e}'));
    assert!(!track.title.contains('\u{202e}'));
    assert!(!track.album.as_ref().unwrap().contains('\u{202e}'));
}

#[test]
fn version_constant_still_v8() {
    // publish.rs is v8-only machinery; a bump above 8 must revisit the snapshots.
    assert_eq!(PROTOCOL_VERSION, 8);
}
