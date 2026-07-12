use super::*;

#[test]
fn playlists_is_the_last_normal_tab() {
    assert_eq!(
        LibraryTab::NORMAL,
        [
            LibraryTab::All,
            LibraryTab::Favorites,
            LibraryTab::History,
            LibraryTab::Downloads,
            LibraryTab::Playlists,
        ]
    );
}

#[test]
fn n_is_bound_to_playlist_create_in_the_playlists_context() {
    let app = App::new(100);
    let n = Chord::new(KeyCode::Char('n'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Playlists, n),
        Some(Action::PlaylistCreate)
    );
    // The song tabs must not gain the binding.
    assert_eq!(app.keymap.action(KeyContext::Library, n), None);
}

#[test]
fn playlists_root_lists_playlists() {
    let app = app_with_playlists();
    assert!(app.playlists_root());
    assert_eq!(app.library_len(), 2);
    assert_eq!(app.library_count_for(LibraryTab::Playlists), 2);
    // Root rows are playlists, so there are no song rows to act on.
    assert!(app.library_rows().is_empty());
}

#[test]
fn enter_opens_a_playlist_and_back_returns_to_the_list() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    assert_eq!(app.library_ui.open_playlist.as_deref(), Some("alpha"));
    assert_eq!(row_ids(&app), vec!["a1", "a2"]);

    app.update(Msg::Key(key(KeyCode::Char('q')))); // back to the playlist list
    assert_eq!(app.mode, Mode::Library);
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.library_ui.selected, 0); // cursor restored to "Alpha"

    app.update(Msg::Key(key(KeyCode::Char('q')))); // and out of the Library
    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn esc_backs_out_of_an_opened_playlist() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.library_ui.open_playlist.is_some());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.mode, Mode::Library);
}

#[test]
fn a_on_the_playlist_root_plays_that_playlist_as_a_fresh_queue() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Down))); // cursor to "Beta"
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert_eq!(app.mode, Mode::Library, "mode changes only after admission");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "b1");
    assert_loads_video(&cmds, "b1");
}

#[test]
fn enter_in_the_drilldown_plays_the_selected_song() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    app.update(Msg::Key(key(KeyCode::Down))); // cursor to a2
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_loads_video(&cmds, "a2");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(current(&app), "a2");
}

#[test]
fn delete_on_the_playlist_root_asks_then_deletes_on_confirm() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Delete)));
    // Nothing deleted yet — the modal is pending.
    assert_eq!(
        app.library_ui.confirm_playlist_delete.as_deref(),
        Some("alpha")
    );
    assert_eq!(app.playlists.list().len(), 2);

    let cmds = app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.library_ui.confirm_playlist_delete.is_none());
    assert_eq!(app.playlists.list().len(), 1);
    assert!(app.playlists.find("alpha").is_none());
    assert!(app.status.text.contains("Deleted playlist"));
}

#[test]
fn any_other_key_cancels_the_playlist_delete_confirm() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(app.library_ui.confirm_playlist_delete.is_some());
    let cmds = app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert!(
        cmds.iter()
            .all(|c| !matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.library_ui.confirm_playlist_delete.is_none());
    assert_eq!(app.playlists.list().len(), 2);
}

#[test]
fn delete_in_the_drilldown_removes_the_song_from_the_playlist() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    let cmds = app.update(Msg::Key(key(KeyCode::Delete))); // remove a1, no confirm
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert_eq!(app.library_ui.open_playlist.as_deref(), Some("alpha"));
    assert_eq!(row_ids(&app), vec!["a2"]);
    assert_eq!(app.playlists.find("alpha").unwrap().songs.len(), 1);
}

#[test]
fn n_opens_the_create_popup_and_enter_creates_and_selects() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert_eq!(app.library_ui.create_input.as_deref(), Some(""));

    for c in "My Mix".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_ui.create_input.as_deref(), Some("My Mix"));

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.library_ui.create_input.is_none());
    assert!(app.playlists.find("My Mix").is_some());
    // The cursor lands on the new playlist (appended last).
    assert_eq!(app.library_ui.selected, 2);
    assert!(app.status.text.contains("Created playlist"));
}

#[test]
fn esc_cancels_the_create_popup_without_creating() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    app.update(Msg::Key(key(KeyCode::Char('x'))));
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.create_input.is_none());
    assert_eq!(app.playlists.list().len(), 2);
}

#[test]
fn blank_create_popup_enter_hints_instead_of_creating() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .all(|c| !matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    // The popup stays open for a correction.
    assert!(app.library_ui.create_input.is_some());
    assert_eq!(app.playlists.list().len(), 2);
    assert!(app.status.text.contains("Enter a playlist name"));
}

#[test]
fn slash_filters_playlist_names_at_the_root() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "be".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_len(), 1);
    assert_eq!(app.filtered_playlists()[0].name, "Beta");

    // Enter commits the filter; the next Enter opens the (only) filtered playlist.
    app.update(Msg::Key(key(KeyCode::Enter)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.library_ui.open_playlist.as_deref(), Some("beta"));
}

#[test]
fn switching_tabs_closes_an_opened_playlist() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.library_ui.open_playlist.is_some());
    app.update(Msg::Key(key(KeyCode::Tab))); // wraps to All
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.library_ui.tab, LibraryTab::All);
}

#[test]
fn backslash_on_the_playlist_root_enqueues_the_whole_playlist() {
    // Playing state, so the enqueue is a pure append (idle enqueues start playback).
    let mut app = app_playing(1, 0);
    app.playlists.create("Alpha");
    app.playlists.add("Alpha", fsong("a1", "Song A1", "X"));
    app.playlists.add("Alpha", fsong("a2", "Song A2", "Y"));
    open_library_tab(&mut app, LibraryTab::Playlists);
    let before = app.queue.len();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));
    assert_no_load(&cmds); // no interruption
    assert_eq!(app.queue.len(), before + 2); // both "Alpha" tracks
    assert_eq!(app.mode, Mode::Library); // enqueue never leaves the screen
}

#[test]
fn playlists_reload_reconciles_a_dangling_drilldown() {
    let mut app = app_with_playlists();
    app.update(Msg::Key(key(KeyCode::Enter))); // open "Alpha"
    assert!(app.library_ui.open_playlist.is_some());
    // Simulate an external rewrite (finished transfer job) that dropped "alpha".
    app.playlists = crate::playlists::Playlists::default();
    app.reconcile_playlists_reload();
    assert!(app.library_ui.open_playlist.is_none());
    assert_eq!(app.library_ui.selected, 0);
}

// --- "Add to playlist" picker (p / P) --------------------------------------

/// Favorites tab with two tracks and one existing playlist ("Mix").

#[test]
fn p_is_bound_to_add_to_playlist_and_listed_in_the_cheat_sheet() {
    let app = App::new(100);
    let p = Chord::new(KeyCode::Char('p'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Library, p),
        Some(Action::AddToPlaylist)
    );
    assert_eq!(
        app.keymap.action(KeyContext::SearchResults, p),
        Some(Action::AddToPlaylist)
    );
    let shift_p = Chord::new(KeyCode::Char('P'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Player, shift_p),
        Some(Action::AddToPlaylist)
    );
}

#[test]
fn p_on_a_library_row_opens_the_picker_and_enter_adds() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    let picker = app.playlist_picker.as_ref().expect("picker open");
    assert_eq!(picker.songs.len(), 1);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.playlist_picker.is_none());
    assert_eq!(app.playlists.find("Mix").unwrap().songs.len(), 1);
    assert!(app.status.text.contains("Added 1 track to Mix"));
}

#[test]
fn p_with_a_multiselect_range_adds_the_whole_range() {
    let mut app = app_with_picker_fixture();
    app.library_ui.selected = 0;
    app.library_ui.anchor = 1;
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert_eq!(app.playlist_picker.as_ref().unwrap().songs.len(), 2);
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.playlists.find("Mix").unwrap().songs.len(), 2);
}

#[test]
fn adding_a_duplicate_reports_already_there_without_saving() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // first add
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    let cmds = app.update(Msg::Key(key(KeyCode::Enter))); // same song again
    assert!(
        cmds.iter()
            .all(|c| !matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert_eq!(app.playlists.find("Mix").unwrap().songs.len(), 1);
    assert!(app.status.text.contains("Already in playlist"));
}

#[test]
fn picker_n_creates_a_playlist_and_adds_in_one_go() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    app.update(Msg::Key(key(KeyCode::Char('n')))); // jump to the name entry
    assert!(app.playlist_picker.as_ref().unwrap().naming.is_some());
    for c in "Road".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Playlists)))
    );
    assert!(app.playlist_picker.is_none());
    assert_eq!(app.playlists.find("Road").unwrap().songs.len(), 1);
}

#[test]
fn picker_esc_backs_out_of_naming_then_closes() {
    let mut app = app_with_picker_fixture();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    app.update(Msg::Key(key(KeyCode::Esc)));
    let picker = app.playlist_picker.as_ref().expect("back to the list");
    assert!(picker.naming.is_none());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.playlist_picker.is_none());
    assert!(app.playlists.find("Mix").unwrap().songs.is_empty()); // nothing added
}

#[test]
fn shift_p_on_the_player_picks_up_the_current_track() {
    let mut app = app_playing(2, 0);
    app.playlists.create("Mix");
    app.update(Msg::Key(key(KeyCode::Char('P'))));
    let picker = app.playlist_picker.as_ref().expect("picker open");
    assert_eq!(picker.songs[0].video_id, "id0");
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.playlists.find("Mix").unwrap().songs[0].video_id, "id0");
    assert_eq!(app.mode, Mode::Player); // adding never leaves the screen
}

#[test]
fn p_on_a_search_result_picks_that_result() {
    let mut app = App::new(100);
    app.playlists.create("Mix");
    app.mode = Mode::Search;
    app.search.results = songs(2);
    app.search.focus = SearchFocus::Results;
    app.search.selected = 1;
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    let picker = app.playlist_picker.as_ref().expect("picker open");
    assert_eq!(picker.songs[0].video_id, "id1");
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.playlists.find("Mix").unwrap().songs[0].video_id, "id1");
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn p_is_a_noop_on_the_playlists_root() {
    let mut app = app_with_playlists();
    assert!(app.playlists_root());
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert!(app.playlist_picker.is_none());
}

#[test]
fn entering_the_playlists_tab_nudges_playlist_creation() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    open_library_tab(&mut app, LibraryTab::Playlists);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "Press n to create a new playlist");
}

#[test]
fn the_playlist_create_nudge_follows_a_rebind() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.keymap
        .rebind(
            KeyContext::Playlists,
            Action::PlaylistCreate,
            crate::keymap::parse_chord("b").unwrap(),
        )
        .unwrap();
    open_library_tab(&mut app, LibraryTab::Playlists);
    assert_eq!(app.status.text, "Press b to create a new playlist");
}

#[test]
fn mouse_tab_click_on_playlists_also_nudges() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.on_mouse_target(MouseTarget::LibraryTab(LibraryTab::Playlists));
    assert_eq!(app.library_ui.tab, LibraryTab::Playlists);
    assert!(app.status.text.contains("create a new playlist"));
}

// Text zoom (Ctrl+wheel / Ctrl+-/=) ------------------------------------------------
