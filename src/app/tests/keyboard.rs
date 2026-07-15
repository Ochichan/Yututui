use super::*;

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
