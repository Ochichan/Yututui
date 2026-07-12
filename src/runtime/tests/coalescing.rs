use super::*;

#[tokio::test(flavor = "current_thread")]
async fn stale_runtime_results_coalesce_by_request_without_losing_a_newer_request() {
    use crate::util::delivery::DeliveryReceipt;

    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0,
            )))
            .is_ok()
    );

    let search_error = |request_id, error: &str| {
        RuntimeEvent::Api(crate::api::ApiEvent::SearchError {
            request_id,
            source: crate::search_source::SearchSource::Youtube,
            error: error.to_owned(),
        })
    };
    assert_eq!(
        emit(&tx, search_error(1, "old")),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: false,
            evicted_oldest: false,
        })
    );
    assert!(matches!(
        emit(&tx, search_error(1, "new")),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            ..
        })
    ));
    assert!(emit(&tx, search_error(2, "current")).is_ok());

    assert!(matches!(rx.recv().await, Some(RuntimeEvent::Player(_))));
    assert!(matches!(rx.recv().await, Some(RuntimeEvent::TelemetryWake)));
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 2);
    assert!(drained.iter().any(|event| matches!(
        event,
        RuntimeEvent::Api(crate::api::ApiEvent::SearchError {
            request_id: 1,
            error,
            ..
        }) if error == "new"
    )));
    assert!(drained.iter().any(|event| matches!(
        event,
        RuntimeEvent::Api(crate::api::ApiEvent::SearchError {
            request_id: 2,
            error,
            ..
        }) if error == "current"
    )));
}

#[test]
fn runtime_telemetry_coalesces_time_pos_to_one_wake() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);

    for tick in 0..10_000 {
        assert!(
            emit(
                &tx,
                RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(tick as f64)),
            )
            .is_ok()
        );
    }

    assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
    assert!(rx.try_recv().is_err());
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 1);
    assert!(matches!(
        &drained[0],
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(t)) if (*t - 9999.0).abs() < f64::EPSILON
    ));
}

#[test]
fn runtime_app_scan_progress_coalesces_to_latest_snapshot() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);

    for seen in 0..10_000 {
        assert!(
            emit(
                &tx,
                RuntimeEvent::App(Msg::Local(crate::app::LocalMsg::ScanProgress(
                    crate::local::LocalScanProgress {
                        seen,
                        indexed: seen / 2,
                        ..crate::local::LocalScanProgress::default()
                    },
                ))),
            )
            .is_ok()
        );
    }

    assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
    assert!(rx.try_recv().is_err());
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 1);
    assert!(matches!(
        &drained[0],
        RuntimeEvent::App(Msg::Local(crate::app::LocalMsg::ScanProgress(progress)))
            if progress.seen == 9_999 && progress.indexed == 4_999
    ));
}

#[test]
fn runtime_download_progress_coalesces_but_terminal_is_must_deliver() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(4);
    let tx = RuntimeSender::new(raw_tx);

    assert!(
        emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "a".to_owned(),
                percent: 10.0,
            }),
        )
        .is_ok()
    );
    assert!(
        emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "a".to_owned(),
                percent: 70.0,
            }),
        )
        .is_ok()
    );
    assert!(
        emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "b".to_owned(),
                percent: 40.0,
            }),
        )
        .is_ok()
    );
    assert!(
        emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Done {
                video_id: "a".to_owned(),
                path: "a.m4a".to_owned(),
            }),
        )
        .is_ok()
    );

    assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
    assert!(matches!(
        rx.try_recv(),
        Ok(RuntimeEvent::Download(
            crate::download::DownloadEvent::Done { video_id, path }
        )) if video_id == "a" && path == "a.m4a"
    ));
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 2);
    assert!(drained.iter().any(|event| matches!(
        event,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id,
            percent,
        }) if video_id == "a" && (*percent - 70.0).abs() < f64::EPSILON
    )));
    assert!(drained.iter().any(|event| matches!(
        event,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id,
            percent,
        }) if video_id == "b" && (*percent - 40.0).abs() < f64::EPSILON
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_work_result_survives_a_full_telemetry_buffer() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);

    assert!(
        emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Done {
                video_id: "finished".to_owned(),
                path: "finished.m4a".to_owned(),
            }),
        )
        .is_ok()
    );
    for index in 0..256 {
        assert!(
            emit(
                &tx,
                RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                    video_id: format!("progress-{index}"),
                    percent: index as f64,
                }),
            )
            .is_ok()
        );
    }

    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Download(
            crate::download::DownloadEvent::Done { video_id, path }
        )) if video_id == "finished" && path == "finished.m4a"
    ));
    assert!(matches!(rx.recv().await, Some(RuntimeEvent::TelemetryWake)));
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 256);
}

#[tokio::test(flavor = "current_thread")]
async fn scrobble_auth_and_cumulative_loss_notices_survive_telemetry_saturation() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);

    for event in [
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthUrl(
            "https://example.invalid/auth".to_owned(),
        )),
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthDone {
            username: "listener".to_owned(),
            session_key: "secret".to_owned(),
        }),
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 1 }),
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 9 }),
    ] {
        assert!(emit(&tx, event).is_ok());
    }
    for index in 0..256 {
        assert!(
            emit(
                &tx,
                RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                    video_id: format!("telemetry-{index}"),
                    percent: index as f64,
                }),
            )
            .is_ok()
        );
    }

    let mut controls = Vec::new();
    for _ in 0..4 {
        controls.push(rx.recv().await.expect("scrobble control event was lost"));
    }
    assert!(matches!(rx.recv().await, Some(RuntimeEvent::TelemetryWake)));
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 256);
    assert!(controls.iter().any(|event| matches!(
        event,
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthUrl(url))
            if url == "https://example.invalid/auth"
    )));
    assert!(controls.iter().any(|event| matches!(
        event,
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthDone {
            username,
            session_key,
        }) if username == "listener" && session_key == "secret"
    )));
    assert!(controls.iter().any(|event| matches!(
        event,
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 1 })
    )));
    assert!(controls.iter().any(|event| matches!(
        event,
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 9 })
    )));
}

#[tokio::test(flavor = "current_thread")]
async fn scrobble_session_invalidation_is_must_deliver_per_service() {
    use crate::scrobble::service::ServiceKind;

    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);
    assert!(
        emit(
            &tx,
            RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::SessionInvalid(
                ServiceKind::Lastfm,
            )),
        )
        .is_ok()
    );
    assert!(
        emit(
            &tx,
            RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::SessionInvalid(
                ServiceKind::ListenBrainz,
            )),
        )
        .is_ok()
    );

    let first = rx.recv().await.expect("first invalidation was lost");
    let second = rx.recv().await.expect("second invalidation was lost");
    let delivered = [first, second];
    assert!(delivered.iter().any(|event| matches!(
        event,
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::SessionInvalid(
            ServiceKind::Lastfm
        ))
    )));
    assert!(delivered.iter().any(|event| matches!(
        event,
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::SessionInvalid(
            ServiceKind::ListenBrainz
        ))
    )));
    assert!(tx.drain_coalesced().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn video_terminal_events_are_delivered_in_order_without_coalescing() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);

    for event in [
        RuntimeEvent::Video {
            generation: 4,
            event: crate::player::video::VideoEvent::Failed("old".to_owned()),
        },
        RuntimeEvent::Video {
            generation: 4,
            event: crate::player::video::VideoEvent::Closed,
        },
        RuntimeEvent::Video {
            generation: 4,
            event: crate::player::video::VideoEvent::Eof,
        },
        RuntimeEvent::Video {
            generation: 5,
            event: crate::player::video::VideoEvent::Failed("new".to_owned()),
        },
    ] {
        assert!(emit(&tx, event).is_ok());
    }

    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Video {
            generation: 4,
            event: crate::player::video::VideoEvent::Failed(error),
        }) if error == "old"
    ));
    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Video {
            generation: 4,
            event: crate::player::video::VideoEvent::Closed,
        })
    ));
    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Video {
            generation: 4,
            event: crate::player::video::VideoEvent::Eof,
        })
    ));
    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Video {
            generation: 5,
            event: crate::player::video::VideoEvent::Failed(error),
        }) if error == "new"
    ));
    assert!(tx.drain_coalesced().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn self_heal_terminal_result_waits_when_owner_lane_is_full() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx.clone());
    raw_tx
        .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
            1.0,
        )))
        .expect("fill owner lane");

    assert_eq!(
        emit(
            &tx,
            RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
                video_id: crate::ids::VideoId::from("heal"),
                purpose: crate::resolver::ResolvePurpose::SelfHeal,
            }),
        ),
        Ok(crate::util::delivery::DeliveryReceipt::Deferred)
    );

    assert!(matches!(rx.recv().await, Some(RuntimeEvent::Player(_))));
    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Resolver(
            crate::resolver::ResolverEvent::Failed { video_id, purpose }
        )) if video_id.as_str() == "heal" && purpose == crate::resolver::ResolvePurpose::SelfHeal
    ));
    assert!(tx.drain_coalesced().is_empty());
}
