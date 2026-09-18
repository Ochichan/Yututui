use super::*;
use crate::streaming::{TasteEdit, TasteOutcome};

#[test]
fn gem_off_session_taste_projects_identical_bans_across_owners() {
    use crate::library::Library;

    let banned = song("banned-track");
    let other = Song::remote("kept-track", "Kept", "Other Artist", "3:00");
    let mut taste = crate::streaming::SessionTaste::default();
    assert_eq!(
        taste.apply(TasteEdit::ban_track(&banned).expect("id")),
        TasteOutcome::Applied
    );
    assert_eq!(
        taste.apply(TasteEdit::ban_artist(&banned).expect("artist")),
        TasteOutcome::Applied
    );
    assert_eq!(
        taste.apply(TasteEdit::parse_seed("jazz").expect("term")),
        TasteOutcome::Applied
    );

    let mut config = Config::default();
    config.streaming.ai.enabled = false;
    let mut engine = DaemonEngine::with_state(
        EngineState {
            config,
            station: StationStore::default(),
            library: Library::default(),
            playlists: crate::playlists::Playlists::default(),
            signals: Signals::default(),
        },
        Arc::new(|_event| {}),
    );
    engine.set_taste_for_test(taste.clone());

    let mut app = App::new(Config::default().volume);
    app.config.streaming.ai.enabled = false;
    app.ai.available = false;
    app.streaming.taste = taste;

    let app_state = app.recommendation_station_state_for_test("seed-x");
    let engine_state = engine.recommendation_station_state_for_test("seed-x");
    assert_eq!(app_state.banned_track_ids, engine_state.banned_track_ids);
    assert_eq!(
        app_state.banned_artist_keys,
        engine_state.banned_artist_keys
    );
    assert!(app_state.banned_track_ids.contains("banned-track"));
    assert!(
        !app_state.banned_track_ids.contains("kept-track"),
        "unrelated ids stay off the ban set"
    );
    assert!(app_state.banned_artist_keys.contains("artist-banned-track"));
    assert!(!app_state.seed_bias.is_empty());
    let probe = crate::streaming::Candidate::from_song(
        other,
        crate::streaming::CandidateSource::YtdlpStreaming,
        0,
    );
    assert_eq!(
        app_state.seed_bias.score(&probe),
        engine_state.seed_bias.score(&probe)
    );
}
