use super::*;

fn song(id: &str) -> Song {
    Song::remote(id, format!("title-{id}"), "artist", "3:00")
}

fn fallback_request_id(effects: &[EngineEffect]) -> u64 {
    match effects {
        [EngineEffect::StreamingFallback { request_id, .. }] => *request_id,
        other => panic!("expected one streaming fallback, got {other:?}"),
    }
}

#[tokio::test]
async fn api_streaming_events_extend_clear_pending_and_trip_circuit_breaker() {
    let mut engine = tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.streaming = true;
    engine.consecutive_streaming_failures = 2;

    let request_id = fallback_request_id(&engine.force_autoplay_extend());
    engine
        .pending_streaming_request
        .as_mut()
        .expect("request is pending")
        .stage = StreamingRequestStage::Preflight;
    let effects = engine
        .handle_api_event(ApiEvent::StreamingPreflighted {
            request_id,
            seed_video_id: "seed".to_owned(),
            songs: vec![song("fresh-a"), song("fresh-b")],
        })
        .await;
    assert!(effects.is_empty());
    assert!(!engine.streaming_pending);
    assert_eq!(engine.consecutive_streaming_failures, 0);
    assert!(engine.queue.contains_video_id("fresh-a"));

    let request_id = fallback_request_id(&engine.force_autoplay_extend());
    let effects = engine
        .handle_api_event(ApiEvent::StreamingResults {
            request_id,
            seed_video_id: "not-in-queue".to_owned(),
            candidates: vec![(song("ignored"), CandidateSource::YtdlpStreaming)],
        })
        .await;
    assert!(effects.is_empty());
    assert!(
        engine.streaming_pending,
        "wrong seed must not consume the request"
    );
    assert!(!engine.queue.contains_video_id("ignored"));
    engine.cancel_pending_streaming_request();

    for idx in 0..AUTOPLAY_MAX_FAILURES {
        engine.streaming = true;
        let request_id = fallback_request_id(&engine.force_autoplay_extend());
        engine
            .handle_api_event(ApiEvent::StreamingError {
                request_id,
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

#[tokio::test]
async fn disable_reenable_same_seed_rejects_the_old_request_generation() {
    let mut engine = tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.streaming = true;

    let old_request_id = fallback_request_id(&engine.force_autoplay_extend());
    let (_, off_effects) = engine.set_streaming(ToggleState::Off);
    assert!(off_effects.is_empty());
    assert!(!engine.streaming_pending);

    let (_, on_effects) = engine.set_streaming(ToggleState::On);
    let new_request_id = fallback_request_id(&on_effects);
    assert!(new_request_id > old_request_id);

    engine
        .handle_api_event(ApiEvent::StreamingError {
            request_id: old_request_id,
            seed_video_id: "seed".to_owned(),
            error: "old generation".to_owned(),
        })
        .await;
    assert_eq!(
        engine
            .pending_streaming_request
            .as_ref()
            .map(|pending| pending.request_id),
        Some(new_request_id)
    );
    assert_eq!(engine.consecutive_streaming_failures, 0);

    engine
        .handle_api_event(ApiEvent::StreamingError {
            request_id: new_request_id,
            seed_video_id: "seed".to_owned(),
            error: "current generation".to_owned(),
        })
        .await;
    assert!(!engine.streaming_pending);
    assert_eq!(engine.consecutive_streaming_failures, 1);
}

#[tokio::test]
async fn mode_change_rejects_old_results_instead_of_using_the_new_why_gem_source() {
    use crate::remote::proto::ResponseData;

    let mut engine = tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.streaming = true;
    assert_eq!(engine.config.streaming.mode, StreamingMode::Balanced);
    let old_request_id = fallback_request_id(&engine.force_autoplay_extend());

    let (_, effects) = engine.set_setting(RemoteSettingChange::StreamingMode {
        value: StreamingMode::Discovery,
    });
    let new_request_id = fallback_request_id(&effects);
    engine
        .handle_api_event(ApiEvent::StreamingResults {
            request_id: old_request_id,
            seed_video_id: "seed".to_owned(),
            candidates: vec![(song("old-pick"), CandidateSource::YtdlpStreaming)],
        })
        .await;
    assert!(!engine.queue.contains_video_id("old-pick"));
    assert!(engine.gui_fetch_why_gem("old-pick").data.is_none());
    assert_eq!(
        engine
            .pending_streaming_request
            .as_ref()
            .map(|pending| pending.request_id),
        Some(new_request_id)
    );

    engine
        .pending_streaming_request
        .as_mut()
        .expect("new mode request remains pending")
        .stage = StreamingRequestStage::Preflight;
    engine
        .handle_api_event(ApiEvent::StreamingPreflighted {
            request_id: new_request_id,
            seed_video_id: "seed".to_owned(),
            songs: vec![song("new-pick")],
        })
        .await;
    let Some(ResponseData::WhyGem(model)) = engine.gui_fetch_why_gem("new-pick").data else {
        panic!("new request should record WhyGem provenance");
    };
    assert_eq!(model.slot, "Discovery");
}

#[tokio::test]
async fn admitted_same_seed_manual_replacement_cancels_the_old_queue_revision() {
    let mut engine = tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.streaming = true;
    let _player_rx = tests::install_accepting_player(&mut engine);
    let old_request_id = fallback_request_id(&engine.force_autoplay_extend());
    let old_queue_rev = engine.queue.rev();

    let replacement = Song::remote("seed", "replacement", "manual artist", "4:00");
    let response = engine.gui_replace_queue(vec![replacement]).await;
    assert!(response.ok);
    assert_ne!(engine.queue.rev(), old_queue_rev);
    engine.reconcile_pending_streaming_request();
    assert!(!engine.streaming_pending);

    engine
        .handle_api_event(ApiEvent::StreamingResults {
            request_id: old_request_id,
            seed_video_id: "seed".to_owned(),
            candidates: vec![(song("stale-pick"), CandidateSource::YtdlpStreaming)],
        })
        .await;
    assert!(!engine.queue.contains_video_id("stale-pick"));
    assert_eq!(
        engine.queue.current().map(|song| song.title.as_str()),
        Some("replacement")
    );
}

#[tokio::test]
async fn duplicate_pool_result_during_preflight_does_not_consume_the_generation() {
    let mut engine = tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.streaming = true;
    let request_id = fallback_request_id(&engine.force_autoplay_extend());
    engine
        .pending_streaming_request
        .as_mut()
        .expect("request is pending")
        .stage = StreamingRequestStage::Preflight;

    let effects = engine
        .handle_api_event(ApiEvent::StreamingResults {
            request_id,
            seed_video_id: "seed".to_owned(),
            candidates: vec![(song("duplicate-pool"), CandidateSource::YtdlpStreaming)],
        })
        .await;
    assert!(effects.is_empty());
    assert!(matches!(
        engine.pending_streaming_request.as_ref(),
        Some(pending)
            if pending.request_id == request_id
                && pending.stage == StreamingRequestStage::Preflight
    ));

    engine
        .handle_api_event(ApiEvent::StreamingPreflighted {
            request_id,
            seed_video_id: "seed".to_owned(),
            songs: vec![song("preflight-pick")],
        })
        .await;
    assert!(!engine.streaming_pending);
    assert!(engine.queue.contains_video_id("preflight-pick"));
}
