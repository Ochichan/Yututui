//! Miniplayer tier (terminal below `ui::layout::{MINI_MIN_H, MINI_MIN_W}`): the whole UI
//! becomes title/seek/transport/status; keys route to the Player context regardless of the
//! retained mode; hidden screens must neither eat input nor take clicks they can't show.

use super::*;
use crate::ui::layout::UiTier;

fn render_mini(app: &App, w: u16, h: u16) {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, app)).unwrap();
}

#[test]
fn tiny_frames_render_the_miniplayer_without_panic() {
    for (w, h) in [(28, 8), (60, 10), (31, 24), (32, 3), (10, 2)] {
        let mut app = app_playing(2, 0);
        app.mode = Mode::Search; // retained mode is irrelevant below the threshold
        render_mini(&app, w, h);
        assert_eq!(app.bridges.ui_tier.get(), UiTier::Mini, "{w}x{h}");
        // No chrome: neither nav tabs nor the footer help cluster exist.
        assert!(
            !app.hits
                .regions()
                .iter()
                .any(|b| matches!(b.target, MouseTarget::Nav(_) | MouseTarget::MouseHelp)),
            "{w}x{h}: mini renders no nav/footer chrome"
        );
        // The art rect is never set (no art in mini).
        assert!(app.art.rect.get().is_none());
    }
}

#[test]
fn mini_keeps_the_transport_clickable() {
    let mut app = app_playing(2, 0);
    // Even in the legacy Top layout — mini replaces the whole UI either way.
    app.config.player_bar_position = Some(crate::config::PlayerBarPosition::Top);
    app.mode = Mode::Library;
    render_mini(&app, 28, 8);
    assert!(app.hits.seekbar_rect().is_some(), "click-to-seek lives");
    let rect = app
        .hits
        .rect_of_target(MouseTarget::Player(Action::TogglePause))
        .expect("pause registered in mini");
    let cmds = app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
    });
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::CyclePause)]
    ));
}

#[test]
fn mini_routes_keys_to_the_player_context() {
    let mut app = app_playing(2, 0);
    app.mode = Mode::Library;
    render_mini(&app, 28, 8); // sets the tier bridge
    // `.` is Player:NextTrack; on the full Library screen it would be a list key or a no-op.
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Player(PlayerCmd::Load(_)))),
        "Player transport keys must work under the miniplayer: {}",
        cmds.len()
    );
    // Growing back restores normal routing.
    render_mini(&app, 80, 24);
    assert_eq!(app.bridges.ui_tier.get(), UiTier::Full);
}

#[test]
fn mini_entry_drops_search_input_focus() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('s')))); // Search, input focused
    assert_eq!(app.mode, Mode::Search);
    assert!(app.in_text_entry());
    render_mini(&app, 28, 8);
    app.update(Msg::Resize); // any event runs the tier sync
    assert!(
        !app.in_text_entry(),
        "the invisible search input must not keep eating keys"
    );
    // Typeable globals work again: `B` reaches the collapse toggle, not the input.
    let before = app.search.input.clone();
    app.update(Msg::Key(key(KeyCode::Char('B'))));
    assert_eq!(app.search.input, before, "B must not type into the input");
}

#[test]
fn mini_keeps_mode_owned_modals_operable() {
    let mut app = app_playing(1, 0);
    app.mode = Mode::Library;
    app.library_ui.create_input = Some("mix".to_owned());
    render_mini(&app, 28, 8);
    app.update(Msg::Resize);
    // The create-playlist popup renders mode-independently and its keys live inside the
    // Library handler — Esc must still dismiss it under the miniplayer.
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(
        app.library_ui.create_input.is_none(),
        "modal stays dismissable under the miniplayer"
    );
}

#[test]
fn mini_keeps_settings_import_dropdown_visible_and_keyed() {
    let mut app = app_playing(1, 0);
    app.open_settings();
    focus_settings_field(&mut app, SettingsTab::Accounts, Field::SpotifyImportMode);
    app.settings_activate();

    let buf = render_app_buffer(&app, 28, 8);
    assert_eq!(app.bridges.ui_tier.get(), UiTier::Mini);
    assert!(
        buffer_contains(&buf, "Strict playlist"),
        "settings import dropdown must render over the miniplayer"
    );

    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(
        app.settings.as_ref().unwrap().spotify_import_mode_dropdown,
        Some(crate::config::SpotifyImportMode::StrictPlaylist.index())
    );
}

#[test]
fn tier_flip_moves_the_art_geometry_key() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Sixel);
    render_mini(&app, 80, 24);
    app.sync_art_geometry(); // seed at Full
    let _ = app.take_clear_before_draw();
    render_mini(&app, 28, 8); // shrink into mini
    app.sync_art_geometry();
    assert!(
        app.take_clear_before_draw(),
        "entering mini hides native art — the parked bytes need one clear"
    );
}
