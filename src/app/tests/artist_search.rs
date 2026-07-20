use super::*;

/// A search-results screen with one artist row selected.
fn app_with_artist_row() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = vec![Song::remote(
        "ytar:UCabc123",
        "Some Artist",
        "",
        "1M subscribers",
    )];
    app.search.selected = 0;
    app
}

fn artist_page() -> crate::api::ArtistPage {
    crate::api::ArtistPage {
        channel_id: "UCabc123".to_owned(),
        name: "Some Artist".to_owned(),
        subscribers: Some("1M subscribers".to_owned()),
        songs: vec![Song::remote("id0", "Top Song", "Some Artist", "1B plays")],
        albums: vec![Song::remote(
            "ytpl:MPREalbum1",
            "First Album",
            "Some Artist",
            "2021",
        )],
        songs_playlist_id: Some("VLPLartistsongs".to_owned()),
    }
}

#[test]
fn ctrl_p_cycles_songs_playlists_artists_and_routes_submit() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s')))); // Search screen, input focus
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Playlists);
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Artists);

    for c in "iu".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Search(SearchCmd::Artists { query, .. }) if query == "iu")),
        "artist kind must route to the artist search command"
    );
    assert!(app.search.searching);

    // A third toggle completes the cycle back to ordinary source-routed search.
    app.update(Msg::Key(ctrl(KeyCode::Char('p'))));
    assert_eq!(app.search.kind, SearchKind::Songs);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Search(SearchCmd::Query { .. })))
    );
}

#[test]
fn enter_on_an_artist_row_fetches_the_page_to_open() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Search(SearchCmd::ArtistPage {
            channel_id,
            intent: crate::api::ArtistIntent::Open,
            ..
        })
            if channel_id == "UCabc123"
    )));
}

#[test]
fn enqueue_and_import_keys_map_to_their_artist_intents() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Search(SearchCmd::ArtistPage {
            intent: crate::api::ArtistIntent::Enqueue,
            ..
        })
    )));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Search(SearchCmd::ArtistPage {
            intent: crate::api::ArtistIntent::Import,
            ..
        })
    )));
}

#[test]
fn favorite_and_download_on_an_artist_row_only_hint() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(cmds.is_empty());
    assert!(!app.status.text.is_empty());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(cmds.is_empty());
    assert!(app.downloads.active.is_empty());
}

#[test]
fn artist_rows_are_excluded_from_the_multi_selection() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    app.search
        .results
        .push(Song::remote("id9", "A Song", "Someone", "3:00"));
    app.search.selected = 0;
    app.search.anchor = 1; // range selection covering both rows
    let songs = app.multi_selected_search_songs().expect("song remains");
    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0].video_id, "id9");
}

#[test]
fn artist_page_opens_the_detail_screen_and_esc_returns_to_search() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    app.update(Msg::Search(SearchMsg::ArtistPage {
        page: artist_page(),
    }));
    assert_eq!(app.mode, Mode::Artist);
    assert!(app.search.artist.is_some());

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.mode, Mode::Search);
    assert!(app.search.artist.is_none());
    assert_eq!(
        app.search.results.len(),
        1,
        "search results survive the round-trip"
    );
}

#[test]
fn empty_artist_page_stays_on_search_with_a_status() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    app.update(Msg::Search(SearchMsg::ArtistPage {
        page: crate::api::ArtistPage {
            songs: Vec::new(),
            albums: Vec::new(),
            ..artist_page()
        },
    }));
    assert_eq!(app.mode, Mode::Search);
    assert!(app.search.artist.is_none());
    assert!(!app.status.text.is_empty());
}

#[test]
fn artist_screen_album_row_fetches_the_album_to_play() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    app.update(Msg::Search(SearchMsg::ArtistPage {
        page: artist_page(),
    }));
    assert_eq!(
        app.search.artist.as_ref().map(|st| st.section),
        Some(ArtistSection::Songs)
    );
    // Tab hops to the albums section; Enter fetches the album through the playlist path.
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(
        app.search.artist.as_ref().map(|st| st.section),
        Some(ArtistSection::Albums)
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Search(SearchCmd::PlaylistTracks {
            playlist_id,
            intent: crate::api::PlaylistIntent::Play,
            ..
        })
            if playlist_id == "MPREalbum1"
    )));
}

#[test]
fn artist_page_error_reports_in_the_status_line() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_artist_row();
    app.update(Msg::Search(SearchMsg::ArtistPageError {
        title: "Some Artist".to_owned(),
        error: "network down".to_owned(),
    }));
    assert_eq!(app.mode, Mode::Search);
    assert!(app.status.text.contains("Some Artist"));
    assert!(app.status.text.contains("network down"));
}
