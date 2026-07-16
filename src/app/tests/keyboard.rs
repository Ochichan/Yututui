use super::*;

fn use_legacy_keyboard(app: &mut App) {
    app.terminal_keyboard_mode = crate::terminal_keyboard::KeyboardInputMode::Legacy;
}

fn begin_key_capture(app: &mut App, ctx: KeyContext, action: Action) {
    if app.settings.is_none() {
        app.open_settings();
    }
    let row = crate::keymap::editable_entries()
        .iter()
        .position(|entry| *entry == (ctx, action))
        .expect("editable keybinding");
    let st = app.settings.as_mut().expect("settings open");
    st.tab = SettingsTab::Keys;
    st.row = row;
    app.settings_begin_capture();
    assert_eq!(
        app.settings.as_ref().unwrap().capturing,
        Some((ctx, action))
    );
}

#[test]
fn q_is_back_in_player_mode_without_quitting() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_q_quits_in_player_mode() {
    let mut app = App::new(100);
    app.update(Msg::Key(ctrl(KeyCode::Char('q'))));
    assert!(app.should_quit);
}

#[test]
fn korean_q_key_is_back_without_quitting() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('ㅂ'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn korean_ctrl_q_key_quits_in_player_mode() {
    let mut app = App::new(100);
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅂ'))));
    assert!(app.should_quit);
}

#[test]
fn korean_ctrl_c_still_quits() {
    let mut app = App::new(100);
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅊ'))));
    assert!(app.should_quit);
}

#[test]
fn legacy_ctrl_h_deletes_a_word_in_search_without_navigating() {
    let mut app = App::new(100);
    use_legacy_keyboard(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "alpha beta".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));

    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.input, "alpha ");
}

#[test]
fn legacy_ctrl_h_is_consumed_outside_text_entry_and_by_overlays() {
    let mut app = App::new(100);
    use_legacy_keyboard(&mut app);
    app.mode = Mode::Library;
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Library);

    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Input;
    app.search.input = "alpha beta".to_owned();
    app.search.input_cursor = TextCursor::at_end(&app.search.input);
    app.overlays.help_visible = true;
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.input, "alpha beta");
    assert!(app.overlays.help_visible);
}

#[test]
fn legacy_ctrl_h_is_released_when_delete_word_is_remapped() {
    let mut app = App::new(100);
    use_legacy_keyboard(&mut app);
    app.keymap
        .rebind(
            KeyContext::Common,
            Action::DeleteWord,
            crate::keymap::parse_chord("f8").unwrap(),
        )
        .unwrap();
    app.mode = Mode::Search;

    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));

    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn legacy_fallback_survives_home_remap_but_yields_to_an_explicit_ctrl_h_claim() {
    let mut app = App::new(100);
    use_legacy_keyboard(&mut app);
    app.keymap
        .rebind(
            KeyContext::Global,
            Action::Home,
            crate::keymap::parse_chord("f8").unwrap(),
        )
        .unwrap();
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Input;
    for c in "alpha beta".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.input, "alpha ");

    app.update(Msg::Key(key(KeyCode::F(8))));
    assert_eq!(app.mode, Mode::Player);

    app.keymap
        .rebind(
            KeyContext::Global,
            Action::ToggleAbout,
            crate::keymap::parse_chord("ctrl+h").unwrap(),
        )
        .unwrap();
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert!(app.overlays.about_visible);
}

#[test]
fn legacy_settings_capture_owns_ctrl_h_before_home_navigation() {
    let mut app = App::new(100);
    use_legacy_keyboard(&mut app);
    begin_key_capture(&mut app, KeyContext::Global, Action::Home);

    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));

    assert_eq!(app.mode, Mode::Settings);
    assert!(app.settings.is_some());
    assert!(app.settings.as_ref().unwrap().capturing.is_none());
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .keymap
            .chord(KeyContext::Global, Action::Home),
        crate::keymap::parse_chord("ctrl+h")
    );
}

#[test]
fn settings_capture_conflict_checks_ctrl_q_but_ctrl_c_remains_emergency_quit() {
    let mut app = App::new(100);
    use_legacy_keyboard(&mut app);
    begin_key_capture(&mut app, KeyContext::Player, Action::TogglePause);

    app.update(Msg::Key(ctrl(KeyCode::Char('q'))));

    assert!(!app.should_quit);
    assert_eq!(app.mode, Mode::Settings);
    assert_eq!(
        app.overlays.key_conflict.map(|conflict| conflict.existing),
        Some(Action::Quit)
    );

    let mut emergency = App::new(100);
    begin_key_capture(&mut emergency, KeyContext::Player, Action::TogglePause);
    let mut cmds = emergency.update(Msg::Key(ctrl(KeyCode::Char('c'))));
    assert!(
        !cmds.is_empty(),
        "Ctrl+C must start the Settings quit transaction"
    );
    admit_player_transition(&mut emergency, &mut cmds);
    assert!(emergency.should_quit);
}

#[test]
fn settings_capture_escape_still_cancels_without_leaving_settings() {
    let mut app = App::new(100);
    begin_key_capture(&mut app, KeyContext::Player, Action::TogglePause);

    app.update(Msg::Key(key(KeyCode::Esc)));

    assert_eq!(app.mode, Mode::Settings);
    assert!(app.settings.as_ref().unwrap().capturing.is_none());
    assert!(!app.status.text.is_empty());
}

#[test]
fn ctrl_a_selects_then_backspace_clears_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s')))); // open search (input focused)
    assert_eq!(app.mode, Mode::Search);
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.search.input, "lofi");
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.search.select_all);
    // Backspace with everything selected clears the field, not one char.
    app.update(Msg::Key(key(KeyCode::Backspace)));
    assert_eq!(app.search.input, "");
    assert!(!app.search.select_all);
}

#[test]
fn ctrl_a_then_typing_replaces_search_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert_eq!(app.search.input, "x");
    assert!(!app.search.select_all);
}

#[test]
fn search_and_ai_support_middle_cursor_edits() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "ab🙂c".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Left)));
    assert_eq!(app.search.input_cursor.byte_index(&app.search.input), 2);
    app.update(Msg::Key(key(KeyCode::Char('X'))));
    assert_eq!(app.search.input, "abX🙂c");
    app.update(Msg::Key(key(KeyCode::Backspace)));
    assert_eq!(app.search.input, "ab🙂c");

    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "one two".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Left)));
    app.update(Msg::Key(key(KeyCode::Char('X'))));
    assert_eq!(app.ai.input, "one Xtwo");
}

#[test]
fn select_all_collapses_directionally_without_an_extra_jump() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    app.update(Msg::Key(ctrl(KeyCode::Left)));
    assert!(!app.search.select_all);
    assert_eq!(app.search.input_cursor.byte_index(&app.search.input), 0);

    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    app.update(Msg::Key(key(KeyCode::Right)));
    assert!(!app.search.select_all);
    assert_eq!(
        app.search.input_cursor.byte_index(&app.search.input),
        app.search.input.len()
    );
}

#[test]
fn text_edit_remaps_beat_typeable_and_input_context_actions() {
    let mut search = App::new(100);
    search.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        search.update(Msg::Key(key(KeyCode::Char(c))));
    }
    search.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    search
        .keymap
        .rebind(
            KeyContext::Common,
            Action::DeleteChar,
            Chord::new(KeyCode::Char(';'), KeyModifiers::empty()),
        )
        .unwrap();
    search.update(Msg::Key(key(KeyCode::Char(';'))));
    assert!(search.search.input.is_empty());

    for c in "word".chars() {
        search.update(Msg::Key(key(KeyCode::Char(c))));
    }
    search
        .keymap
        .rebind(
            KeyContext::Common,
            Action::MoveCursorLeft,
            Chord::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        )
        .unwrap();
    search.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(!search.search.select_all);
    assert_eq!(
        search.search.input_cursor.byte_index(&search.search.input),
        "wor".len()
    );

    let mut ai = App::new(100);
    ai.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "gem".chars() {
        ai.update(Msg::Key(key(KeyCode::Char(c))));
    }
    ai.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    ai.keymap
        .rebind(
            KeyContext::Common,
            Action::DeleteChar,
            Chord::new(KeyCode::Char(';'), KeyModifiers::empty()),
        )
        .unwrap();
    ai.update(Msg::Key(key(KeyCode::Char(';'))));
    assert!(ai.ai.input.is_empty());
}

#[test]
fn ctrl_backspace_deletes_words_in_search_and_ai_inputs() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi hip hop".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Backspace)));
    assert_eq!(app.search.input, "lofi hip ");
    app.update(Msg::Key(key(KeyCode::Backspace)));
    assert_eq!(app.search.input, "lofi hip");
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    app.update(Msg::Key(ctrl(KeyCode::Backspace)));
    assert!(app.search.input.is_empty());

    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('g'))));
    for c in "quiet piano".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Backspace)));
    assert_eq!(app.ai.input, "quiet ");
}

#[test]
fn word_delete_honors_remap_and_unbind_in_text_inputs() {
    let mut app = App::new(100);
    app.keymap
        .rebind(
            KeyContext::Common,
            Action::DeleteWord,
            crate::keymap::parse_chord("f8").unwrap(),
        )
        .unwrap();
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "one two".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }

    app.update(Msg::Key(ctrl(KeyCode::Backspace)));
    assert_eq!(app.search.input, "one two");
    app.update(Msg::Key(key(KeyCode::F(8))));
    assert_eq!(app.search.input, "one ");

    app.keymap.unbind(KeyContext::Common, Action::DeleteWord);
    app.update(Msg::Key(key(KeyCode::F(8))));
    assert_eq!(app.search.input, "one ");
}

#[test]
fn navigating_away_clears_a_pending_select_all_highlight() {
    let mut app = App::new(100);
    // Search box: select the whole query, then leave via Ctrl+H (a global nav action that's
    // resolved before the input handler's own deselect runs).
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    for c in "lofi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.search.select_all);
    app.update(Msg::Key(ctrl(KeyCode::Char('h')))); // go home
    assert!(
        !app.search.select_all,
        "highlight must not survive leaving the screen"
    );

    // DJ Gem box: same story — select all, leave, flag cleared so it can't reappear highlighted.
    app.update(Msg::Key(key(KeyCode::Char('g')))); // enter DJ Gem
    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.ai.select_all);
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert!(!app.ai.select_all);
}

#[test]
fn ctrl_a_selects_then_backspace_clears_ai_input() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('g')))); // open DJ Gem assistant (input focused)
    assert_eq!(app.mode, Mode::Ai);
    for c in "hi".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.ai.input, "hi");
    app.update(Msg::Key(ctrl(KeyCode::Char('a'))));
    assert!(app.ai.select_all);
    app.update(Msg::Key(key(KeyCode::Backspace)));
    assert_eq!(app.ai.input, "");
    assert!(!app.ai.select_all);
}
