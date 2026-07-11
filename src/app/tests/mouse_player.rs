use super::*;

#[test]
fn click_on_seekbar_seeks_to_fraction() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // Column 50 of a 100-wide bar → 50% of 200 s → ~100 s.
    let cmds = app.update(Msg::MouseClick { col: 50, row: 5 });
    match cmds.as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 100.0).abs() < 1.0),
        _ => panic!("expected a SeekAbsolute cmd"),
    }
}

#[test]
fn click_off_seekbar_is_ignored() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    assert!(app.update(Msg::MouseClick { col: 50, row: 9 }).is_empty()); // wrong row
    assert!(app.update(Msg::MouseClick { col: 200, row: 5 }).is_empty()); // past the bar
}

#[test]
fn click_does_nothing_outside_player_mode() {
    // Legacy Top layout: no docked box on other screens, so a stale seekbar rect must
    // not take clicks there. (In the default Bottom layout the docked bar makes this
    // click live everywhere — covered in `docked_bar`.)
    let mut app = app_playing(1, 0);
    app.config.player_bar_position = Some(crate::config::PlayerBarPosition::Top);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    app.mode = Mode::Search;
    assert!(app.update(Msg::MouseClick { col: 50, row: 5 }).is_empty());
}

#[test]
fn drag_on_seekbar_scrubs_continuously() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // Press on the bar arms the scrub and seeks (col 25 → 50 s).
    match app.update(Msg::MouseClick { col: 25, row: 5 }).as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 50.0).abs() < 1.0),
        _ => panic!("expected a SeekAbsolute from the press"),
    }
    // Dragging to a new column seeks continuously — even off the bar's row (row ignored).
    match app.update(Msg::MouseDrag { col: 75, row: 9 }).as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 150.0).abs() < 1.0),
        _ => panic!("expected a SeekAbsolute from the drag"),
    }
    // Same cell → no duplicate seek (intra-cell dedupe).
    assert!(app.update(Msg::MouseDrag { col: 75, row: 9 }).is_empty());
    // Dragging past the right end pins near the maximum (clamped to width-1, like click-seek).
    match app.update(Msg::MouseDrag { col: 250, row: 5 }).as_slice() {
        [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 198.0).abs() < 1.0),
        _ => panic!("expected a clamped SeekAbsolute"),
    }
    // Release ends the scrub; a later stray drag does nothing.
    app.update(Msg::MouseLeftUp);
    assert!(
        app.update(Msg::MouseDrag { col: 10, row: 5 }).is_empty(),
        "no scrub after release"
    );
}

#[test]
fn drag_without_a_seekbar_press_does_not_seek() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // No prior press on the bar → a bare drag must not seek.
    assert!(app.update(Msg::MouseDrag { col: 50, row: 5 }).is_empty());
}

#[test]
fn click_player_buttons_dispatch_actions() {
    let mut app = app_playing(3, 0);
    app.register_mouse_button(
        Rect {
            x: 10,
            y: 4,
            width: 9,
            height: 1,
        },
        MouseTarget::Player(Action::TogglePause),
    );
    let cmds = app.update(Msg::MouseClick { col: 12, row: 4 });
    assert!(app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::CyclePause)]
    ));

    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 22,
            y: 4,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::VolUp),
    );
    let cmds = app.update(Msg::MouseClick { col: 25, row: 4 });
    assert_eq!(app.playback.volume, 45);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(45))]
    ));
}

#[test]
fn wheel_over_volume_cluster_adjusts_volume_when_enabled() {
    let mut app = app_playing(1, 0);
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 20,
            y: 4,
            width: 16,
            height: 1,
        },
        MouseTarget::VolumeArea,
    );

    let cmds = app.update(Msg::MouseScroll {
        up: true,
        col: 25,
        row: 4,
        ctrl: false,
    });
    assert_eq!(app.playback.volume, 45);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(45))]
    ));

    let cmds = app.update(Msg::MouseScroll {
        up: false,
        col: 25,
        row: 4,
        ctrl: false,
    });
    assert_eq!(app.playback.volume, 40);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(40))]
    ));
}

#[test]
fn wheel_volume_setting_can_disable_volume_scroll() {
    let mut app = app_playing(1, 0);
    app.config.mouse_wheel_volume = Some(false);
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 20,
            y: 4,
            width: 16,
            height: 1,
        },
        MouseTarget::VolumeArea,
    );

    let cmds = app.update(Msg::MouseScroll {
        up: true,
        col: 25,
        row: 4,
        ctrl: false,
    });
    assert!(cmds.is_empty());
    assert_eq!(app.playback.volume, 40);
}

#[test]
fn click_next_button_loads_next_track() {
    let mut app = app_playing(3, 0);
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 1,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::NextTrack),
    );
    let cmds = app.update(Msg::MouseClick { col: 3, row: 1 });
    assert_eq!(current(&app), "id1");
    assert_loads_video(&cmds, "id1");
}

#[test]
fn click_help_button_opens_cheatsheet() {
    let mut app = app_playing(1, 0);
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 9,
            width: 16,
            height: 1,
        },
        MouseTarget::Global(Action::ToggleHelp),
    );
    assert!(app.update(Msg::MouseClick { col: 4, row: 9 }).is_empty());
    assert!(app.overlays.help_visible);
}

#[test]
fn click_mouse_help_button_opens_mouse_cheatsheet() {
    let mut app = app_playing(1, 0);
    app.register_mouse_button(
        Rect {
            x: 18,
            y: 9,
            width: 8,
            height: 1,
        },
        MouseTarget::MouseHelp,
    );
    assert!(app.update(Msg::MouseClick { col: 20, row: 9 }).is_empty());
    assert!(app.overlays.mouse_help_visible);
    assert!(!app.overlays.help_visible);
}

#[test]
fn korean_q_key_closes_help_overlay() {
    let mut app = app_playing(1, 0);
    app.overlays.help_visible = true;
    assert!(app.update(Msg::Key(key(KeyCode::Char('ㅂ')))).is_empty());
    assert!(!app.overlays.help_visible);
}

#[test]
fn esc_closes_mouse_help_overlay() {
    let mut app = app_playing(1, 0);
    app.overlays.mouse_help_visible = true;
    assert!(app.update(Msg::Key(key(KeyCode::Esc))).is_empty());
    assert!(!app.overlays.mouse_help_visible);
}

#[test]
fn click_closes_help_overlay_before_buttons() {
    let mut app = app_playing(1, 0);
    app.overlays.help_visible = true;
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 1,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::VolUp),
    );
    assert!(app.update(Msg::MouseClick { col: 3, row: 1 }).is_empty());
    assert!(!app.overlays.help_visible);
    assert_eq!(app.playback.volume, 40);
}

#[test]
fn click_closes_mouse_help_overlay_before_buttons() {
    let mut app = app_playing(1, 0);
    app.overlays.mouse_help_visible = true;
    app.playback.volume = 40;
    app.register_mouse_button(
        Rect {
            x: 0,
            y: 1,
            width: 8,
            height: 1,
        },
        MouseTarget::Player(Action::VolUp),
    );
    assert!(app.update(Msg::MouseClick { col: 3, row: 1 }).is_empty());
    assert!(!app.overlays.mouse_help_visible);
    assert_eq!(app.playback.volume, 40);
}
