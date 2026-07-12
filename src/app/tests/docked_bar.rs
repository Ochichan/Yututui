//! Docked control box (Bottom player-bar position): the transport must be live on every
//! screen that shows the box, and the legacy Top position must leave other screens exactly
//! as they were — no rows reserved, no player controls clickable.

use super::*;
use crate::config::PlayerBarPosition;

fn render_at_size(app: &App, w: u16, h: u16) {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
}

#[test]
fn bottom_mode_docks_player_controls_on_other_screens() {
    let mut app = app_playing(2, 0);
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.mode = Mode::Search;
    render_at_size(&app, 80, 24);

    let buttons = app.hits.regions();
    for target in [
        MouseTarget::Player(Action::TogglePause),
        MouseTarget::Player(Action::PrevTrack),
        MouseTarget::Player(Action::NextTrack),
        MouseTarget::VolumeArea,
        MouseTarget::Player(Action::ToggleShuffle),
        MouseTarget::Player(Action::CycleRepeat),
        MouseTarget::EqMenu,
    ] {
        let rect = buttons
            .iter()
            .find(|b| b.target == target)
            .unwrap_or_else(|| panic!("{target:?} not registered on Search"))
            .rect;
        // The box sits just above the footer: separator + 4 control rows + help + border.
        assert!(
            rect.y >= 24 - 7,
            "{target:?} at y={} — expected inside the docked box",
            rect.y
        );
    }
    // Click-to-seek is live from the docked seekbar too.
    assert!(app.hits.seekbar_rect().is_some());
}

#[test]
fn top_mode_keeps_other_screens_free_of_player_controls() {
    let mut app = app_playing(2, 0);
    app.config.player_bar_position = Some(PlayerBarPosition::Top); // legacy layout
    app.mode = Mode::Search;
    render_at_size(&app, 80, 24);

    let buttons = app.hits.regions();
    assert!(
        !buttons
            .iter()
            .any(|b| matches!(b.target, MouseTarget::Player(_) | MouseTarget::VolumeArea)),
        "legacy Top layout must not register player controls on Search"
    );
    assert!(app.hits.seekbar_rect().is_none());
}

#[test]
fn docked_transport_clicks_dispatch_from_other_screens() {
    let mut app = app_playing(2, 0);
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.mode = Mode::Library;
    render_at_size(&app, 80, 24);

    let rect = app
        .hits
        .rect_of_target(MouseTarget::Player(Action::TogglePause))
        .expect("pause button registered on Library");
    let paused_before = app.playback.paused;
    let mut cmds = app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
        multi: false,
    });
    assert!(
        matches!(
            cmds.as_slice(),
            [cmd] if matches!(
                cmd.player_command(),
                Some(PlayerCmd::SetProperty { name, value })
                    if name == "pause" && value == &serde_json::Value::Bool(!paused_before)
            )
        ),
        "docked pause click must reach the player"
    );
    assert_eq!(
        app.playback.paused, paused_before,
        "reducer state must wait for player admission"
    );
    admit_player_transition(&mut app, &mut cmds);
    assert_ne!(app.playback.paused, paused_before);
}

#[test]
fn wheel_over_docked_volume_cluster_adjusts_volume_on_library() {
    let mut app = app_playing(2, 0);
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.playback.volume = 40;
    app.mode = Mode::Library;
    render_at_size(&app, 80, 24);

    let rect = app
        .hits
        .rect_of_target(MouseTarget::VolumeArea)
        .expect("volume cluster registered on Library");
    let mut cmds = app.update(Msg::MouseScroll {
        up: true,
        col: rect.x + 1,
        row: rect.y,
        ctrl: false,
    });
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(45)))
    ));
    assert_eq!(
        app.playback.volume, 40,
        "reducer state must wait for player admission"
    );
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.playback.volume, 45, "wheel over the cluster nudges");

    // Off the cluster the wheel still scrolls the list (no volume change).
    app.update(Msg::MouseScroll {
        up: true,
        col: 5,
        row: 5,
        ctrl: false,
    });
    assert_eq!(app.playback.volume, 45, "list wheel must not nudge volume");
}

#[test]
fn queue_pos_click_from_other_screen_follows_to_player() {
    let mut app = app_playing(3, 1);
    app.config.player_bar_position = Some(PlayerBarPosition::Bottom);
    app.mode = Mode::Search;
    render_at_size(&app, 80, 24);

    let rect = app
        .hits
        .rect_of_target(MouseTarget::QueuePos)
        .expect("queue position label registered on Search");
    app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
        multi: false,
    });
    // The queue window lives on the Player screen — the click navigates there and opens it,
    // never an invisible popup over Search.
    assert_eq!(app.mode, Mode::Player);
    assert!(app.queue_popup.open);
}

#[test]
fn settings_shows_player_bar_position_select() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert_eq!(app.mode, Mode::Settings);
    let st = app.settings.as_ref().expect("settings open");
    assert!(
        st.fields().contains(&Field::PlayerBarPosition),
        "General tab offers the player-bar position select"
    );
    assert_eq!(st.draft.player_bar_position, PlayerBarPosition::Bottom);
}

#[test]
fn cycling_player_bar_position_previews_live_and_persists_on_close() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    focus_settings_field(&mut app, Field::PlayerBarPosition);
    app.update(Msg::Key(key(KeyCode::Right)));
    // Draft previews live: the whole UI relocates the bar before the value is committed.
    assert_eq!(app.player_bar_position(), PlayerBarPosition::Top);
    assert!(!app.control_box_active());
    // Closing Settings commits the draft into config.
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(
        app.config.player_bar_position, None,
        "Settings config must wait for player admission"
    );
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.config.player_bar_position, Some(PlayerBarPosition::Top));
    assert_eq!(
        app.config.effective_player_bar_position(),
        PlayerBarPosition::Top
    );
}

#[test]
fn collapse_toggle_reclaims_rows_and_persists() {
    let mut app = app_playing(2, 0);
    // Library, not Search: the search input owns typeable keys, so `B` would be text there
    // (the footer click still works — same rule as every typeable global like `A`).
    app.mode = Mode::Library; // default Bottom layout
    render_at_size(&app, 80, 24);
    assert!(
        app.hits
            .rect_of_target(MouseTarget::Global(Action::ToggleControlBox))
            .is_some(),
        "footer offers the ▼ toggle on non-Player screens"
    );

    // `B` collapses: the box's rows and controls vanish from non-Player screens…
    let cmds = app.update(Msg::Key(key(KeyCode::Char('B'))));
    assert!(
        cmds.iter().any(|c| matches!(c, Cmd::Persist(_))),
        "collapse persists like any preference"
    );
    assert!(app.config.control_box_collapsed());
    assert!(!app.control_box_active());
    render_at_size(&app, 80, 24);
    assert!(
        !app.hits
            .regions()
            .iter()
            .any(|b| matches!(b.target, MouseTarget::Player(_))),
        "collapsed box must not take clicks"
    );
    assert!(app.hits.seekbar_rect().is_none());
    // …but the Player screen always keeps its controls.
    app.mode = Mode::Player;
    assert!(app.control_box_active());
    render_at_size(&app, 80, 24);
    assert!(app.hits.seekbar_rect().is_some());

    // And `B` again expands.
    app.mode = Mode::Library;
    app.update(Msg::Key(key(KeyCode::Char('B'))));
    assert!(!app.config.control_box_collapsed());
    assert!(app.control_box_active());
}

#[test]
fn collapse_toggle_closes_docked_dropdowns() {
    let mut app = app_playing(1, 0);
    app.mode = Mode::Library;
    app.dropdowns.eq_open = true;
    app.dropdowns.streaming_open = true;

    app.update(Msg::Key(key(KeyCode::Char('B'))));

    assert!(app.config.control_box_collapsed());
    assert!(!app.dropdowns.eq_open);
    assert!(!app.dropdowns.streaming_open);
}

#[test]
fn status_message_renders_once_when_docked_bar_is_active() {
    for mode in [Mode::Search, Mode::Library, Mode::Ai] {
        let mut app = app_playing(1, 0);
        app.mode = mode;
        app.set_status_info("Docked toast");

        let buf = render_app_buffer(&app, 100, 24);
        let occurrences = (0..buf.area.height)
            .filter(|&y| buffer_row(&buf, y).contains("Docked toast"))
            .count();

        assert_eq!(occurrences, 1, "{mode:?} should render the status once");
    }
}

#[test]
fn collapsed_screen_matches_the_legacy_top_layout() {
    let mut collapsed = app_playing(2, 0);
    collapsed.config.control_box_collapsed = Some(true);
    collapsed.mode = Mode::Library;
    let mut top = app_playing(2, 0);
    top.config.player_bar_position = Some(PlayerBarPosition::Top);
    top.mode = Mode::Library;
    // Byte-identity except the footer (which carries the ▲ affordance in Bottom mode):
    // the body must reclaim every reserved row exactly.
    let buf_collapsed = render_app_buffer(&collapsed, 80, 24);
    let buf_top = render_app_buffer(&top, 80, 24);
    for y in 0..22 {
        let row =
            |b: &ratatui::buffer::Buffer| (0..80).map(|x| b[(x, y)].symbol()).collect::<String>();
        assert_eq!(
            row(&buf_collapsed),
            row(&buf_top),
            "collapsed body row {y} must match the legacy layout"
        );
    }
}

#[test]
fn footer_toggle_hidden_on_player_and_in_top_mode() {
    let mut app = app_playing(2, 0);
    render_at_size(&app, 80, 24); // Player, Bottom
    assert!(
        app.hits
            .rect_of_target(MouseTarget::Global(Action::ToggleControlBox))
            .is_none(),
        "the Player screen's box is not collapsible"
    );
    app.config.player_bar_position = Some(PlayerBarPosition::Top);
    app.mode = Mode::Search;
    render_at_size(&app, 80, 24);
    assert!(
        app.hits
            .rect_of_target(MouseTarget::Global(Action::ToggleControlBox))
            .is_none(),
        "the legacy Top footer stays byte-identical"
    );
}

#[test]
fn eq_dropdown_drops_up_from_the_docked_status_line() {
    let mut app = app_playing(2, 0);
    app.mode = Mode::Search; // default Bottom: status line near the screen bottom
    render_at_size(&app, 80, 24);
    let anchor = app
        .hits
        .rect_of_target(MouseTarget::EqMenu)
        .expect("eq: registered in the docked box");
    app.update(Msg::MouseClick {
        col: anchor.x,
        row: anchor.y,
        multi: false,
    });
    assert!(app.dropdowns.eq_open);
    render_at_size(&app, 80, 24);
    let first_preset = app
        .hits
        .regions()
        .iter()
        .filter(|b| matches!(b.target, MouseTarget::EqSelect(_)))
        .map(|b| b.rect.y)
        .max()
        .expect("dropdown rows registered");
    assert!(
        first_preset < anchor.y,
        "menu rows (bottom at y={first_preset}) must open above the low anchor (y={})",
        anchor.y
    );
}

#[test]
fn returning_to_player_replays_the_title_intro_only_when_enabled() {
    // Flags off (the default test config): the off-mode byte-identity contract — a mode
    // switch must arm nothing.
    let mut app = app_playing(2, 0);
    app.mode = Mode::Library;
    app.update(Msg::Resize); // pump detect_fx with mode=Library as the anchor
    app.update(Msg::Key(ctrl(KeyCode::Char('h')))); // Home → Player
    assert_eq!(app.mode, Mode::Player);
    assert!(
        app.fx.track_intro.is_none(),
        "animations off: no intro may arm"
    );

    // Master + track_intro on: returning to Player replays the cascade.
    let mut app = app_playing(2, 0);
    app.config.animations.master = true;
    app.config.animations.track_intro = true;
    app.update(Msg::Resize); // consume the initial track-change arm
    app.fx.track_intro = None;
    app.mode = Mode::Library;
    app.update(Msg::Resize);
    app.fx.track_intro = None;
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(
        app.fx.track_intro.is_some(),
        "animations on: returning to Player replays the title intro"
    );
}

#[test]
fn art_geometry_moves_request_a_native_clear() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);
    // Seeding the key (first sync after launch) must not clear — nothing moved yet.
    app.sync_art_geometry();
    assert!(!app.take_clear_before_draw(), "seeding must not clear");
    // The lyrics panel re-flows the centered group.
    app.lyrics.visible = !app.lyrics.visible;
    app.sync_art_geometry();
    assert!(
        app.take_clear_before_draw(),
        "lyrics toggle moves the art band"
    );
    // Moving the bar between Top and Bottom relocates the whole filler.
    app.config.player_bar_position = Some(PlayerBarPosition::Top);
    app.sync_art_geometry();
    assert!(
        app.take_clear_before_draw(),
        "bar-position change moves the art band"
    );
}

#[test]
fn resize_with_native_art_requests_a_clear() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);
    app.update(Msg::Resize);
    assert!(
        app.take_clear_before_draw(),
        "a resize moves the centered band with the grid"
    );
    assert!(!app.take_clear_before_draw(), "clear request is one-shot");
}

fn focus_settings_field(app: &mut App, field: Field) {
    let row = app
        .settings
        .as_ref()
        .expect("settings open")
        .fields()
        .iter()
        .position(|f| *f == field)
        .expect("field exists in current settings tab");
    for _ in 0..row {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
}
