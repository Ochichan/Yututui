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
    let cmds = app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
    });
    assert!(
        matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::CyclePause)]),
        "docked pause click must reach the player"
    );
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
    let cmds = app.update(Msg::MouseScroll {
        up: true,
        col: rect.x + 1,
        row: rect.y,
        ctrl: false,
    });
    assert_eq!(app.playback.volume, 45, "wheel over the cluster nudges");
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(45))]
    ));

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
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.config.player_bar_position, Some(PlayerBarPosition::Top));
    assert_eq!(
        app.config.effective_player_bar_position(),
        PlayerBarPosition::Top
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
