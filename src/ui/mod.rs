//! Top-level rendering: owns the screen layout and dispatches to the active view.

pub mod buttons;
pub mod views;

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::widgets::Block;

use crate::app::{App, Mode};
use crate::theme::ThemeRole as R;

pub fn render(frame: &mut Frame, app: &App) {
    app.clear_mouse_regions();
    let area = frame.area();
    frame.render_widget(
        Block::default().style(
            Style::default()
                .fg(app.theme.color(R::TextPrimary))
                .bg(app.theme.color(R::Background)),
        ),
        area,
    );
    match app.mode {
        Mode::Player => views::player::render(frame, app, area),
        Mode::Search => views::search::render(frame, app, area),
        Mode::Library => views::library::render(frame, app, area),
        Mode::Settings => views::settings::render(frame, app, area),
        Mode::Ai => views::ai::render(frame, app, area),
    }
    // The `?` cheat-sheet draws on top of whatever screen is active.
    if app.help_visible {
        views::help::render(frame, app, area);
    }
    // A keybinding-conflict warning (Keys tab) is modal — it sits above everything else.
    if let Some(conflict) = &app.key_conflict {
        views::settings::render_conflict(frame, app, area, conflict);
    }
    // The "reset all settings" confirmation is likewise modal.
    if app.confirm_reset_all {
        views::settings::render_confirm_reset(frame, app, area);
    }
    // Deleting downloaded files is modal and irreversible — drawn last so its buttons win.
    if app.confirm_delete_files.is_some() {
        views::library::render_confirm_delete(frame, app, area);
    }
}
