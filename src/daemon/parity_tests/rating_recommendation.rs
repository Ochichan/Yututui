use super::*;
use crate::streaming::{self, CandidateSource, Cooc, StationState};

fn rerank_snapshot(state: &StationState, signals: &Signals) -> Vec<(String, f32)> {
    let candidates = vec![
        Song::remote(
            "rated-artist-candidate",
            "Rated Artist Song",
            "artist-b",
            "3:45",
        ),
        Song::remote("other-candidate", "Other Song", "artist-other", "3:45"),
        Song::remote("third-candidate", "Third Song", "artist-third", "3:45"),
    ];
    let config = Config::default().streaming;
    let pool = streaming::pool_from_songs(candidates, CandidateSource::YtdlpStreaming);
    streaming::shortlist_for_ai(pool, state, signals, &Cooc::default(), &config, 3, 1)
        .into_iter()
        .map(|candidate| (candidate.video_id().to_owned(), candidate.base_score))
        .collect()
}

fn assert_recommendation_parity(
    step: &str,
    app: &App,
    engine: &DaemonEngine,
    expected_artist_bias: f32,
) {
    assert_eq!(
        serde_json::to_value(&*app.signals).unwrap(),
        serde_json::to_value(engine.signals()).unwrap(),
        "{step}: persistent recommendation projections diverged"
    );

    let app_state = app.recommendation_station_state_for_test("b");
    let daemon_state = engine.recommendation_station_state_for_test("b");
    assert_eq!(
        app_state.session_artist_bias, daemon_state.session_artist_bias,
        "{step}: session recommendation snapshots diverged"
    );
    assert_eq!(
        app_state.session_artist_bias.get("artist-b").copied(),
        Some(expected_artist_bias),
        "{step}: rating feedback was missing or recorded more than once"
    );
    assert_eq!(
        rerank_snapshot(&app_state, &app.signals),
        rerank_snapshot(&daemon_state, engine.signals()),
        "{step}: production rerank results diverged"
    );
}

#[tokio::test]
async fn os_rating_feedback_keeps_session_reranking_in_parity() {
    let (mut app, mut engine) = hermetic_pair();

    for (step, command, expected_bias) in [
        ("liked", MediaCommand::Like, 0.15),
        ("disliked", MediaCommand::Dislike, -0.25),
        ("neutral", MediaCommand::Dislike, -0.25),
    ] {
        app.update(Msg::Media(command.clone()));
        let (shutdown, effects) = engine.handle_media(command).await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_recommendation_parity(step, &app, &engine, expected_bias);
    }

    let (mut app, mut engine) = hermetic_pair();
    for (step, command, expected_bias) in [
        ("OS like", MediaCommand::Like, 0.15),
        ("OS neutral from like", MediaCommand::Like, 0.15),
        ("OS dislike", MediaCommand::Dislike, -0.25),
        ("OS neutral from dislike", MediaCommand::Dislike, -0.25),
    ] {
        app.update(Msg::Media(command.clone()));
        let (shutdown, effects) = engine.handle_media(command).await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_recommendation_parity(step, &app, &engine, expected_bias);
    }
}

#[tokio::test]
async fn radio_favorites_never_enter_track_session_reranking() {
    let (mut app, mut engine) = hermetic_pair();
    let mut station = Song::remote("station", "Station", "Radio", "");
    station.playable = Some(crate::api::PlayableRef::RadioStream {
        url: "https://radio.example/station.mp3".to_owned(),
    });
    let mut queue = Queue::default();
    queue.set(vec![station], 0);
    let snapshot = queue.snapshot();
    engine.restore_queue_snapshot(snapshot.clone(), RNG_SEED);
    app.queue.restore_snapshot(snapshot);

    app.update(Msg::Media(MediaCommand::Like));
    let (shutdown, effects) = engine.handle_media(MediaCommand::Like).await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    let app_effects = app.update(Msg::Media(MediaCommand::Like));
    let (shutdown, engine_effects) = engine.handle_media(MediaCommand::Like).await;
    assert!(!shutdown);
    assert!(
        app_effects
            .iter()
            .all(|effect| matches!(effect, Cmd::Persist(_)))
    );
    assert!(engine_effects.is_empty());

    let app_state = app.recommendation_station_state_for_test("station");
    let daemon_state = engine.recommendation_station_state_for_test("station");
    assert_eq!(
        serde_json::to_value(&*app.signals).unwrap(),
        serde_json::to_value(engine.signals()).unwrap()
    );
    assert!(app_state.session_artist_bias.is_empty());
    assert_eq!(
        app_state.session_artist_bias,
        daemon_state.session_artist_bias
    );
}
