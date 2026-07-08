use super::*;

#[test]
fn s_enters_search_and_q_is_typed_not_quit() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(!app.should_quit);
    assert_eq!(app.search.input, "q");
}

#[test]
fn korean_letters_still_type_in_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    app.update(Msg::Key(key(KeyCode::Char('ㅂ'))));
    assert!(!app.should_quit);
    assert_eq!(app.search.input, "ㅂ");
}

#[test]
fn korean_shortcut_key_redraws_even_when_unhandled() {
    let mut app = App::new(100);
    app.dirty = false;
    app.update(Msg::Key(key(KeyCode::Char('ㅛ'))));
    assert!(app.dirty);
}

#[test]
fn ime_preedit_scrub_is_disabled_in_text_entry() {
    let mut app = App::new(100);
    assert!(app.should_scrub_ime_preedit());
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert!(!app.should_scrub_ime_preedit());
}

#[test]
fn enter_in_search_emits_search_cmd() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.search.searching);
    match cmds.as_slice() {
        [
            Cmd::Search {
                query,
                source,
                config,
                ..
            },
        ] => {
            assert_eq!(query, "lofi");
            assert_eq!(*source, SearchSource::Youtube);
            assert_eq!(config.source, SearchSource::Youtube);
        }
        _ => panic!("expected a Search cmd"),
    }
}

#[test]
fn tab_opens_search_source_menu_and_cycles_source() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));

    let cmds = app.update(Msg::Key(key(KeyCode::Tab)));
    assert!(cmds.is_empty());
    assert!(app.dropdowns.search_source_open);
    assert_eq!(app.search.source, SearchSource::Youtube);

    let cmds = app.update(Msg::Key(key(KeyCode::Down)));
    assert!(app.dropdowns.search_source_open);
    assert_eq!(app.search.source, SearchSource::SoundCloud);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Persist(PersistCmd::Config(cfg))] if cfg.search.source == SearchSource::SoundCloud
    ));

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert!(!app.dropdowns.search_source_open);
}

#[test]
fn shift_tab_toggles_search_focus_between_input_and_results() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(1);
    app.search.focus = SearchFocus::Results;
    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.search.focus, SearchFocus::Input);

    app.update(Msg::Key(key(KeyCode::BackTab)));
    assert_eq!(app.search.focus, SearchFocus::Results);
}

#[test]
fn remapped_search_focus_toggle_updates_both_directions() {
    let mut app = App::new(100);
    app.keymap
        .rebind(
            KeyContext::SearchResults,
            Action::FocusPrev,
            crate::keymap::parse_chord("f5").unwrap(),
        )
        .unwrap();

    app.mode = Mode::Search;
    app.search.results = songs(1);
    app.search.focus = SearchFocus::Results;
    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.search.focus, SearchFocus::Input);

    app.update(Msg::Key(key(KeyCode::F(5))));
    assert_eq!(app.search.focus, SearchFocus::Results);
}

#[test]
fn search_submit_stays_enter_when_common_confirm_is_remapped() {
    let mut app = App::new(100);
    app.keymap = confirm_on_f5_keymap();
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    let cmds = app.update(Msg::Key(key(KeyCode::F(5))));
    assert!(cmds.is_empty());
    assert!(!app.search.searching);

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.search.searching);
    match cmds.as_slice() {
        [Cmd::Search { query, source, .. }] => {
            assert_eq!(query, "lofi");
            assert_eq!(*source, SearchSource::Youtube);
        }
        _ => panic!("expected a Search cmd"),
    }
}
