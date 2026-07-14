use super::ingress::RuntimeTelemetrySlot;
use super::*;
use crate::remote::proto::RemoteCommand;
use crate::util::event_policy::{EventKey, EventLane, EventPolicy};

mod coalescing;
mod downloads;
mod persistence;
mod player_startup;
mod policy;
mod task_set;

fn song(video_id: &str) -> crate::api::Song {
    crate::api::Song::from_search(
        video_id,
        format!("Title {video_id}"),
        "Artist",
        "3:21",
        Some("Album".to_owned()),
    )
}

fn on_demand_load(url: &str) -> PlayerCmd {
    PlayerCmd::load(url, crate::player::MediaSourceContext::OnDemand)
}

fn transfer_progress(job_id: &str) -> crate::transfer::TransferProgress {
    crate::transfer::TransferProgress {
        job_id: job_id.to_owned(),
        stage: crate::transfer::Stage::Matching,
        done: 1,
        total: 2,
        matched: 1,
        auto_accepted: 0,
        ambiguous: 0,
        not_found: 0,
        written: 0,
        current: "Artist - Title".to_owned(),
    }
}

fn update_status() -> crate::update::UpdateStatus {
    crate::update::UpdateStatus {
        current: "1.0.0".to_owned(),
        latest: "v1.0.1".to_owned(),
        available: true,
        first_seen: true,
        method: crate::update::InstallMethod::Cargo,
    }
}

#[test]
fn pending_player_cmds_preserve_active_player_barriers_and_order() {
    let mut pending = PendingPlayerCmds::default();
    assert_eq!(
        pending.push(PlayerCmd::SetVolume(10)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        pending.push(PlayerCmd::SetProperty {
            name: "speed".to_owned(),
            value: serde_json::json!(1.2),
        }),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        pending.push(PlayerCmd::SetVolume(30)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        pending.push(on_demand_load("https://example.invalid/old")),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        pending.push(PlayerCmd::CyclePause),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        pending.push(on_demand_load("https://example.invalid/new")),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        pending.push(PlayerCmd::SeekRelative(2.0)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert!(matches!(
        pending.push(PlayerCmd::SeekRelative(3.0)),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            ..
        })
    ));
    assert!(matches!(
        pending.push(PlayerCmd::interactive_seek(12.0)),
        Ok(DeliveryReceipt::Coalesced { .. })
    ));
    assert_eq!(
        pending.push(PlayerCmd::CyclePause),
        Ok(DeliveryReceipt::Deferred)
    );
    assert!(matches!(
        pending.push(PlayerCmd::CyclePause),
        Ok(DeliveryReceipt::Coalesced { .. })
    ));

    let drained = pending.drain();

    assert_eq!(drained.len(), 7);
    assert!(matches!(drained[0], PlayerCmd::SetVolume(10)));
    assert!(matches!(
        &drained[1],
        PlayerCmd::SetProperty { name, value }
            if name == "speed" && value == &serde_json::json!(1.2)
    ));
    assert!(matches!(drained[2], PlayerCmd::SetVolume(30)));
    assert!(matches!(
        &drained[3],
        PlayerCmd::Load(url) if url == "https://example.invalid/old"
    ));
    assert!(matches!(drained[4], PlayerCmd::CyclePause));
    assert!(matches!(
        &drained[5],
        PlayerCmd::Load(url) if url == "https://example.invalid/new"
    ));
    assert!(matches!(
        drained[6],
        PlayerCmd::SeekAbsolute {
            seconds: value,
            precision: crate::player::SeekPrecision::InteractiveFast,
        } if (value - 12.0).abs() < f64::EPSILON
    ));
}

#[test]
fn pending_player_cmds_cap_rejects_without_evicting_admitted_commands() {
    let mut pending = PendingPlayerCmds::default();
    assert_eq!(
        pending.push(on_demand_load("https://example.invalid/current")),
        Ok(DeliveryReceipt::Deferred)
    );
    for _ in 0..(PENDING_PLAYER_CMDS_MAX - 1) {
        assert_eq!(pending.push(PlayerCmd::Stop), Ok(DeliveryReceipt::Deferred));
    }
    assert_eq!(pending.push(PlayerCmd::Stop), Err(DeliveryError::Saturated));

    assert_eq!(pending.len(), PENDING_PLAYER_CMDS_MAX);
    let drained = pending.drain();
    assert!(matches!(
        &drained[0],
        PlayerCmd::Load(url) if url == "https://example.invalid/current"
    ));
    assert!(
        drained[1..]
            .iter()
            .all(|cmd| matches!(cmd, PlayerCmd::Stop))
    );
}

#[test]
fn pending_player_cmds_coalesce_relative_seek_bursts_before_capacity() {
    let mut pending = PendingPlayerCmds::default();
    assert_eq!(
        pending.push(PlayerCmd::SeekRelative(1.0)),
        Ok(DeliveryReceipt::Deferred)
    );
    for _ in 0..(PENDING_PLAYER_CMDS_MAX * 2) {
        assert!(matches!(
            pending.push(PlayerCmd::SeekRelative(1.0)),
            Ok(DeliveryReceipt::Coalesced { .. })
        ));
    }

    assert_eq!(pending.len(), 1);
    let drained = pending.drain();
    assert!(matches!(
        drained.as_slice(),
        [PlayerCmd::SeekRelative(value)]
            if (*value - (PENDING_PLAYER_CMDS_MAX * 2 + 1) as f64).abs() < f64::EPSILON
    ));
}

#[test]
fn pending_player_batch_rejection_rolls_back_earlier_staged_coalescing() {
    let mut pending = PendingPlayerCmds::default();
    for _ in 0..(PENDING_PLAYER_CMDS_MAX - 1) {
        assert!(pending.push(PlayerCmd::Stop).is_ok());
    }
    assert!(pending.push(on_demand_load("old")).is_ok());

    assert_eq!(
        pending.push_batch(vec![on_demand_load("new"), PlayerCmd::Stop]),
        Err(DeliveryError::Saturated)
    );

    let drained = pending.drain();
    assert_eq!(drained.len(), PENDING_PLAYER_CMDS_MAX);
    assert!(matches!(
        drained.last(),
        Some(PlayerCmd::Load(url)) if url == "old"
    ));
}

#[test]
fn full_pending_player_batch_can_cancel_before_atomic_publish() {
    let mut pending = PendingPlayerCmds::default();
    for _ in 0..PENDING_PLAYER_CMDS_MAX {
        assert!(pending.push(PlayerCmd::Stop).is_ok());
    }

    assert_eq!(
        pending.push_batch(vec![PlayerCmd::CyclePause, PlayerCmd::CyclePause]),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            evicted_oldest: false,
        })
    );
    assert_eq!(pending.len(), PENDING_PLAYER_CMDS_MAX);
    assert!(
        pending
            .drain()
            .iter()
            .all(|command| matches!(command, PlayerCmd::Stop))
    );
}

#[test]
fn empty_pending_player_batch_is_not_an_admitted_intent() {
    let mut pending = PendingPlayerCmds::default();
    assert_eq!(pending.push_batch(Vec::new()), Err(DeliveryError::Busy));
    assert_eq!(pending.len(), 0);
}

#[test]
fn transport_restore_batch_is_ordered_and_rejected_without_a_visible_prefix() {
    let restore = || {
        vec![
            on_demand_load("https://example.invalid/recovered"),
            PlayerCmd::SetAudioFilter("lavfi=[volume=1]".to_owned()),
            PlayerCmd::CyclePause,
        ]
    };

    let mut pending = PendingPlayerCmds::default();
    assert_eq!(pending.push_batch(restore()), Ok(DeliveryReceipt::Deferred));
    let admitted = pending.drain();
    assert!(matches!(
        admitted.as_slice(),
        [
            PlayerCmd::Load(url),
            PlayerCmd::SetAudioFilter(filter),
            PlayerCmd::CyclePause,
        ] if url == "https://example.invalid/recovered" && filter == "lavfi=[volume=1]"
    ));

    for _ in 0..(PENDING_PLAYER_CMDS_MAX - 1) {
        assert!(pending.push(PlayerCmd::Stop).is_ok());
    }
    assert!(
        pending
            .push(on_demand_load("https://example.invalid/original"))
            .is_ok()
    );
    assert_eq!(pending.push_batch(restore()), Err(DeliveryError::Saturated));
    let unchanged = pending.drain();
    assert_eq!(unchanged.len(), PENDING_PLAYER_CMDS_MAX);
    assert!(matches!(
        unchanged.last(),
        Some(PlayerCmd::Load(url)) if url == "https://example.invalid/original"
    ));
}

#[test]
fn rejected_player_intent_keeps_state_and_replies_with_correlated_busy() {
    let mut app = App::new(50);
    let epoch = app.playback.position_epoch;
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let intent = crate::app::PlayerIntent {
        commands: vec![PlayerCmd::interactive_seek(42.0)],
        commit: crate::app::PlayerCommit::Seek {
            optimistic_position: Some(42.0),
        },
        label: "seek_absolute",
        remote_reply: Some(crate::app::PendingRemoteReply {
            sender: reply.into(),
            response: crate::app::RemoteReplyPlan::Status,
        }),
    };

    assert!(settle_player_intent(&mut app, intent, Err(DeliveryError::Busy)).is_empty());
    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.playback.position_epoch, epoch);
    assert!(app.status_visible(), "rejection status must arm its expiry");
    assert_eq!(app.status.kind, crate::app::StatusKind::Error);
    assert!(!app.status.text.is_empty());
    let response = reply_rx.try_recv().expect("correlated busy response");
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("player_busy"));
}

#[test]
fn rejected_eq_and_normalize_controls_keep_session_audio_state() {
    fn key(code: crossterm::event::KeyCode, modifiers: crossterm::event::KeyModifiers) -> Msg {
        Msg::Key(crossterm::event::KeyEvent {
            code,
            modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    let mut app = App::new(50);
    app.dropdowns.eq_open = true;
    app.dropdowns.streaming_open = true;
    app.dropdowns.search_source_open = true;
    let mut cmds = app.update(key(
        crossterm::event::KeyCode::Char('e'),
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(app.audio.preset, crate::eq::EqPreset::Flat);
    assert!(app.dropdowns.eq_open);
    assert!(app.dropdowns.streaming_open);
    assert!(app.dropdowns.search_source_open);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("equalizer")
        )
    ));
    let crate::app::Cmd::PlayerControl(crate::app::PlayerControl::Intent(intent)) =
        cmds.pop().expect("EQ intent")
    else {
        panic!("expected EQ player intent");
    };
    assert!(settle_player_intent(&mut app, *intent, Err(DeliveryError::Busy)).is_empty());
    assert_eq!(app.audio.preset, crate::eq::EqPreset::Flat);
    assert_eq!(app.audio.bands, [0.0; crate::eq::BANDS]);
    assert!(app.dropdowns.eq_open);
    assert!(app.dropdowns.streaming_open);
    assert!(app.dropdowns.search_source_open);

    let mut app = App::new(50);
    let mut cmds = app.update(key(
        crossterm::event::KeyCode::Char('N'),
        crossterm::event::KeyModifiers::SHIFT,
    ));
    assert!(!app.audio.normalize);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("dynaudnorm")
        )
    ));
    let crate::app::Cmd::PlayerControl(crate::app::PlayerControl::Intent(intent)) =
        cmds.pop().expect("normalize intent")
    else {
        panic!("expected normalize player intent");
    };
    assert!(settle_player_intent(&mut app, *intent, Err(DeliveryError::Closed)).is_empty());
    assert!(!app.audio.normalize);
    assert_eq!(app.config.normalize, None);
}

#[test]
fn rejected_remote_audio_settings_do_not_commit_or_persist_and_reply_with_error() {
    let mut app = App::new(50);
    app.status.text = "before".to_owned();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let mut cmds = app.update(Msg::Remote(
        RemoteCommand::SetSetting {
            change: crate::remote::proto::RemoteSettingChange::Speed { tenths: 13 },
        },
        reply.into(),
    ));
    assert_eq!(app.playback.speed, 1.0);
    assert_eq!(app.config.speed, None);
    assert_eq!(app.status.text, "before");
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "speed" && value == &serde_json::json!(1.3)
        )
    ));
    let crate::app::Cmd::PlayerControl(crate::app::PlayerControl::Intent(intent)) =
        cmds.pop().expect("remote speed intent")
    else {
        panic!("expected remote speed intent");
    };
    let follow_ups = settle_player_intent(&mut app, *intent, Err(DeliveryError::Busy));
    assert!(follow_ups.is_empty(), "rejected speed must not persist");
    assert_eq!(app.playback.speed, 1.0);
    assert_eq!(app.config.speed, None);
    assert_eq!(
        reply_rx
            .try_recv()
            .expect("correlated speed rejection")
            .reason
            .as_deref(),
        Some("player_busy")
    );

    let mut app = App::new(50);
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let mut cmds = app.update(Msg::Remote(
        RemoteCommand::SetSetting {
            change: crate::remote::proto::RemoteSettingChange::Normalize { value: true },
        },
        reply.into(),
    ));
    assert!(!app.audio.normalize);
    assert_eq!(app.config.normalize, None);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("dynaudnorm")
        )
    ));
    let crate::app::Cmd::PlayerControl(crate::app::PlayerControl::Intent(intent)) =
        cmds.pop().expect("remote normalize intent")
    else {
        panic!("expected remote normalize intent");
    };
    let follow_ups = settle_player_intent(&mut app, *intent, Err(DeliveryError::Closed));
    assert!(follow_ups.is_empty(), "rejected normalize must not persist");
    assert!(!app.audio.normalize);
    assert_eq!(app.config.normalize, None);
    assert_eq!(
        reply_rx
            .try_recv()
            .expect("correlated normalize rejection")
            .reason
            .as_deref(),
        Some("player_unavailable")
    );
}

#[test]
fn rejected_mouse_seek_intent_clears_preview_without_committing_position() {
    let mut app = App::new(50);
    app.mode = crate::app::Mode::Player;
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(ratatui::layout::Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    let press = app.update(crate::app::Msg::MouseClick {
        col: 25,
        row: 5,
        multi: false,
    });
    assert!(press.is_empty());
    let mut cmds = app.update(crate::app::Msg::MouseLeftUp);
    let crate::app::Cmd::PlayerControl(crate::app::PlayerControl::Intent(intent)) =
        cmds.pop().expect("mouse seek intent")
    else {
        panic!("expected a player intent");
    };

    assert!(settle_player_intent(&mut app, *intent, Err(DeliveryError::Busy)).is_empty());
    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.seekbar_preview_target(), None);
    assert!(
        app.update(crate::app::Msg::MouseDrag { col: 25, row: 9 })
            .is_empty()
    );
}

#[test]
fn accepted_player_intent_commits_position_through_central_epoch_path() {
    let mut app = App::new(50);
    let epoch = app.playback.position_epoch;
    let intent = crate::app::PlayerIntent {
        commands: vec![PlayerCmd::interactive_seek(42.0)],
        commit: crate::app::PlayerCommit::Seek {
            optimistic_position: Some(42.0),
        },
        label: "seek_absolute",
        remote_reply: None,
    };

    assert!(settle_player_intent(&mut app, intent, Ok(DeliveryReceipt::Deferred)).is_empty());
    assert_eq!(app.playback.time_pos, Some(42.0));
    assert_eq!(app.playback.position_epoch, epoch + 1);
}

#[test]
fn rejected_video_load_returns_typed_unpause_compensation() {
    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = App::new(50);
        app.playback.paused = true;
        app.set_video_pause_ownership_for_test(true);

        let mut follow_ups = settle_video_load_delivery(&mut app, Err(error));

        assert!(
            app.playback.paused,
            "audio resumed before compensation admission"
        );
        assert!(
            app.video_pause_owned_for_test(),
            "ownership cleared before admission"
        );
        assert!(matches!(
            follow_ups.as_slice(),
            [cmd] if matches!(
                cmd.player_command(),
                Some(PlayerCmd::SetProperty { name, value })
                    if name == "pause" && value == &serde_json::Value::Bool(false)
            )
        ));
        let crate::app::Cmd::PlayerControl(crate::app::PlayerControl::Intent(intent)) =
            follow_ups.pop().expect("video unpause compensation")
        else {
            panic!("expected typed player compensation");
        };

        assert!(settle_player_intent(&mut app, *intent, Ok(DeliveryReceipt::Deferred),).is_empty());
        assert!(!app.playback.paused);
        assert!(!app.video_pause_owned_for_test());
    }
}

#[test]
fn accepted_video_load_leaves_overlay_pause_ownership_unchanged() {
    let mut app = App::new(50);
    app.playback.paused = true;
    app.set_video_pause_ownership_for_test(true);

    let follow_ups = settle_video_load_delivery(&mut app, Ok(DeliveryReceipt::Enqueued));

    assert!(follow_ups.is_empty());
    assert!(app.playback.paused);
    assert!(app.video_pause_owned_for_test());
}

#[test]
fn player_restart_gate_allows_one_replacement_and_prevents_loops() {
    let mut gate = PlayerRestartGate::default();
    assert_eq!(gate.request(), PlayerRestartDecision::Start);
    assert_eq!(gate.request(), PlayerRestartDecision::AlreadyPending);
    assert!(gate.take_request());
    assert_eq!(gate.request(), PlayerRestartDecision::AlreadyPending);
    assert!(gate.complete_start());
    assert_eq!(gate.request(), PlayerRestartDecision::Exhausted);
    assert!(!gate.take_request());
}

#[test]
fn player_restart_gate_owner_exit_suppresses_queued_and_future_replacements() {
    let mut gate = PlayerRestartGate::default();
    assert_eq!(gate.request(), PlayerRestartDecision::Start);

    // Models a TransportClosed reduced immediately before either normal Quit committed or the
    // out-of-band latch won. Owner exit must revoke it before the runner's sole spawn point.
    gate.suppress_for_shutdown();
    assert!(!gate.take_request());
    assert_eq!(gate.request(), PlayerRestartDecision::Suppressed);
    assert!(!gate.take_request());
}

#[test]
fn actor_rejection_releases_optimistic_ui_guards() {
    let mut app = App::new(50);

    app.lyrics.loading = true;
    assert!(recover_actor_rejection(&mut app, ActorRejectionRecovery::Lyrics).is_none());
    assert!(!app.lyrics.loading);

    app.art.loading = true;
    assert!(recover_actor_rejection(&mut app, ActorRejectionRecovery::Artwork).is_none());
    assert!(!app.art.loading);

    app.ai.thinking = true;
    assert!(recover_actor_rejection(&mut app, ActorRejectionRecovery::AiTurn).is_none());
    assert!(!app.ai.thinking);

    app.streaming.feedback_in_flight = true;
    assert!(recover_actor_rejection(&mut app, ActorRejectionRecovery::AiFeedback).is_none());
    assert!(!app.streaming.feedback_in_flight);

    app.transfer_running = true;
    assert!(recover_actor_rejection(&mut app, ActorRejectionRecovery::TransferStart).is_none());
    assert!(!app.transfer_running);

    assert!(recover_actor_rejection(&mut app, ActorRejectionRecovery::TransferCancel).is_none());
    assert!(app.transfer_running);
    assert!(app.dirty);
}

#[test]
fn rejected_ai_rerank_schedules_empty_result_fallback() {
    let mut app = App::new(50);
    app.ai.thinking = true;

    let event = recover_actor_rejection(
        &mut app,
        ActorRejectionRecovery::AiRerank("seed".to_owned()),
    )
    .expect("rerank rejection must schedule the reducer's local fallback");

    assert!(!app.ai.thinking);
    assert!(matches!(
        event,
        Msg::Streaming(StreamingMsg::AiPicks {
            seed_video_id,
            picks,
            conf: None,
        }) if seed_video_id == "seed" && picks.is_empty()
    ));
}

#[test]
fn runtime_event_kind_and_telemetry_slots_are_stable() {
    assert_eq!(
        RuntimeEvent::Ai(crate::ai::AiEvent::Chat("hi".to_owned())).kind(),
        "ai"
    );
    assert_eq!(
        RuntimeEvent::Api(crate::api::ApiEvent::StreamingError {
            seed_video_id: "seed".to_owned(),
            error: "e".to_owned(),
        })
        .kind(),
        "api"
    );
    assert_eq!(
        RuntimeEvent::Download(crate::download::DownloadEvent::Done {
            video_id: "v".to_owned(),
            path: "v.m4a".to_owned(),
        })
        .kind(),
        "download"
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).kind(),
        "player"
    );
    assert_eq!(
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).kind(),
        "signal"
    );
    assert_eq!(RuntimeEvent::TelemetryWake.kind(), "telemetry_wake");
    assert!(RuntimeEvent::TelemetryWake.is_telemetry_wake());
    assert!(!RuntimeEvent::Player(crate::player::PlayerEvent::Eof).is_telemetry_wake());

    assert_eq!(
        RuntimeEvent::App(Msg::Download(crate::app::DownloadMsg::Progress {
            video_id: "a".to_owned(),
            percent: 1.0,
        }))
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::DownloadProgress("a".to_owned()))
    );
    assert_eq!(
        RuntimeEvent::Download(crate::download::DownloadEvent::Done {
            video_id: "a".to_owned(),
            path: "a.m4a".to_owned(),
        })
        .telemetry_slot(),
        None
    );
    assert_eq!(
        RuntimeEvent::App(Msg::MediaArtworkReady(
            crate::media::artwork::MediaArtworkReady {
                key: "cover-key".to_owned(),
                path: "cover.jpg".into(),
            },
        ))
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::MediaArt("cover-key".to_owned()))
    );
    assert_eq!(
        RuntimeEvent::App(Msg::Local(crate::app::LocalMsg::ScanProgress(
            crate::local::LocalScanProgress::default(),
        )))
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::Static(EventKey::LocalScanProgress))
    );
    assert_eq!(
        RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
            transfer_progress("import-1"),
        ))
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::TransferProgress(
            "import-1".to_owned()
        ))
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).telemetry_slot(),
        Some(RuntimeTelemetrySlot::Static(EventKey::PlayerTimePos))
    );
    assert_eq!(
        RuntimeEvent::Api(crate::api::ApiEvent::SearchError {
            request_id: 42,
            source: crate::search_source::SearchSource::Youtube,
            error: "nope".to_owned(),
        })
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::StaleSearch(42))
    );
    assert_eq!(
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).telemetry_slot(),
        None
    );
    assert_eq!(
        RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("prefetch"),
            purpose: crate::resolver::ResolvePurpose::Prefetch,
        })
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::StaleResolver("prefetch".to_owned()))
    );
    assert_eq!(
        RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("heal"),
            purpose: crate::resolver::ResolvePurpose::SelfHeal,
        })
        .telemetry_slot(),
        None
    );
    assert_eq!(
        RuntimeEvent::Video {
            generation: 9,
            event: crate::player::video::VideoEvent::Paused(true),
        }
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::VideoPaused(9))
    );
    assert_eq!(
        RuntimeEvent::Video {
            generation: 9,
            event: crate::player::video::VideoEvent::Failed("closed".to_owned()),
        }
        .telemetry_slot(),
        None
    );
}

#[test]
fn app_message_policy_covers_backpressure_lanes() {
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    assert_eq!(
        app_msg_policy(&Msg::Remote(RemoteCommand::TogglePause, reply.into())),
        EventPolicy::MustReplyOrBusy {
            lane: EventLane::RemoteCommand,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Download(crate::app::DownloadMsg::Progress {
            video_id: "v".to_owned(),
            percent: 12.0,
        })),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::DownloadProgress,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::MediaArtworkReady(
            crate::media::artwork::MediaArtworkReady {
                key: "v".to_owned(),
                path: "cover.jpg".into(),
            },
        )),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::MediaArtVideo,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Local(crate::app::LocalMsg::ScanProgress(
            crate::local::LocalScanProgress::default(),
        ))),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::LocalScanProgress,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::SearchError {
            request_id: 1,
            source: crate::search_source::SearchSource::Youtube,
            error: "offline".to_owned(),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::SearchRequest,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::TrackResolved {
            seq: 1,
            result: Ok(Vec::new()),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::ResolverVideo,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Streaming(StreamingMsg::Error {
            seed_video_id: "seed".to_owned(),
            error: "empty".to_owned(),
        })),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Streaming(StreamingMsg::Resolved {
            video_id: "seed".to_owned(),
            stream_url: "https://example.invalid/audio".to_owned(),
            self_heal: true,
        })),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::ResolveFailed {
            video_id: "seed".to_owned(),
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert!(matches!(
        app_msg_policy(&Msg::Noop),
        EventPolicy::BestEffort { .. }
    ));
    assert_eq!(
        app_msg_policy(&Msg::Media(crate::media::MediaCommand::Play)),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Transfer(
            crate::transfer::actor::TransferEvent::Disconnected,
        )),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
}

#[test]
fn player_message_policy_covers_each_property_lane() {
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::TimePos(1.0)),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerTimePos,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::Metadata(serde_json::json!({"title":"Song"}))),
        EventPolicy::CoalesceLatest {
            lane: EventLane::WorkResult,
            key: EventKey::PlayerMetadata,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::AudioCodec(Some("aac".to_owned()))),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerAudioCodec,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::VideoOverlay {
            generation: 2,
            event: crate::player::video::VideoEvent::Closed,
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::Error("boom".to_owned())),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
}

#[test]
fn runtime_event_to_msg_preserves_ai_api_and_transport_payloads() {
    let msg = Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::SetStationProfile {
        query: "city pop".to_owned(),
        explore: Some("wide".to_owned()),
        avoid_artists: vec!["skip".to_owned()],
    }));
    assert!(matches!(
        msg,
        Msg::Ai(AiMsg::SetStationProfile {
            query,
            explore: Some(explore),
            avoid_artists,
        }) if query == "city pop" && explore == "wide" && avoid_artists == ["skip"]
    ));

    let msg = Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::StreamingPicks {
        seed_video_id: "seed".to_owned(),
        picks: vec![crate::app::AiPick {
            cid: "c1".to_owned(),
            role: Some("bridge".to_owned()),
            reasons: vec!["tr".to_owned()],
        }],
        conf: Some(0.75),
    }));
    assert!(matches!(
        msg,
        Msg::Streaming(StreamingMsg::AiPicks {
            seed_video_id,
            picks,
            conf: Some(conf),
        }) if seed_video_id == "seed" && picks[0].cid == "c1" && (conf - 0.75).abs() < f32::EPSILON
    ));

    let msg = Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::SearchResults {
        request_id: 3,
        query: "query".to_owned(),
        source: crate::search_source::SearchSource::SoundCloud,
        songs: vec![song("v1")],
        timed_out: true,
    }));
    assert!(matches!(
        msg,
        Msg::SearchResults {
            request_id: 3,
            query,
            source: crate::search_source::SearchSource::SoundCloud,
            songs,
            timed_out: true,
        } if query == "query" && songs[0].video_id == "v1"
    ));

    let msg = Msg::from(RuntimeEvent::Api(
        crate::api::ApiEvent::PlaylistTracksError {
            title: "mix".to_owned(),
            error: "denied".to_owned(),
        },
    ));
    assert!(matches!(
        msg,
        Msg::PlaylistTracksError { title, error } if title == "mix" && error == "denied"
    ));

    let msg = Msg::from(RuntimeEvent::Download(
        crate::download::DownloadEvent::Error {
            video_id: "v2".to_owned(),
            error: "disk".to_owned(),
        },
    ));
    assert!(matches!(
        msg,
        Msg::Download(crate::app::DownloadMsg::Error { video_id, error })
            if video_id == "v2" && error == "disk"
    ));

    let msg = Msg::from(RuntimeEvent::Player(
        crate::player::PlayerEvent::FileFormat(Some("mp4".to_owned())),
    ));
    assert!(matches!(
        msg,
        Msg::Player(PlayerMsg::FileFormat(Some(format))) if format == "mp4"
    ));

    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    let msg = Msg::from(RuntimeEvent::Remote(
        crate::remote::server::RemoteEvent::Command(RemoteCommand::Next, reply.into()),
    ));
    assert!(matches!(msg, Msg::Remote(RemoteCommand::Next, _)));

    let msg = Msg::from(RuntimeEvent::Video {
        generation: 42,
        event: crate::player::video::VideoEvent::Failed("403".to_owned()),
    });
    assert!(matches!(
        msg,
        Msg::Player(PlayerMsg::VideoOverlay {
            generation: 42,
            event: crate::player::video::VideoEvent::Failed(error),
        }) if error == "403"
    ));
}

#[test]
fn runtime_event_to_msg_validates_resolver_urls_and_side_channels() {
    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Resolved {
            video_id: crate::ids::VideoId::from("v1"),
            stream_url: crate::ids::StreamUrl::from("https://rr1---sn.test/video.m4a"),
            purpose: crate::resolver::ResolvePurpose::Prefetch,
        },
    ));
    assert!(matches!(
        msg,
        Msg::Streaming(StreamingMsg::Resolved {
            video_id,
            stream_url,
            self_heal: false,
        }) if video_id == "v1" && stream_url.starts_with("https://")
    ));

    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Resolved {
            video_id: crate::ids::VideoId::from("heal"),
            stream_url: crate::ids::StreamUrl::from("https://rr1---sn.test/healed.m4a"),
            purpose: crate::resolver::ResolvePurpose::SelfHeal,
        },
    ));
    assert!(matches!(
        msg,
        Msg::Streaming(StreamingMsg::Resolved {
            video_id,
            self_heal: true,
            ..
        }) if video_id == "heal"
    ));

    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Resolved {
            video_id: crate::ids::VideoId::from("v2"),
            stream_url: crate::ids::StreamUrl::from("file:///etc/passwd"),
            purpose: crate::resolver::ResolvePurpose::SelfHeal,
        },
    ));
    assert!(matches!(msg, Msg::ResolveFailed { video_id } if video_id == "v2"));

    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("ordinary"),
            purpose: crate::resolver::ResolvePurpose::Prefetch,
        },
    ));
    assert!(
        matches!(msg, Msg::Noop),
        "ordinary prefetch failure cannot consume a pending self-heal latch"
    );

    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("heal"),
            purpose: crate::resolver::ResolvePurpose::SelfHeal,
        },
    ));
    assert!(matches!(msg, Msg::ResolveFailed { video_id } if video_id == "heal"));

    let msg = Msg::from(RuntimeEvent::Api(
        crate::api::ApiEvent::GuiSearchCompleted {
            request_id: crate::api::GuiSearchRequestId::new(0, 1),
            groups: Vec::new(),
        },
    ));
    assert!(matches!(msg, Msg::Noop));

    let msg = Msg::from(RuntimeEvent::Signal(
        crate::player::lifetime::SignalEvent::Quit,
    ));
    assert!(matches!(msg, Msg::Quit));

    let msg = Msg::from(RuntimeEvent::Update(crate::update::UpdateEvent::Checked(
        update_status(),
    )));
    assert!(matches!(
        msg,
        Msg::UpdateChecked(status) if status.latest == "v1.0.1" && status.available
    ));

    let msg = Msg::from(RuntimeEvent::Transfer(
        crate::transfer::actor::TransferEvent::Progress(transfer_progress("job-2")),
    ));
    assert!(matches!(
        msg,
        Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(progress))
            if progress.job_id == "job-2"
    ));
}

#[test]
fn runtime_event_to_msg_preserves_ai_payload_variants() {
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Thinking(true))),
        Msg::Ai(AiMsg::Thinking(true))
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Error(
            "no key".to_owned()
        ))),
        Msg::Ai(AiMsg::Error(error)) if error == "no key"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::PlayTracks(vec![song(
            "play1234567"
        )]))),
        Msg::Ai(AiMsg::PlayTracks(songs)) if songs[0].video_id == "play1234567"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Enqueue(vec![song(
            "queue123456"
        )]))),
        Msg::Ai(AiMsg::Enqueue(songs)) if songs[0].video_id == "queue123456"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Suggestions(vec![
            song("suggest1234")
        ]))),
        Msg::Ai(AiMsg::Suggestions(songs)) if songs[0].video_id == "suggest1234"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::SetAutoplay(false))),
        Msg::Ai(AiMsg::SetAutoplay(false))
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::CreatePlaylist(
            "Road".to_owned()
        ))),
        Msg::Ai(AiMsg::CreatePlaylist(name)) if name == "Road"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::AddToPlaylist {
            playlist: "Road".to_owned(),
            songs: vec![song("add12345678")],
        })),
        Msg::Ai(AiMsg::AddToPlaylist { playlist, songs })
            if playlist == "Road" && songs[0].video_id == "add12345678"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::PlayPlaylist(
            "Favorites".to_owned()
        ))),
        Msg::Ai(AiMsg::PlayPlaylist(key)) if key == "Favorites"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::StationPatch {
            down_artists: vec!["Skip".to_owned()],
            boost_artists: vec!["Boost".to_owned()],
        })),
        Msg::Ai(AiMsg::StationPatch {
            down_artists,
            boost_artists,
        }) if down_artists == ["Skip"] && boost_artists == ["Boost"]
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::RomanizedTitles {
            request_id: 77,
            keys: vec!["k1".to_owned()],
            entries: vec![crate::romanize::RomanizedResult {
                key: "k1".to_owned(),
                title: "Title".to_owned(),
                artist: "Artist".to_owned(),
                confidence: Some(0.9),
            }],
        })),
        Msg::Ai(AiMsg::RomanizedTitles {
            request_id: 77,
            keys,
            entries,
        }) if keys == ["k1"] && entries[0].title == "Title"
    ));
}

#[test]
fn cache_safety_terminal_reconciles_against_latest_admitted_generation() {
    let mut current = crate::player::PlayerEvent::CacheEmergency {
        file_generation: 7,
        position_secs: 3_600.25,
        paused: true,
        reason: crate::player::long_form_seek::CacheReason::DisableFailed,
    };
    super::player_delivery::reconcile_cache_safety_event(&mut current, Some(7));
    assert!(matches!(
        current,
        crate::player::PlayerEvent::CacheEmergency { .. }
    ));

    super::player_delivery::reconcile_cache_safety_event(&mut current, Some(8));
    assert!(matches!(
        current,
        crate::player::PlayerEvent::CacheReplacementEmergency {
            reason: crate::player::long_form_seek::CacheReason::DisableFailed,
        }
    ));
}

#[test]
fn runtime_event_to_msg_preserves_api_player_and_service_payloads() {
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::ModeResolved {
            mode: crate::api::ApiMode::Authenticated,
            had_cookie: true,
        })),
        Msg::ApiModeResolved {
            mode: crate::api::ApiMode::Authenticated,
            had_cookie: true,
        }
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::TrackResolved {
            seq: 12,
            result: Err("not found".to_owned()),
        })),
        Msg::TrackResolved {
            seq: 12,
            result: Err(error),
        } if error == "not found"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::PlaylistTracks {
            title: "Mix".to_owned(),
            intent: crate::api::PlaylistIntent::Enqueue,
            songs: vec![song("plist123456")],
        })),
        Msg::PlaylistTracks {
            title,
            intent: crate::api::PlaylistIntent::Enqueue,
            songs,
        } if title == "Mix" && songs[0].video_id == "plist123456"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::StreamingResults {
            seed_video_id: "seed".to_owned(),
            candidates: vec![(song("cand1234567"), crate::streaming::CandidateSource::YtdlpStreaming)],
        })),
        Msg::Streaming(StreamingMsg::Results {
            seed_video_id,
            candidates,
        }) if seed_video_id == "seed"
            && candidates[0].0.video_id == "cand1234567"
            && candidates[0].1 == crate::streaming::CandidateSource::YtdlpStreaming
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::StreamingPreflighted {
            seed_video_id: "seed".to_owned(),
            songs: vec![song("pre12345678")],
        })),
        Msg::Streaming(StreamingMsg::Preflighted { seed_video_id, songs })
            if seed_video_id == "seed" && songs[0].video_id == "pre12345678"
    ));

    for (event, assert_msg) in [
        (crate::player::PlayerEvent::Duration(Some(88.0)), "duration"),
        (crate::player::PlayerEvent::Paused(true), "paused"),
        (crate::player::PlayerEvent::Volume(42.0), "volume"),
        (crate::player::PlayerEvent::CacheTime(Some(91.0)), "cache"),
        (
            crate::player::PlayerEvent::AudioCodec(Some("aac".to_owned())),
            "codec",
        ),
        (crate::player::PlayerEvent::Eof, "eof"),
        (
            crate::player::PlayerEvent::Error("decode".to_owned()),
            "error",
        ),
        (
            crate::player::PlayerEvent::TransportClosed("broken pipe".to_owned()),
            "transport_closed",
        ),
    ] {
        let msg = Msg::from(RuntimeEvent::Player(event));
        match assert_msg {
            "duration" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::Duration(Some(d))) if (d - 88.0).abs() < f64::EPSILON
            )),
            "paused" => assert!(matches!(msg, Msg::Player(PlayerMsg::Paused(true)))),
            "volume" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::Volume(v)) if (v - 42.0).abs() < f64::EPSILON
            )),
            "cache" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::CacheTime(Some(t))) if (t - 91.0).abs() < f64::EPSILON
            )),
            "codec" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::AudioCodec(Some(codec))) if codec == "aac"
            )),
            "eof" => assert!(matches!(msg, Msg::Player(PlayerMsg::Eof))),
            "error" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::Error(error)) if error == "decode"
            )),
            "transport_closed" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::TransportClosed(reason)) if reason == "broken pipe"
            )),
            _ => unreachable!(),
        }
    }

    assert!(matches!(
        Msg::from(RuntimeEvent::Scrobble(
            crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 4 },
        )),
        Msg::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 4 })
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Tools(crate::tools::ToolsEvent::Installed {
            version: "2026.07.07".to_owned(),
        })),
        Msg::Tools(crate::tools::ToolsEvent::Installed { version })
            if version == "2026.07.07"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Transfer(
            crate::transfer::actor::TransferEvent::AuthDone {
                display_name: "Tester".to_owned(),
            },
        )),
        Msg::Transfer(crate::transfer::actor::TransferEvent::AuthDone { display_name })
            if display_name == "Tester"
    ));
}

#[tokio::test]
async fn must_deliver_runtime_signal_waits_when_owner_lane_is_full() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );

    assert_eq!(
        emit(
            &tx,
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit)
        ),
        Ok(DeliveryReceipt::Deferred)
    );
    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await,
        Ok(Some(RuntimeEvent::Signal(
            crate::player::lifetime::SignalEvent::Quit
        )))
    ));
    assert!(tx.drain_coalesced().is_empty());
}

#[test]
fn native_callback_backpressure_preserves_the_exact_media_command_until_owner_admission() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::with_deferred_capacity(raw_tx.clone(), 0);
    raw_tx
        .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
            1.0,
        )))
        .unwrap();

    let callback_tx = tx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let callback = std::thread::spawn(move || {
        let result = emit_callback_result(
            &callback_tx,
            RuntimeEvent::App(Msg::Media(crate::media::MediaCommand::Next)),
        );
        done_tx.send(result).unwrap();
    });

    assert_eq!(
        done_rx.recv_timeout(std::time::Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "the callback must retain its command while every bounded owner lane is saturated"
    );
    assert!(matches!(
        rx.blocking_recv(),
        Some(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("callback should complete after owner capacity is released")
            .is_ok()
    );
    assert!(matches!(
        rx.blocking_recv(),
        Some(RuntimeEvent::App(Msg::Media(
            crate::media::MediaCommand::Next
        )))
    ));
    callback.join().unwrap();
}

#[test]
fn retiring_media_generation_releases_callback_without_closing_runtime_owner() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::with_deferred_capacity(raw_tx.clone(), 0);
    raw_tx
        .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
            1.0,
        )))
        .unwrap();
    let cancellation = crate::util::delivery::CallbackCancellation::new();
    let callback_cancellation = cancellation.clone();
    let callback_tx = tx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let callback = std::thread::spawn(move || {
        done_tx
            .send(ingress::emit_callback_result_until(
                &callback_tx,
                RuntimeEvent::App(Msg::Media(crate::media::MediaCommand::Next)),
                &callback_cancellation,
            ))
            .unwrap();
    });

    assert_eq!(
        done_rx.recv_timeout(std::time::Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );
    cancellation.cancel();
    assert_eq!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("retiring the media generation should release its callback"),
        Err(crate::util::delivery::DeliveryError::Closed)
    );
    callback.join().unwrap();

    assert!(matches!(
        rx.try_recv(),
        Ok(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(!rx.is_closed());
}

#[test]
fn remote_runtime_event_reports_full_to_callers() {
    use crate::util::delivery::DeliveryError;

    let (raw_tx, _rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();

    assert_eq!(
        emit(
            &tx,
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(
                RemoteCommand::TogglePause,
                reply.into(),
            ))
        ),
        Err(DeliveryError::Busy)
    );
}

#[test]
fn rejected_resolver_admission_reduces_failure_without_owner_requeue() {
    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = App::new(100);
        app.queue.set(vec![song("id0"), song("id1")], 0);
        let heal = app.update(PlayerMsg::Error(
            "mpv could not play this track (unrecognized file format)".to_owned(),
        ));
        assert!(heal.iter().any(|cmd| matches!(
            cmd,
            Cmd::YtdlpSelfHeal { video_id, .. } if video_id == "id0"
        )));
        let resolve = app.update(Msg::YtdlpHealResult {
            video_id: "id0".to_owned(),
            updated: true,
        });
        assert!(resolve.iter().any(|cmd| matches!(
            cmd,
            Cmd::ResolveForSelfHeal { video_id, .. } if video_id == "id0"
        )));

        let mut follow_ups = settle_resolver_admission(&mut app, "id0".to_owned(), Err(error));
        assert!(
            follow_ups
                .iter()
                .flat_map(Cmd::player_commands)
                .any(|command| { matches!(command, PlayerCmd::Load(url) if url.contains("id1")) })
        );
        assert_eq!(
            app.queue.current().map(|song| song.video_id.as_str()),
            Some("id0"),
            "skip stays speculative until the player batch is admitted"
        );

        let intent_index = follow_ups
            .iter()
            .position(|cmd| matches!(cmd, Cmd::PlayerControl(PlayerControl::Intent(_))))
            .expect("resolver rejection must produce a typed skip intent");
        let Cmd::PlayerControl(PlayerControl::Intent(intent)) =
            follow_ups.swap_remove(intent_index)
        else {
            unreachable!("matched the player intent above")
        };
        let _ = settle_player_intent(&mut app, *intent, Ok(DeliveryReceipt::Enqueued));
        assert_eq!(
            app.queue.current().map(|song| song.video_id.as_str()),
            Some("id1")
        );
        assert!(
            app.update(Msg::ResolveFailed {
                video_id: "id0".to_owned(),
            })
            .is_empty()
        );
    }
}

#[test]
fn closed_persistence_admission_is_visible_to_the_owner() {
    let mut app = App::new(100);

    assert!(!report_actor_delivery(
        &mut app,
        "persistence",
        Err(DeliveryError::Closed),
    ));
    assert_eq!(app.status.kind, crate::app::StatusKind::Error);
    assert!(app.status.text.contains("closed"));
}
