//! Top-level rendering: owns the screen layout and dispatches to the active view.

pub mod buttons;
pub mod views;

use ratatui::Frame;

use crate::app::{App, Mode};

pub fn render(frame: &mut Frame, app: &App) {
    app.clear_mouse_regions();
    let area = frame.area();
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
}
