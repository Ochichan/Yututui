use super::*;

#[test]
fn settings_global_key_capture_rejects_player_overlap() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    for _ in 0..2 {
        app.update(Msg::Key(key(KeyCode::Tab))); // -> Hotkeys tab
    }
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);

    let row = crate::keymap::editable_entries()
        .iter()
        .position(|entry| *entry == (KeyContext::Global, Action::ToggleHelp))
        .expect("global help binding is editable");
    app.settings.as_mut().unwrap().row = row;
    app.update(Msg::Key(key(KeyCode::Enter))); // capture global.toggle_help
    assert_eq!(
        app.settings.as_ref().unwrap().capturing,
        Some((KeyContext::Global, Action::ToggleHelp))
    );

    app.update(Msg::Key(key(KeyCode::Char('.')))); // Player next-track owns `.`.
    let conflict = app
        .overlays
        .key_conflict
        .expect("global overlap should raise a conflict warning");
    assert_eq!(conflict.ctx, KeyContext::Player);
    assert_eq!(conflict.existing, Action::NextTrack);
    assert_eq!(conflict.chord, crate::keymap::parse_chord(".").unwrap());
}
