//! A–B loop TUI keybinding, seekbar marker, status-line tag, and track-change reset tests.

use super::*;
use yututui_core::ab_loop::AbLoopState;

#[test]
fn player_b_key_cycles_ab_loop_and_renders_status_and_seekbar_markers() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);

    let mut app = App::new(100);
    app.queue.set(
        vec![
            fsong("vid-a", "Track A", "Artist"),
            fsong("vid-b", "Track B", "Artist"),
        ],
        0,
    );
    app.install_seek_parity_state("vid-a", 30.0, 200.0);

    // First `b` sets point A at 0:30.
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('b'))));
    assert!(cmds.iter().flat_map(Cmd::player_commands).any(|c| {
        matches!(c, PlayerCmd::SetProperty { name, value } if name == "ab-loop-a" && value.as_f64() == Some(30.0))
    }));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.ab_loop(), AbLoopState::PointA { a_secs: 30.0 });

    let buf_a = render_app_buffer(&app, 100, 24);
    assert!(
        (0..24).any(|y| buffer_row(&buf_a, y).contains("A 0:30")),
        "status line should show A 0:30"
    );

    // Move to 2:20 (140s, outside the centered timestamp label) and press `b` again -> Active [30s, 140s].
    app.playback.time_pos = Some(140.0);
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('b'))));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.ab_loop(),
        AbLoopState::Active {
            a_secs: 30.0,
            b_secs: 140.0
        }
    );

    let buf_ab = render_app_buffer(&app, 100, 24);
    assert!(
        (0..24).any(|y| buffer_row(&buf_ab, y).contains("A-B 0:30")),
        "status line should show A-B range"
    );
    let seekbar_y = app.hits.seekbar_rect().expect("seekbar rect").y;
    let seekbar_row = buffer_row(&buf_ab, seekbar_y);
    assert!(
        seekbar_row.contains('[') && seekbar_row.contains(']'),
        "seekbar should render A/B brackets, got: {seekbar_row:?}"
    );
    assert!(
        seekbar_row.contains("2:20 / 3:20"),
        "seekbar timestamp label must stay intact, got: {seekbar_row:?}"
    );

    // Alt+Shift+B clears the loop immediately.
    let mut cmds = app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('B'),
        KeyModifiers::ALT | KeyModifiers::SHIFT,
    )));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.ab_loop(), AbLoopState::Off);
}

#[test]
fn track_change_clears_active_ab_loop_and_emits_mpv_reset() {
    let mut app = App::new(100);
    app.queue.set(
        vec![
            fsong("vid-a", "Track A", "Artist"),
            fsong("vid-b", "Track B", "Artist"),
        ],
        0,
    );
    app.install_seek_parity_state("vid-a", 20.0, 180.0);

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('b'))));
    admit_player_transition(&mut app, &mut cmds);
    app.playback.time_pos = Some(60.0);
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('b'))));
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.ab_loop().is_active());

    let mut next_cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    let saw_clear_a = next_cmds.iter().flat_map(Cmd::player_commands).any(|c| {
        matches!(c, PlayerCmd::SetProperty { name, value } if name == "ab-loop-a" && value.as_str() == Some("no"))
    });
    let saw_clear_b = next_cmds.iter().flat_map(Cmd::player_commands).any(|c| {
        matches!(c, PlayerCmd::SetProperty { name, value } if name == "ab-loop-b" && value.as_str() == Some("no"))
    });
    assert!(
        saw_clear_a && saw_clear_b,
        "track transition must reset mpv ab-loop properties"
    );
    admit_player_transition(&mut app, &mut next_cmds);
    assert_eq!(app.ab_loop(), AbLoopState::Off);
}
