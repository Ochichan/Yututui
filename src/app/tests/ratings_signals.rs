use super::*;

#[test]
fn rating_key_cycles_neutral_like_dislike() {
    let mut app = app_playing(2, 0);
    let id = current(&app).to_owned();
    // Starts neutral: neither favorited nor disliked.
    assert!(!app.library.is_favorite(&id));
    assert!(!app.signals.is_disliked(&id));
    // First `f` → like (favorite); persists both library and signals.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(app.library.is_favorite(&id));
    assert!(!app.signals.is_disliked(&id));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    // Second `f` → dislike; flips the flag and drops the favorite.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(!app.library.is_favorite(&id));
    assert!(app.signals.is_disliked(&id));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    // Third `f` → back to neutral.
    app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(!app.library.is_favorite(&id));
    assert!(!app.signals.is_disliked(&id));
}

#[test]
fn rating_radio_toggles_radio_favorite_without_signals() {
    let mut app = App::new(100);
    let station = radio_station("station-like");
    app.queue.set(vec![station], 0);
    app.mode = Mode::Player;
    app.load_song(app.queue.current().cloned());

    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert!(app.library.favorites.is_empty());
    assert!(app.library.is_radio_favorite("rad:station-like"));
    assert!(!app.signals.is_disliked("rad:station-like"));
    assert_eq!(
        app.library.radios.front().map(|s| s.video_id.as_str()),
        Some("rad:station-like")
    );

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());

    app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert_eq!(row_ids(&app), vec!["rad:station-like"]);
    app.library_ui.tab = LibraryTab::Radio;
    assert!(app.library_rows().is_empty());

    app.mode = Mode::Player;
    app.queue.set(vec![radio_station("station-like")], 0);
    app.load_song(app.queue.current().cloned());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert!(!app.library.is_radio_favorite("rad:station-like"));

    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::RadioFavorites;
    assert!(app.library_rows().is_empty());
    app.library_ui.tab = LibraryTab::Radio;
    assert_eq!(row_ids(&app), vec!["rad:station-like"]);
}

#[test]
fn manual_next_records_signals_then_advances() {
    let mut app = app_playing(3, 0);
    let id = current(&app).to_owned();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    // The skipped track is persisted (SaveSignals) and playback advances.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert_ne!(current(&app), id);
}

#[test]
fn manual_next_from_radio_does_not_record_signals() {
    let mut app = App::new(100);
    app.queue.set(
        vec![
            radio_station("station-skip"),
            Song::remote("id0", "t0", "a", "0:10"),
        ],
        0,
    );
    app.mode = Mode::Player;
    app.load_song(app.queue.current().cloned());

    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));

    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert_eq!(current(&app), "id0");
}

#[test]
fn eof_records_signals_for_the_finished_track() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Eof);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
}
