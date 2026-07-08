use super::*;

#[test]
fn f_toggles_favorite_of_current_track() {
    let mut app = app_playing(3, 0); // playing "id0"
    assert!(!app.library.is_favorite("id0"));
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(app.library.is_favorite("id0"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    app.update(Msg::Key(key(KeyCode::Char('f')))); // toggle off
    assert!(!app.library.is_favorite("id0"));
}

#[test]
fn playing_records_history_most_recent_first() {
    let mut app = app_playing(3, 0); // loads id0 -> history [id0]
    app.update(Msg::Key(key(KeyCode::Char('.')))); // id1 -> [id1, id0]
    let hist: Vec<&str> = app
        .library
        .history
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(hist, vec!["id1", "id0"]);
}

#[test]
fn playing_radio_records_radio_tab_only() {
    let mut app = App::new(100);
    let station = radio_station("station-a");
    app.queue.set(vec![station.clone()], 0);
    let cmds = app.load_song(app.queue.current().cloned());

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(app.library.history.is_empty());
    assert!(app.library.favorites.is_empty());
    assert_eq!(
        app.library.radios.front().map(|s| s.video_id.as_str()),
        Some("rad:station-a")
    );

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::All;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::History;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::Favorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::Radio;
    assert!(app.library_rows().is_empty());

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Radio;
    assert_eq!(row_ids(&app), vec!["rad:station-a"]);
}

#[test]
fn radio_favorite_is_separate_from_song_favorites() {
    let mut app = App::new(100);
    let station = radio_station("station-fav");
    app.search.results = vec![station.clone()];
    app.search.focus = SearchFocus::Results;
    app.mode = Mode::Search;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(app.library.favorites.is_empty());
    assert!(app.library.history.is_empty());
    assert_eq!(app.library.radio_favorites.len(), 1);
    assert_eq!(app.library.radio_favorites[0].video_id, "rad:station-fav");
    assert!(app.library.is_favorite("rad:station-fav"));

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::All;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert_eq!(row_ids(&app), vec!["rad:station-fav"]);
    app.library_ui.tab = LibraryTab::Radio;
    assert!(app.library_rows().is_empty());
}

#[test]
fn favorite_from_search_results() {
    let mut app = App::new(100);
    app.search.results = songs(3);
    app.search.selected = 1;
    app.search.focus = SearchFocus::Results;
    app.mode = Mode::Search;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(app.library.is_favorite("id1"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
}

#[test]
fn l_opens_library_and_enter_plays_selected() {
    let mut app = app_playing(3, 0);
    // favorites become [id0, id1] (most-recent-first insertion).
    app.library.toggle_favorite(&songs(2)[1]);
    app.library.toggle_favorite(&songs(2)[0]);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::Down))); // select all[1] = id1
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(current(&app), "id1");
    assert_loads_video(&cmds, "id1");
}

#[test]
fn other_screens_keep_remapped_confirm_key() {
    // Library isn't hardwired to Enter the way Search is: a remapped Common Confirm key (F5)
    // still plays there, via the Common fallback. (Library also keeps its own Enter→play
    // binding, like the Queue window — see `enter_on_library_plays_selected_song...`.)
    let mut app = app_playing(3, 0);
    app.keymap = confirm_on_f5_keymap();
    app.library.toggle_favorite(&songs(2)[1]);
    app.library.toggle_favorite(&songs(2)[0]);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(key(KeyCode::Down))); // select all[1] = id1

    let cmds = app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(current(&app), "id1");
    assert_loads_video(&cmds, "id1");
}

#[test]
fn q_closes_library_without_quitting_app() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn library_tab_toggles_and_unfavorite_fixes_selection() {
    let mut app = app_playing(1, 0);
    app.library.toggle_favorite(&songs(1)[0]); // favorites = [id0]
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::History);
    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    // Unfavorite the only entry: selection clamps to 0, list empties.
    app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert_eq!(app.library_ui.selected, 0);
    assert!(app.library.favorites.is_empty());
}

#[test]
fn library_all_includes_downloaded_tracks_and_loads_local_path() {
    let mut app = App::new(100);
    let local = Song::local_file(PathBuf::from("/tmp/local-track.m4a"));
    app.library_ui.downloaded = vec![local.clone()];
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    assert_eq!(app.library_len(), 1);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(load_url(&cmds), Some("/tmp/local-track.m4a"));
    assert_eq!(app.queue.current().unwrap().video_id, local.video_id);
}

#[test]
fn downloads_tab_shows_download_folder_tracks() {
    let mut app = App::new(100);
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/a.m4a"))];
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    open_library_tab(&mut app, LibraryTab::Downloads);
    assert_eq!(app.library_ui.tab, LibraryTab::Downloads);
    assert_eq!(app.library_len(), 1);
}

// --- in-library filter (`/`) -------------------------------------------------
