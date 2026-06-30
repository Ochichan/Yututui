//! Top-level rendering: owns the screen layout and dispatches to the active view.

pub mod anim;
pub mod buttons;
pub mod scroll;
pub mod text;
pub mod views;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Clear};

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
    // The About card draws on top too (clicking the `ytm-tui` brand or F1).
    if app.about_visible {
        views::about::render(frame, app, area);
    }
    // The "Why AI" card explains the last AI radio refill (`w`); also drawn on top.
    if app.why_ai_visible {
        views::why_ai::render(frame, app, area);
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
    if app.library_ui.confirm_delete.is_some() {
        views::library::render_confirm_delete(frame, app, area);
    }
}

pub fn popup_bg(app: &App) -> Color {
    match app.theme.color(R::Background) {
        Color::Reset => app.theme.color(R::TextInverse),
        bg => bg,
    }
}

pub fn popup_style(app: &App, role: R) -> Style {
    app.theme.style(role).bg(popup_bg(app))
}

pub fn render_popup_background(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(popup_bg(app))),
        area,
    );
}

pub fn seal_popup_background(frame: &mut Frame, app: &App, area: Rect) {
    let bg = popup_bg(app);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y))
                && cell.bg == Color::Reset
            {
                cell.set_bg(bg);
            }
        }
    }
}

pub fn mark_art_rows_for_popup(frame: &mut Frame, app: &App, popup: Rect) {
    let Some(art) = app.art.rect.get() else {
        return;
    };
    if art.intersection(popup).is_empty() {
        return;
    }
    if let Some(protocol) = app.art.protocol.borrow().as_ref() {
        protocol.mark_kitty_rows_for_redraw(art, popup, frame.buffer_mut());
    }
}
