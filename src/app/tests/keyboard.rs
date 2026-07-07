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
