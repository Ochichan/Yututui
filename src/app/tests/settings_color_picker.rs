use super::*;
use crate::settings::{
    COLOR_PICKER_CHOICE_COUNT, ColorPickerSelection, SettingsTab, color_picker_choice,
};
use crate::theme::ThemeRole;

fn color_row(role: ThemeRole) -> usize {
    SettingsTab::Graphics
        .fields()
        .iter()
        .position(|field| *field == Field::ThemeColor(role))
        .unwrap()
}

fn color_settings(role: ThemeRole, value: &str) -> App {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    let row = color_row(role);
    let state = app.settings.as_mut().unwrap();
    state.tab = SettingsTab::Graphics;
    state.row = row;
    state.draft.theme.set_override(role, value).unwrap();
    app.theme = state.draft.theme.normalized();
    app
}

fn render(app: &App, width: u16, height: u16) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, app))
        .unwrap();
}

fn target_rect(app: &App, target: MouseTarget) -> ratatui::layout::Rect {
    app.hits
        .regions()
        .iter()
        .find(|region| region.target == target)
        .unwrap_or_else(|| panic!("missing target {target:?}"))
        .rect
}

#[test]
fn swatch_and_hex_are_separate_buttons_with_distinct_actions() {
    let role = ThemeRole::Accent;
    let row = color_row(role);
    let mut swatch_app = color_settings(role, "#123456");
    render(&swatch_app, 80, 40);
    let swatch = target_rect(&swatch_app, MouseTarget::SettingsColorSwatch(row));
    let hex = target_rect(&swatch_app, MouseTarget::SettingsActivate(row));
    assert_eq!(swatch.width, 2);
    assert_eq!(hex.width, 9);
    assert_eq!(swatch.y, hex.y);
    assert_eq!(hex.x, swatch.right() + 2);

    swatch_app.update(Msg::MouseClick {
        col: swatch.x,
        row: swatch.y,
        multi: false,
    });
    assert!(swatch_app.settings.as_ref().unwrap().color_picker.is_some());
    assert!(!swatch_app.settings.as_ref().unwrap().editing_text);

    let mut hex_app = color_settings(role, "#123456");
    render(&hex_app, 80, 40);
    let hex = target_rect(&hex_app, MouseTarget::SettingsActivate(row));
    hex_app.update(Msg::MouseClick {
        col: hex.x,
        row: hex.y,
        multi: false,
    });
    assert!(hex_app.settings.as_ref().unwrap().editing_text);
    assert!(hex_app.settings.as_ref().unwrap().color_picker.is_none());
}

#[test]
fn paired_swatch_double_click_cannot_act_on_the_newly_opened_picker() {
    let role = ThemeRole::Accent;
    let mut app = color_settings(role, "#123456");
    render(&app, 80, 24);
    let row = color_row(role);
    let swatch = target_rect(&app, MouseTarget::SettingsColorSwatch(row));

    app.update(Msg::MouseClick {
        col: swatch.x,
        row: swatch.y,
        multi: false,
    });
    assert!(app.settings.as_ref().unwrap().color_picker.is_some());
    assert_eq!(
        app.interaction.color_picker_click,
        Some((swatch.x, swatch.y))
    );

    render(&app, 80, 24);
    app.update(Msg::MouseDoubleClick {
        col: swatch.x,
        row: swatch.y,
    });
    assert!(app.settings.as_ref().unwrap().color_picker.is_some());
    assert_eq!(app.theme.effective_hex(role), "#123456");
    assert!(app.interaction.color_picker_click.is_none());
}

#[test]
fn space_opens_picker_enter_is_lossless_and_enter_still_edits_hex() {
    let role = ThemeRole::Accent;
    let mut app = color_settings(role, "#123456");

    app.update(Msg::Key(key(KeyCode::Char(' '))));
    let picker = app
        .settings
        .as_ref()
        .unwrap()
        .color_picker
        .as_ref()
        .unwrap();
    assert_eq!(picker.selection(), ColorPickerSelection::Current);
    assert_eq!(picker.current(), "#123456");
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.theme.effective_hex(role), "#123456");
    assert!(app.settings.as_ref().unwrap().color_picker.is_none());

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.settings.as_ref().unwrap().editing_text);
    for _ in 0..7 {
        app.update(Msg::Key(key(KeyCode::Backspace)));
    }
    for ch in "#A1B2C3".chars() {
        app.update(Msg::Key(key(KeyCode::Char(ch))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.theme.effective_hex(role), "#A1B2C3");
    assert!(!app.settings.as_ref().unwrap().editing_text);
}

#[test]
fn keyboard_navigation_applies_or_cancels_without_leaking() {
    let role = ThemeRole::Accent;
    let mut app = color_settings(role, "#123456");
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .color_picker
            .as_ref()
            .unwrap()
            .selection(),
        ColorPickerSelection::Choice(0)
    );
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert_eq!(app.theme.effective_hex(role), "#123456");

    app.update(Msg::Key(key(KeyCode::Char(' '))));
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.theme.effective_hex(role), "none");
    assert!(app.settings.as_ref().unwrap().color_picker.is_none());
}

#[test]
fn full_popup_registers_surface_current_and_all_palette_buttons() {
    let mut app = color_settings(ThemeRole::Accent, "#123456");
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    render(&app, 80, 24);

    let regions = app.hits.regions();
    assert!(
        regions
            .iter()
            .any(|region| { region.target == MouseTarget::SettingsColorPickerSurface })
    );
    assert!(
        regions
            .iter()
            .any(|region| { region.target == MouseTarget::SettingsColorPickerCurrent })
    );
    assert_eq!(
        regions
            .iter()
            .filter(|region| matches!(region.target, MouseTarget::SettingsColorPickerChoice(_)))
            .count(),
        COLOR_PICKER_CHOICE_COUNT
    );
}

#[test]
fn mini_narrow_popup_stays_visible_and_scrolls_to_the_selection() {
    let mut app = color_settings(ThemeRole::Accent, "#123456");
    let row = app.settings.as_ref().unwrap().row;
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    render(&app, 30, 10);
    assert_eq!(app.bridges.ui_tier.get(), crate::ui::layout::UiTier::Mini);
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|region| region.target == MouseTarget::SettingsColorPickerSurface)
    );
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .color_picker
            .as_ref()
            .unwrap()
            .columns(),
        8
    );

    for _ in 0..10 {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    // A typeable global must be swallowed while the mode-owned Mini modal is open.
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Settings);
    assert_eq!(app.settings.as_ref().unwrap().row, row);
    render(&app, 30, 10);
    let selection = app
        .settings
        .as_ref()
        .unwrap()
        .color_picker
        .as_ref()
        .unwrap()
        .selection();
    let ColorPickerSelection::Choice(index) = selection else {
        panic!("navigation should enter the grid");
    };
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|region| { region.target == MouseTarget::SettingsColorPickerChoice(index) })
    );
}

#[test]
fn mouse_apply_outside_cancel_and_modal_inputs_never_reach_settings() {
    let role = ThemeRole::Accent;
    let mut app = color_settings(role, "#123456");
    let settings_row = app.settings.as_ref().unwrap().row;
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    render(&app, 80, 24);

    app.update(Msg::MouseScroll {
        up: false,
        col: 0,
        row: 0,
        ctrl: true,
    });
    assert!(matches!(
        app.settings
            .as_ref()
            .unwrap()
            .color_picker
            .as_ref()
            .unwrap()
            .selection(),
        ColorPickerSelection::Choice(_)
    ));
    app.update(Msg::MouseDrag { col: 0, row: 0 });
    app.update(Msg::MouseRightClick { col: 0, row: 0 });
    assert!(app.overlays.context_menu.is_none());
    assert_eq!(app.settings.as_ref().unwrap().row, settings_row);

    app.update(Msg::MouseClick {
        col: 0,
        row: 0,
        multi: false,
    });
    assert!(app.settings.as_ref().unwrap().color_picker.is_none());
    assert_eq!(app.theme.effective_hex(role), "#123456");
    app.update(Msg::MouseDoubleClick { col: 0, row: 0 });
    assert_eq!(app.mode, Mode::Settings, "paired double click is consumed");
    assert_eq!(app.settings.as_ref().unwrap().row, settings_row);

    app.update(Msg::Key(key(KeyCode::Char(' '))));
    render(&app, 80, 24);
    let choice = 2;
    let rect = target_rect(&app, MouseTarget::SettingsColorPickerChoice(choice));
    app.update(Msg::MouseClick {
        col: rect.x,
        row: rect.y,
        multi: false,
    });
    assert!(app.settings.as_ref().unwrap().color_picker.is_none());
    assert_eq!(
        app.theme.effective_hex(role),
        color_picker_choice(choice).unwrap().value()
    );
    assert_eq!(app.interaction.color_picker_click, Some((rect.x, rect.y)));
    app.update(Msg::MouseDoubleClick {
        col: rect.x,
        row: rect.y,
    });
    assert_eq!(app.mode, Mode::Settings, "paired double click is consumed");
    assert!(app.interaction.color_picker_click.is_none());
}
