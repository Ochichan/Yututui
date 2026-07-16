use super::*;

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

#[test]
fn daemon_logging_uses_the_writer_leased_cache_override() {
    let cache = std::env::temp_dir().join(format!("yututui-daemon-cache-{}", std::process::id()));
    let cache_env = cache.to_string_lossy().into_owned();
    crate::test_util::env::with_var("YTM_CACHE_DIR", Some(cache_env.as_str()), || {
        assert_eq!(daemon_log_dir(), Some(cache.join("logs")));
    });
}

#[test]
fn parses_start_and_resume() {
    assert_eq!(
        parse(&owned(&["start", "--resume"])),
        Ok(DaemonCommand::Start { resume: true })
    );
    assert_eq!(
        parse(&owned(&["start"])),
        Ok(DaemonCommand::Start { resume: false })
    );
}

#[test]
fn parses_serve_status_and_stop() {
    assert_eq!(
        parse(&owned(&["serve", "--from-tray", "--resume"])),
        Ok(DaemonCommand::Serve {
            from_tray: true,
            resume: true
        })
    );
    assert_eq!(
        parse(&owned(&["status", "--json"])),
        Ok(DaemonCommand::Status { json: true })
    );
    assert_eq!(parse(&owned(&["stop"])), Ok(DaemonCommand::Stop));
}

#[test]
fn parse_rejects_unknown_flags_and_reports_usage_requests() {
    assert_eq!(parse(&owned(&[])), Err(ParseOutcome::Usage));
    assert_eq!(parse(&owned(&["--help"])), Err(ParseOutcome::Usage));
    assert_eq!(
        parse(&owned(&["start", "--help"])),
        Err(ParseOutcome::Usage)
    );
    assert_eq!(
        parse(&owned(&["serve", "--help"])),
        Err(ParseOutcome::Usage)
    );
    assert_eq!(
        parse(&owned(&["status", "--help"])),
        Err(ParseOutcome::Usage)
    );
    assert!(matches!(
        parse(&owned(&["start", "--bad"])),
        Err(ParseOutcome::Invalid(message)) if message == "start: unknown flag `--bad`"
    ));
    assert!(matches!(
        parse(&owned(&["serve", "--bad"])),
        Err(ParseOutcome::Invalid(message)) if message == "serve: unknown flag `--bad`"
    ));
    assert!(matches!(
        parse(&owned(&["status", "--bad"])),
        Err(ParseOutcome::Invalid(message)) if message == "status: unknown flag `--bad`"
    ));
    assert!(matches!(
        parse(&owned(&["stop", "--bad"])),
        Err(ParseOutcome::Invalid(message)) if message == "stop: unexpected arguments"
    ));
    assert!(matches!(
        parse(&owned(&["bogus"])),
        Err(ParseOutcome::Invalid(message)) if message.contains("unknown command `bogus`")
    ));
}

#[test]
fn daemon_error_exit_codes_match_user_actionability() {
    assert_eq!(
        daemon_error_exit_code(&DaemonError::StandaloneOwner),
        EXIT_USAGE
    );
    assert_eq!(
        daemon_error_exit_code(&DaemonError::ResumeRejected("session_empty".to_owned())),
        EXIT_USAGE
    );
    assert_eq!(
        daemon_error_exit_code(&DaemonError::StopRejected("busy".to_owned())),
        EXIT_USAGE
    );
    assert_eq!(
        daemon_error_exit_code(&DaemonError::Transport("socket closed".to_owned())),
        EXIT_TRANSPORT
    );
    assert_eq!(
        daemon_error_exit_code(&DaemonError::Spawn("denied".to_owned())),
        EXIT_TRANSPORT
    );
}

#[test]
fn daemon_capabilities_advertise_headless_playback() {
    assert!(daemon_capabilities().contains(&"headless-playback".to_string()));
    assert!(daemon_capabilities().contains(&"queue-control".to_string()));
    assert!(daemon_capabilities().contains(&RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string()));
    assert!(daemon_capabilities().contains(&PERSONAL_EXPORT_CAPABILITY.to_string()));
    assert!(daemon_capabilities().contains(&LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY.to_string()));
}

#[test]
fn daemon_event_policy_covers_representative_events() {
    use crate::util::event_policy::{EventKey, EventLane, EventPolicy};

    assert_eq!(
        DaemonEvent::Signal.policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::Error("x".to_owned())).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::Volume(42.0)).policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerVolume
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::Duration(Some(180.0))).policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerDuration
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::Paused(true)).policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerPaused
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::Metadata(serde_json::json!({
            "title": "Track"
        })))
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::WorkResult,
            key: EventKey::PlayerMetadata
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::CacheTime(None)).policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerCacheTime
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::AudioCodec(Some(
            "aac".to_owned()
        )))
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerAudioCodec
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::FileFormat(Some(
            "mp4".to_owned()
        )))
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerFileFormat
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::Eof).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::TransportClosed(
            "EOF".to_owned()
        ))
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::ModeResolved {
            mode: crate::api::ApiMode::Anonymous,
            had_cookie: false,
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::SearchResults {
            request_id: 1,
            query: "q".to_owned(),
            source: crate::search_source::SearchSource::Youtube,
            songs: Vec::new(),
            timed_out: false,
        })
        .policy(),
        EventPolicy::DropIfStale {
            stale_key: EventKey::SearchRequest
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::SearchError {
            request_id: 1,
            source: crate::search_source::SearchSource::Youtube,
            error: "bad".to_owned(),
        })
        .policy(),
        EventPolicy::DropIfStale {
            stale_key: EventKey::SearchRequest
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::StreamingResults {
            seed_video_id: "seed".to_owned(),
            candidates: Vec::new(),
        })
        .policy(),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::StreamingPreflighted {
            seed_video_id: "seed".to_owned(),
            songs: Vec::new(),
        })
        .policy(),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
            seed_video_id: "seed".to_owned(),
            error: "bad".to_owned(),
        })
        .policy(),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::TrackResolved {
            seq: 7,
            result: Ok(Vec::new()),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::PlaylistTracks {
            title: "Mix".to_owned(),
            intent: crate::api::PlaylistIntent::Import,
            songs: Vec::new(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::PlaylistTracksError {
            title: "Mix".to_owned(),
            error: "bad".to_owned(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult
        }
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::GuiSearchCompleted {
            request_id: crate::api::GuiSearchRequestId::new(0, 7),
            groups: Vec::new(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult
        }
    );
    assert_eq!(
        DaemonEvent::Media(crate::media::MediaCommand::Next).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control
        }
    );
    assert_eq!(
        DaemonEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 1 }).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        DaemonEvent::MediaArt(crate::media::artwork::MediaArtworkReady {
            key: "track".to_owned(),
            path: "art.jpg".into(),
        })
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::MediaArtVideo
        }
    );
    assert_eq!(
        DaemonEvent::Download(crate::download::DownloadEvent::Progress {
            video_id: "track".to_owned(),
            percent: 40.0,
        })
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::DownloadProgress,
        }
    );
    assert_eq!(
        DaemonEvent::Download(crate::download::DownloadEvent::Done {
            video_id: "track".to_owned(),
            path: "track.m4a".to_owned(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        DaemonEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
            crate::transfer::TransferProgress {
                job_id: "job".to_owned(),
                stage: crate::transfer::Stage::Matching,
                done: 1,
                total: 2,
                matched: 1,
                auto_accepted: 0,
                ambiguous: 0,
                not_found: 0,
                written: 0,
                current: "Track".to_owned(),
            }
        ))
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::TransferJob,
        }
    );
    assert_eq!(
        DaemonEvent::Transfer(crate::transfer::actor::TransferEvent::JobFailed {
            job_id: "job".to_owned(),
            error: "failed".to_owned(),
            resumable: true,
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        DaemonEvent::Ai(crate::ai::AiEvent::Thinking(true)).policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::AiThinking,
        }
    );
    assert_eq!(
        DaemonEvent::Ai(crate::ai::AiEvent::Chat("hello".to_owned())).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    // Unlike the interactive owner, the daemon has no reducer-side stale slot for AI picks.
    assert_eq!(
        DaemonEvent::Ai(crate::ai::AiEvent::StreamingPicks {
            seed_video_id: "seed".to_owned(),
            picks: Vec::new(),
            conf: None,
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    // The daemon lyrics host already gates work on the current subscriber/track generation.
    assert_eq!(
        DaemonEvent::Lyrics(crate::lyrics::LyricsEvent::Result {
            video_id: "track".to_owned(),
            lines: Vec::new().into(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        DaemonEvent::YtdlpHeal {
            video_id: "v".to_owned(),
            updated: true,
        }
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        DaemonEvent::TransportRecoveryRetry { generation: 7 }.policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        DaemonEvent::PersonalExportFinished(personal_export::Finished {
            generation: 7,
            result: Ok(std::path::PathBuf::from("export.json")),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        DaemonEvent::TelemetryWake.policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control
        }
    );
}

#[test]
fn daemon_event_kind_and_telemetry_slots_are_stable() {
    use crate::util::event_policy::EventKey;

    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    assert_eq!(
        DaemonEvent::Remote(RemoteEvent::Command(RemoteCommand::Status, reply.into())).kind(),
        "remote"
    );
    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).kind(),
        "player"
    );
    assert_eq!(
        DaemonEvent::Api(crate::api::ApiEvent::SearchError {
            request_id: 1,
            source: crate::search_source::SearchSource::Youtube,
            error: "bad".to_owned(),
        })
        .kind(),
        "api"
    );
    assert_eq!(
        DaemonEvent::Media(crate::media::MediaCommand::Pause).kind(),
        "media"
    );
    assert_eq!(
        DaemonEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueStalled { pending: 3 }).kind(),
        "scrobble"
    );
    assert_eq!(
        DaemonEvent::Download(crate::download::DownloadEvent::Error {
            video_id: "v".to_owned(),
            error: "failed".to_owned(),
        })
        .kind(),
        "download"
    );
    assert_eq!(
        DaemonEvent::Transfer(crate::transfer::actor::TransferEvent::AuthError(
            "failed".to_owned()
        ))
        .kind(),
        "transfer"
    );
    assert_eq!(
        DaemonEvent::Ai(crate::ai::AiEvent::Chat("hello".to_owned())).kind(),
        "ai"
    );
    assert_eq!(
        DaemonEvent::YtdlpHeal {
            video_id: "v".to_owned(),
            updated: false,
        }
        .kind(),
        "ytdlp_heal"
    );
    assert_eq!(
        DaemonEvent::TransportRecoveryRetry { generation: 7 }.kind(),
        "transport_recovery_retry"
    );
    assert_eq!(
        DaemonEvent::PersonalExportFinished(personal_export::Finished {
            generation: 7,
            result: Ok(std::path::PathBuf::from("export.json")),
        })
        .kind(),
        "personal_export_finished"
    );
    assert_eq!(DaemonEvent::Signal.kind(), "signal");
    assert_eq!(DaemonEvent::TelemetryWake.kind(), "telemetry_wake");
    assert!(DaemonEvent::TelemetryWake.is_telemetry_wake());
    assert!(!DaemonEvent::Signal.is_telemetry_wake());

    assert_eq!(
        DaemonEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).telemetry_slot(),
        Some(DaemonTelemetrySlot::Static(EventKey::PlayerTimePos))
    );
    assert_eq!(
        DaemonEvent::MediaArt(crate::media::artwork::MediaArtworkReady {
            key: "track-a".to_owned(),
            path: "art.jpg".into(),
        })
        .telemetry_slot(),
        Some(DaemonTelemetrySlot::MediaArt("track-a".to_owned()))
    );
    assert_eq!(
        DaemonEvent::Download(crate::download::DownloadEvent::Progress {
            video_id: "track-a".to_owned(),
            percent: 50.0,
        })
        .telemetry_slot(),
        Some(DaemonTelemetrySlot::DownloadProgress("track-a".to_owned()))
    );
    assert_eq!(
        DaemonEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
            crate::transfer::TransferProgress {
                job_id: "job".to_owned(),
                stage: crate::transfer::Stage::Matching,
                done: 1,
                total: 2,
                matched: 1,
                auto_accepted: 0,
                ambiguous: 0,
                not_found: 0,
                written: 0,
                current: "Track".to_owned(),
            }
        ))
        .telemetry_slot(),
        Some(DaemonTelemetrySlot::Static(EventKey::TransferJob))
    );
    assert_eq!(
        DaemonEvent::Ai(crate::ai::AiEvent::Thinking(true)).telemetry_slot(),
        Some(DaemonTelemetrySlot::Static(EventKey::AiThinking))
    );
    assert_eq!(DaemonEvent::Signal.telemetry_slot(), None);
    assert_eq!(
        DaemonEvent::TransportRecoveryRetry { generation: 7 }.telemetry_slot(),
        None
    );
    assert_eq!(
        DaemonEvent::Scrobble(crate::scrobble::ScrobbleEvent::SessionInvalid(
            crate::scrobble::service::ServiceKind::ListenBrainz,
        ))
        .telemetry_slot(),
        None
    );
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_scrobble_health_and_stale_results_coalesce_when_owner_is_full() {
    use crate::util::delivery::DeliveryReceipt;

    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );

    assert!(
        emit_daemon_event(
            &tx,
            DaemonEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueStalled { pending: 10 })
        )
        .is_ok()
    );
    assert_eq!(
        emit_daemon_event(
            &tx,
            DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
                seed_video_id: "seed".to_owned(),
                error: "old".to_owned(),
            })
        ),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: false,
            evicted_oldest: false,
        })
    );
    assert!(matches!(
        emit_daemon_event(
            &tx,
            DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
                seed_video_id: "seed".to_owned(),
                error: "new".to_owned(),
            })
        ),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            ..
        })
    ));
    assert!(
        emit_daemon_event(
            &tx,
            DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
                seed_video_id: "other".to_owned(),
                error: "other".to_owned(),
            })
        )
        .is_ok()
    );

    assert!(matches!(rx.recv().await, Some(DaemonEvent::Player(_))));
    assert!(matches!(rx.recv().await, Some(DaemonEvent::TelemetryWake)));
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 3);
    assert!(drained.iter().any(|event| matches!(
        event,
        DaemonEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueStalled { pending: 10 })
    )));
    assert!(drained.iter().any(|event| matches!(
        event,
        DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
            seed_video_id,
            error,
        }) if seed_video_id == "seed" && error == "new"
    )));
}

#[test]
fn daemon_emit_reports_closed_after_receiver_closes() {
    use crate::util::delivery::DeliveryError;

    let (raw_tx, rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::new(raw_tx);
    drop(rx);

    assert_eq!(
        emit_daemon_event(&tx, DaemonEvent::Media(crate::media::MediaCommand::Stop)),
        Err(DeliveryError::Closed)
    );
}

#[test]
fn native_callback_backpressure_preserves_the_exact_daemon_media_command() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::with_deferred_capacity(raw_tx.clone(), 0);
    raw_tx
        .try_send(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(
            1.0,
        )))
        .unwrap();

    let callback_tx = tx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let callback = std::thread::spawn(move || {
        let result = emit_daemon_callback_result(
            &callback_tx,
            DaemonEvent::Media(crate::media::MediaCommand::Previous),
        );
        done_tx.send(result).unwrap();
    });

    assert!(matches!(
        done_rx.recv_timeout(std::time::Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(matches!(
        rx.blocking_recv(),
        Some(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("callback should complete after daemon owner capacity is released")
            .is_ok()
    );
    assert!(matches!(
        rx.blocking_recv(),
        Some(DaemonEvent::Media(crate::media::MediaCommand::Previous))
    ));
    callback.join().unwrap();
}

#[test]
fn retiring_media_generation_releases_callback_without_closing_daemon_owner() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::with_deferred_capacity(raw_tx.clone(), 0);
    raw_tx
        .try_send(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(
            1.0,
        )))
        .unwrap();
    let cancellation = crate::util::delivery::CallbackCancellation::new();
    let callback_cancellation = cancellation.clone();
    let callback_tx = tx.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let callback = std::thread::spawn(move || {
        done_tx
            .send(emit_daemon_callback_result_until(
                &callback_tx,
                DaemonEvent::Media(crate::media::MediaCommand::Previous),
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
        Ok(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(!rx.is_closed());
}

#[test]
fn log_scrobble_event_accepts_all_notice_shapes() {
    use crate::scrobble::ScrobbleEvent;
    use crate::scrobble::service::ServiceKind;

    log_scrobble_event(ScrobbleEvent::SessionInvalid(ServiceKind::Lastfm));
    log_scrobble_event(ScrobbleEvent::QueueStalled { pending: 4 });
    log_scrobble_event(ScrobbleEvent::QueueDropped { dropped: 2 });
    log_scrobble_event(ScrobbleEvent::AuthUrl("http://localhost/auth".to_owned()));
    log_scrobble_event(ScrobbleEvent::AuthDone {
        username: "user".to_owned(),
        session_key: "secret".to_owned(),
    });
    log_scrobble_event(ScrobbleEvent::AuthFailed("bad\nsecret".to_owned()));
}

#[tokio::test]
async fn must_deliver_daemon_critical_events_wait_in_order_when_owner_lane_is_full() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );

    for event in [
        DaemonEvent::Player(crate::player::PlayerEvent::TransportClosed(
            "broken pipe".to_owned(),
        )),
        DaemonEvent::TransportRecoveryRetry { generation: 7 },
        DaemonEvent::Signal,
    ] {
        assert_eq!(
            emit_daemon_event(&tx, event),
            Ok(crate::util::delivery::DeliveryReceipt::Deferred)
        );
    }
    assert!(matches!(
        rx.recv().await,
        Some(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(matches!(
        rx.recv().await,
        Some(DaemonEvent::Player(
            crate::player::PlayerEvent::TransportClosed(reason)
        )) if reason == "broken pipe"
    ));
    assert!(matches!(
        rx.recv().await,
        Some(DaemonEvent::TransportRecoveryRetry { generation: 7 })
    ));
    assert!(matches!(rx.recv().await, Some(DaemonEvent::Signal)));
    assert!(tx.drain_coalesced().is_empty());
}

#[test]
fn remote_daemon_event_reports_full_to_callers() {
    use crate::util::delivery::DeliveryError;

    let (raw_tx, _rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(DaemonEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();

    assert_eq!(
        emit_daemon_event(
            &tx,
            DaemonEvent::Remote(RemoteEvent::Command(
                RemoteCommand::TogglePause,
                reply.into(),
            ))
        ),
        Err(DeliveryError::Busy)
    );
}

#[test]
fn daemon_telemetry_coalesces_time_pos_to_one_wake() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = DaemonEventSender::new(raw_tx);

    for tick in 0..10_000 {
        assert!(
            emit_daemon_event(
                &tx,
                DaemonEvent::Player(crate::player::PlayerEvent::TimePos(tick as f64))
            )
            .is_ok()
        );
    }

    assert!(matches!(rx.try_recv(), Ok(DaemonEvent::TelemetryWake)));
    assert!(rx.try_recv().is_err());
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 1);
    assert!(matches!(
        &drained[0],
        DaemonEvent::Player(crate::player::PlayerEvent::TimePos(t)) if (*t - 9999.0).abs() < f64::EPSILON
    ));
}

#[test]
fn daemon_media_art_coalesces_by_track_key() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(4);
    let tx = DaemonEventSender::new(raw_tx);

    assert!(
        emit_daemon_event(
            &tx,
            DaemonEvent::MediaArt(crate::media::artwork::MediaArtworkReady {
                key: "a".to_owned(),
                path: std::path::PathBuf::from("old.jpg"),
            })
        )
        .is_ok()
    );
    assert!(
        emit_daemon_event(
            &tx,
            DaemonEvent::MediaArt(crate::media::artwork::MediaArtworkReady {
                key: "a".to_owned(),
                path: std::path::PathBuf::from("new.jpg"),
            })
        )
        .is_ok()
    );
    assert!(
        emit_daemon_event(
            &tx,
            DaemonEvent::MediaArt(crate::media::artwork::MediaArtworkReady {
                key: "b".to_owned(),
                path: std::path::PathBuf::from("other.jpg"),
            })
        )
        .is_ok()
    );

    assert!(matches!(rx.try_recv(), Ok(DaemonEvent::TelemetryWake)));
    assert!(rx.try_recv().is_err());
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 2);
    assert!(drained.iter().any(|event| matches!(
        event,
        DaemonEvent::MediaArt(ready)
            if ready.key == "a" && ready.path == std::path::Path::new("new.jpg")
    )));
    assert!(drained.iter().any(|event| matches!(
        event,
        DaemonEvent::MediaArt(ready)
            if ready.key == "b" && ready.path == std::path::Path::new("other.jpg")
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_beats_a_slow_owner_handler_before_it_can_mutate() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let shutdown = crate::player::lifetime::ShutdownLatch::new();
    let task_shutdown = shutdown.clone();
    let mutated = Arc::new(AtomicBool::new(false));
    let task_mutated = Arc::clone(&mutated);
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        await_owner_handler(&task_shutdown, async move {
            let _ = release_rx.await;
            task_mutated.store(true, Ordering::SeqCst);
            7
        })
        .await
    });
    tokio::task::yield_now().await;

    // Make both branches ready in the same scheduler turn. The biased latch branch must win.
    shutdown.trigger();
    let _ = release_tx.send(());

    assert_eq!(task.await.expect("handler task should join"), None);
    assert!(!mutated.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn latched_shutdown_does_not_poll_an_already_ready_owner_mutation() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let shutdown = crate::player::lifetime::ShutdownLatch::new();
    shutdown.trigger();
    let mutated = AtomicBool::new(false);

    let result = await_owner_handler(&shutdown, async {
        mutated.store(true, Ordering::SeqCst);
        1
    })
    .await;

    assert_eq!(result, None);
    assert!(!mutated.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_drain_settles_pending_main_deferred_and_coalesced_events() {
    let (raw_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
    let event_tx = DaemonEventSender::with_deferred_capacity(raw_tx, 4);

    let (main_reply, main_reply_rx) = tokio::sync::oneshot::channel();
    assert_eq!(
        emit_daemon_event(
            &event_tx,
            DaemonEvent::Remote(RemoteEvent::Command(
                RemoteCommand::Status,
                main_reply.into(),
            )),
        ),
        Ok(crate::util::delivery::DeliveryReceipt::Enqueued)
    );
    assert_eq!(
        emit_daemon_event(
            &event_tx,
            DaemonEvent::Player(crate::player::PlayerEvent::TransportClosed(
                "owner exiting".to_owned(),
            )),
        ),
        Ok(crate::util::delivery::DeliveryReceipt::Deferred)
    );
    assert_eq!(
        emit_daemon_event(
            &event_tx,
            DaemonEvent::Player(crate::player::PlayerEvent::Eof),
        ),
        Ok(crate::util::delivery::DeliveryReceipt::Deferred)
    );
    assert_eq!(
        emit_daemon_event(
            &event_tx,
            DaemonEvent::Player(crate::player::PlayerEvent::Error(
                "late terminal".to_owned(),
            )),
        ),
        Ok(crate::util::delivery::DeliveryReceipt::Deferred)
    );
    assert!(
        emit_daemon_event(
            &event_tx,
            DaemonEvent::Player(crate::player::PlayerEvent::TimePos(7.0)),
        )
        .is_ok()
    );

    let (pending_reply, pending_reply_rx) = tokio::sync::oneshot::channel();
    let mut pending_events = VecDeque::from([
        DaemonEvent::Signal,
        DaemonEvent::PersonalExportFinished(personal_export::Finished {
            generation: 99,
            result: Ok(std::path::PathBuf::from("export.json")),
        }),
        DaemonEvent::Remote(RemoteEvent::SessionCommand {
            command: RemoteCommand::Status,
            origin: crate::remote::RemoteSessionScope::for_test(9, Some("page")),
            reply: pending_reply.into(),
        }),
    ]);
    assert!(event_tx.close_admission());

    let (hub, _session, _line_rx) = crate::remote::test_register(Default::default());
    let publisher = crate::remote::publish::Publisher::new(hub);
    let mut personal_export = personal_export::PersonalExport::default();
    let drain = crate::daemon::shutdown_drain::drain_daemon_shutdown_ingress(
        &event_tx,
        &mut event_rx,
        &mut pending_events,
        &publisher,
        &mut personal_export,
    )
    .await;

    assert_eq!(
        pending_reply_rx.await.unwrap().reason.as_deref(),
        Some("shutting_down"),
        "a command already behind Signal in the owner's pending FIFO must be settled"
    );
    assert_eq!(
        main_reply_rx.await.unwrap().reason.as_deref(),
        Some("shutting_down")
    );
    assert_eq!(drain.remote_requests, 2);
    assert_eq!(
        drain.terminal_events, 3,
        "capacity-one main lane must keep draining more deferred events than it can hold"
    );
    assert_eq!(drain.coalesced_events, 1);
    assert_eq!(drain.personal_export_completions, 1);
    assert_eq!(
        drain.retired_events, 2,
        "Signal plus the coalesced time tick"
    );
    assert!(pending_events.is_empty());
    assert!(event_rx.is_closed());
}
