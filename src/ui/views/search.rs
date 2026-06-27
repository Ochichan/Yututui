//! The search view: a query input box above a results list.
//!
//! The input box and the list share focus ([`SearchFocus`]); the focused one gets the
//! accent color. Rows are formatted here, per frame — the result set is small (one
//! page of songs) so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::app::{App, SearchFocus};
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(3), // input box
        Constraint::Min(0),    // results
        Constraint::Length(1), // help
    ])
    .split(inner);

    render_input(frame, app, rows[0]);
    render_results(frame, app, rows[1]);

    buttons::render_help_button(frame, app, rows[2]);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.search_focus == SearchFocus::Input;
    let border = if focused { R::BorderFocused } else { R::BorderMuted };
    // Make it obvious when we're not signed in (anonymous = search + public play only).
    let title = if app.authenticated {
        " Search "
    } else {
        " Search · anonymous "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));

    // A trailing block cursor while the input has focus.
    let text = if focused {
        format!("{}\u{2588}", app.search_input)
    } else {
        app.search_input.clone()
    };
    frame.render_widget(Paragraph::new(text).style(app.theme.style(R::TextPrimary)).block(block), area);
}

fn render_results(frame: &mut Frame, app: &App, area: Rect) {
    if app.searching {
        let msg = Line::from("Searching…").style(app.theme.style(R::Warning));
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
            ListItem::new(line).style(app.theme.style(R::TextPrimary))
        })
        .collect();

    let highlight = if focused {
        Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.color(R::SelectionInactiveFg))
            .bg(app.theme.color(R::SelectionInactiveBg))
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .style(app.theme.style(R::TextPrimary))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !app.search_results.is_empty() {
        state.select(Some(app.search_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}
