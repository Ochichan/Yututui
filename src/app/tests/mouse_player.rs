use super::*;

fn admit_mouse_seek(app: &mut App, cmds: &[Cmd]) {
    app.admit_player_intents_for_test(cmds);
}

#[test]
fn click_on_seekbar_previews_then_seeks_once_on_release() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    // Column 50 of a 100-wide bar → 50% of 200 s → ~100 s.
    let epoch = app.playback.position_epoch;
    let press = app.update(Msg::MouseClick {
        col: 50,
        row: 5,
        multi: false,
    });
    assert!(press.is_empty(), "press is preview-only");
    assert!(
        app.seekbar_preview_target()
            .is_some_and(|target| (target - 100.0).abs() < 1.0)
    );
    assert_eq!(
        app.playback.time_pos, None,
        "seek position must wait for player admission"
    );
    let cmds = app.update(Msg::MouseLeftUp);
    match cmds.as_slice() {
        [cmd] => match cmd.player_command() {
            Some(PlayerCmd::SeekAbsolute {
                seconds,
                precision: crate::player::SeekPrecision::InteractiveFast,
            }) => assert!((*seconds - 100.0).abs() < 1.0),
            _ => panic!("expected a SeekAbsolute cmd"),
        },
        _ => panic!("expected a SeekAbsolute cmd"),
    }
    admit_mouse_seek(&mut app, &cmds);
    assert!(
        app.playback
            .time_pos
            .is_some_and(|position| (position - 100.0).abs() < 1.0)
    );
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.seekbar_preview_target(), None);
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
    assert!(
        app.update(Msg::MouseClick {
            col: 50,
            row: 9,
            multi: false
        })
        .is_empty()
    ); // wrong row
    assert!(
        app.update(Msg::MouseClick {
            col: 200,
            row: 5,
            multi: false
        })
        .is_empty()
    ); // past the bar
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
    assert!(
        app.update(Msg::MouseClick {
            col: 50,
            row: 5,
            multi: false
        })
        .is_empty()
    );
}

#[test]
fn drag_on_seekbar_updates_preview_and_commits_only_the_final_cell() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    let epoch = app.playback.position_epoch;
    let press = app.update(Msg::MouseClick {
        col: 25,
        row: 5,
        multi: false,
    });
    assert!(press.is_empty());
    assert!(app.update(Msg::MouseDrag { col: 75, row: 9 }).is_empty());
    assert!(
        app.seekbar_preview_target()
            .is_some_and(|target| (target - 150.0).abs() < 1.0)
    );
    assert!(app.update(Msg::MouseDrag { col: 75, row: 9 }).is_empty());
    assert!(app.update(Msg::MouseDrag { col: 250, row: 5 }).is_empty());
    let cmds = app.update(Msg::MouseLeftUp);
    match cmds.as_slice() {
        [cmd] => match cmd.player_command() {
            Some(PlayerCmd::SeekAbsolute {
                seconds,
                precision: crate::player::SeekPrecision::InteractiveFast,
            }) => assert!((*seconds - 198.0).abs() < 1.0),
            _ => panic!("expected a clamped SeekAbsolute"),
        },
        _ => panic!("expected a clamped SeekAbsolute"),
    }
    admit_mouse_seek(&mut app, &cmds);
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert!(
        app.update(Msg::MouseDrag { col: 10, row: 5 }).is_empty(),
        "no scrub after release"
    );
}

#[test]
fn rejected_seekbar_release_clears_preview_without_false_position() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });

    assert!(
        app.update(Msg::MouseClick {
            col: 25,
            row: 5,
            multi: false,
        })
        .is_empty()
    );
    let first = app.update(Msg::MouseLeftUp);
    assert!(matches!(
        first.as_slice(),
        [cmd] if matches!(cmd.player_command(), Some(PlayerCmd::SeekAbsolute { seconds, .. }) if (*seconds - 50.0).abs() < 1.0)
    ));

    app.settle_mouse_seek_admission(false);
    assert_eq!(app.seekbar_preview_target(), None);
    assert_eq!(app.playback.time_pos, None);
    assert!(app.update(Msg::MouseDrag { col: 25, row: 9 }).is_empty());
}

#[test]
fn unrelated_seek_rejection_does_not_arm_a_mouse_retry() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });

    let press = app.update(Msg::MouseClick {
        col: 25,
        row: 5,
        multi: false,
    });
    assert!(press.is_empty());
    app.settle_mouse_seek_admission(false);
    assert!(app.seekbar_preview_target().is_some());
    assert!(app.update(Msg::MouseDrag { col: 25, row: 9 }).is_empty());
}

#[test]
fn duration_or_focus_change_cancels_scrub_without_seek() {
    let mut app = app_playing(1, 0);
    app.playback.duration = Some(200.0);
    app.hits.set_seekbar_rect(Rect {
        x: 0,
        y: 5,
        width: 100,
        height: 1,
    });
    assert!(
        app.update(Msg::MouseClick {
            col: 25,
            row: 5,
            multi: false
        })
        .is_empty()
    );
    assert!(app.seekbar_preview_target().is_some());
    assert!(app.update(PlayerMsg::Duration(None)).is_empty());
    assert_eq!(app.seekbar_preview_target(), None);
    assert!(app.update(Msg::MouseLeftUp).is_empty());

    app.playback.duration = Some(200.0);
    assert!(
        app.update(Msg::MouseClick {
            col: 25,
            row: 5,
            multi: false
        })
        .is_empty()
    );
    assert!(app.update(Msg::Focus(false)).is_empty());
    assert_eq!(app.seekbar_preview_target(), None);
    assert!(app.update(Msg::MouseLeftUp).is_empty());
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
    let cmds = app.update(Msg::MouseClick {
        col: 12,
        row: 4,
        multi: false,
    });
    assert!(!app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(true)
        )
    ));
    app.admit_player_intents_for_test(&cmds);
    assert!(app.playback.paused);

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
    let cmds = app.update(Msg::MouseClick {
        col: 25,
        row: 4,
        multi: false,
    });
    assert_eq!(app.playback.volume, 40);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(45)))
    ));
    app.admit_player_intents_for_test(&cmds);
    assert_eq!(app.playback.volume, 45);
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
    assert_eq!(app.playback.volume, 40);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(45)))
    ));
    app.admit_player_intents_for_test(&cmds);
    assert_eq!(app.playback.volume, 45);

    let cmds = app.update(Msg::MouseScroll {
        up: false,
        col: 25,
        row: 4,
        ctrl: false,
    });
    assert_eq!(app.playback.volume, 45);
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(40)))
    ));
    app.admit_player_intents_for_test(&cmds);
    assert_eq!(app.playback.volume, 40);
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
    let mut cmds = app.update(Msg::MouseClick {
        col: 3,
        row: 1,
        multi: false,
    });
    admit_player_transition(&mut app, &mut cmds);
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
    assert!(
        app.update(Msg::MouseClick {
            col: 4,
            row: 9,
            multi: false
        })
        .is_empty()
    );
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
    assert!(
        app.update(Msg::MouseClick {
            col: 20,
            row: 9,
            multi: false
        })
        .is_empty()
    );
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
    assert!(
        app.update(Msg::MouseClick {
            col: 3,
            row: 1,
            multi: false
        })
        .is_empty()
    );
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
    assert!(
        app.update(Msg::MouseClick {
            col: 3,
            row: 1,
            multi: false
        })
        .is_empty()
    );
    assert!(!app.overlays.mouse_help_visible);
    assert_eq!(app.playback.volume, 40);
}
