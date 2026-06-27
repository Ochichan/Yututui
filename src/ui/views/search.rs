//! The search view: a query input box above a results list.
//!
//! The input box and the list share focus ([`SearchFocus`]); the focused one gets the
//! magenta accent. Rows are formatted here, per frame — the result set is small (one
//! page of songs) so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::app::{App, SearchFocus};

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(3), // input box (bordered)
        Constraint::Min(0),    // results
        Constraint::Length(1), // help
    ])
    .split(area);

    render_input(frame, app, rows[0]);
    render_results(frame, app, rows[1]);

    let help = Line::from(app.help_footer()).fg(Color::DarkGray);
    frame.render_widget(Paragraph::new(help), rows[2]);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.search_focus == SearchFocus::Input;
    let accent = if focused { Color::Magenta } else { Color::DarkGray };
    // Make it obvious when we're not signed in (anonymous = search + public play only).
    let title = if app.authenticated {
        " Search "
    } else {
        " Search · anonymous "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent));

    // A trailing block cursor while the input has focus.
    let text = if focused {
        format!("{}\u{2588}", app.search_input)
    } else {
        app.search_input.clone()
    };
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn render_results(frame: &mut Frame, app: &App, area: Rect) {
    if app.searching {
        let msg = Line::from("Searching…").fg(Color::Yellow);
        frame.render_widget(Paragraph::new(msg), area);
        return;
    }

    let focused = app.search_focus == SearchFocus::Results;
    let items: Vec<ListItem> = app
        .search_results
        .iter()
        .map(|s| {
            let heart = if app.library.is_favorite(&s.video_id) { "♥ " } else { "" };
            let line = if s.duration.is_empty() {
                format!("{heart}{} — {}", s.title, s.artist)
            } else {
                format!("{heart}{} — {}  ({})", s.title, s.artist, s.duration)
            };
            ListItem::new(line)
        })
        .collect();

    let highlight = if focused {
        Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !app.search_results.is_empty() {
        state.select(Some(app.search_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}
