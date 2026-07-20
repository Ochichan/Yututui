use super::*;

#[test]
fn ctrl_p_toggles_playlist_search_kind_and_routes_submit() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s')))); // Search screen, input focus
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Playlists);

    for c in "study".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::SearchPlaylists { query, .. } if query == "study")),
        "playlist kind must route to the playlist search command"
    );
    assert!(app.search.searching);

    // Toggling back restores ordinary source-routed search.
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Songs);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| matches!(c, Cmd::Search { .. })));
}

/// A search-results screen with one playlist row selected.

#[test]
fn enter_on_a_playlist_row_fetches_tracks_to_play() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchPlaylistTracks { playlist_id, intent: crate::api::PlaylistIntent::Play, .. }
            if playlist_id == "PLabcdefgh1234"
    )));
}

#[test]
fn enqueue_and_import_keys_map_to_their_playlist_intents() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchPlaylistTracks {
            intent: crate::api::PlaylistIntent::Enqueue,
            ..
        }
    )));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::FetchPlaylistTracks {
            intent: crate::api::PlaylistIntent::Import,
            ..
        }
    )));
}

#[test]
fn playlist_tracks_play_replaces_the_queue() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    let before = serde_json::to_value(app.queue.snapshot()).unwrap();
    let before_rev = app.queue.rev();
    let rejected = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Play,
        songs: songs(3),
    });
    assert_eq!(serde_json::to_value(app.queue.snapshot()).unwrap(), before);
    assert_eq!(app.queue.rev(), before_rev);
    assert!(!app.status.text.contains("Rainy Mix"));
    assert!(
        reject_player_transition(
            &mut app,
            rejected,
            crate::util::delivery::DeliveryError::Busy,
        )
        .is_empty()
    );
    assert_eq!(serde_json::to_value(app.queue.snapshot()).unwrap(), before);
    assert_eq!(app.queue.rev(), before_rev);
    assert!(!app.status.text.contains("Rainy Mix"));

    let mut cmds = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Play,
        songs: songs(3),
    });
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.queue.len(), 3);
    assert_ne!(app.queue.rev(), before_rev);
    assert_eq!(current(&app), "id0");
    assert_loads_video(&cmds, "id0");
    assert!(app.status.text.contains("Rainy Mix"));
}

#[test]
fn playlist_tracks_enqueue_appends_to_the_queue() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    app.why_gem.upsert(
        "id1".to_owned(),
        why_gem::streaming_origin_model(crate::streaming::StreamingMode::Balanced),
    );
    app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Enqueue,
        songs: vec![Song::remote("id1", "t", "a", "0:10")],
    });
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id0", "current track is untouched");
    assert!(
        !app.why_gem.contains("id1"),
        "a manually enqueued duplicate must not inherit recommendation provenance"
    );
}

#[test]
fn playlist_tracks_import_creates_a_local_playlist() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    let cmds = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Import,
        songs: songs(3),
    });
    let imported = app
        .playlists
        .playlists
        .iter()
        .find(|p| p.name == "Rainy Mix")
        .expect("imported playlist");
    assert_eq!(imported.songs.len(), 3);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
}

#[test]
fn empty_playlist_fetch_reports_instead_of_wiping_the_queue() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_playing(2, 0);
    let cmds = app.update(Msg::PlaylistTracks {
        title: "Rainy Mix".to_owned(),
        intent: crate::api::PlaylistIntent::Play,
        songs: Vec::new(),
    });
    assert!(cmds.is_empty());
    assert_eq!(app.queue.len(), 2, "queue survives an empty fetch");
    assert!(!app.status.text.is_empty());
}
