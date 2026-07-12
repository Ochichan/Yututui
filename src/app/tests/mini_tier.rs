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
        multi: false,
    });
    assert!(matches!(
        cmds.as_slice(),
        [cmd] if matches!(
            cmd.player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "pause" && value == &serde_json::Value::Bool(true)
        )
    ));
}

#[test]
fn ultra_narrow_mini_preserves_the_primary_transport_target_in_bounds() {
    let app = app_playing(2, 0);
    render_mini(&app, 10, 3);
    let rect = app
        .hits
        .rect_of_target(MouseTarget::Player(Action::TogglePause))
        .expect("pause remains the primary narrow-width control");
    assert!(rect.width > 0);
    assert!(rect.x.saturating_add(rect.width) <= 10);
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
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(command, PlayerCmd::Load(_))),
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
    // The very first event after render must run tier hygiene before it is routed.
    let before = app.search.input.clone();
    app.update(Msg::Key(key(KeyCode::Char('B'))));
    assert!(
        !app.in_text_entry(),
        "the invisible search input must not keep eating keys"
    );
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
fn mini_entry_closes_the_search_results_filter() {
    let mut app = app_playing(2, 0);
    app.mode = Mode::Search;
    app.search_filter.open = true; // rendered only by the (suppressed) Search view
    render_mini(&app, 28, 8);
    app.update(Msg::Resize);
    assert!(
        !app.search_filter.open,
        "an unrendered filter popup must not keep capturing keys"
    );
}

#[test]
fn mini_hygiene_is_level_triggered_and_covers_the_ai_input() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('g')))); // DJ Gem, input focused
    assert_eq!(app.mode, Mode::Ai);
    render_mini(&app, 28, 8);
    app.update(Msg::Key(key(KeyCode::Char('?'))));
    assert!(!app.in_text_entry(), "the hidden DJ Gem input must blur");
    assert!(
        app.overlays.help_visible,
        "the first global key must route after Mini hygiene"
    );
    app.overlays.help_visible = false;
    // Navigating inside mini re-focuses the search input — the hygiene must re-fire on the
    // next event (level-triggered), not only on the tier transition.
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    render_mini(&app, 28, 8);
    app.update(Msg::Resize);
    assert!(
        !app.in_text_entry(),
        "re-focused inputs must blur again while the mini tier holds"
    );
}

#[test]
fn every_mode_uses_mini_until_the_bottom_layout_has_a_content_row() {
    for mode in [
        Mode::Player,
        Mode::Search,
        Mode::Library,
        Mode::Settings,
        Mode::Ai,
    ] {
        let mut app = app_playing(2, 0);
        app.mode = mode;
        render_mini(&app, 32, 13);
        assert_eq!(app.bridges.ui_tier.get(), UiTier::Mini, "{mode:?} at 32x13");
        render_mini(&app, 32, 14);
        assert_eq!(app.bridges.ui_tier.get(), UiTier::Full, "{mode:?} at 32x14");
    }
}

#[test]
fn queue_pos_click_opens_the_queue_in_mini_even_with_the_bar_hidden() {
    let mut app = app_playing(3, 1);
    // Top layout: control_box_active() is false, but the mini renders the status line and
    // the queue window itself, so the N/M label must work.
    app.config.player_bar_position = Some(crate::config::PlayerBarPosition::Top);
    app.mode = Mode::Search;
    render_mini(&app, 28, 8);
    let rect = app
        .hits
        .rect_of_target(MouseTarget::QueuePos)
        .expect("queue position label rendered in mini");
    app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
        multi: false,
    });
    assert!(app.queue_popup.open, "N/M opens the queue window in place");
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
