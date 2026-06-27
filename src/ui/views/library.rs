//! The library view: two tabs (Favorites / History), each a selectable list of tracks.
//!
//! Rows are formatted per frame — both lists are bounded (≤500 / ≤1000) and only the
//! visible window is laid out by ratatui, so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::api::Song;
use crate::app::{App, LibraryTab};
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Library ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // list
        Constraint::Length(1), // help
    ])
    .split(inner);

    render_tabs(frame, app, rows[0]);
    render_list(frame, app, rows[2]);

    buttons::render_help_button(frame, app, rows[3], Alignment::Left);
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let tab = |t: LibraryTab, n: usize| {
        let label = format!(" {} ({n}) ", t.label());
        if app.library_tab == t {
            Span::styled(
                label,
                Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(label, Style::default().fg(Color::DarkGray))
        }
    };
    let line = Line::from(vec![
        tab(LibraryTab::Favorites, app.library.favorites.len()),
        Span::raw("  "),
        tab(LibraryTab::History, app.library.history.len()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<&Song> = match app.library_tab {
        LibraryTab::Favorites => app.library.favorites.iter().collect(),
        LibraryTab::History => app.library.history.iter().collect(),
    };

    if rows.is_empty() {
        let msg = match app.library_tab {
            LibraryTab::Favorites => "No favorites yet — press f on a track to save it.",
            LibraryTab::History => "No history yet — play something.",
        };
        frame.render_widget(Paragraph::new(Line::from(msg).fg(Color::DarkGray)), area);
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .map(|s| {
            let line = if s.duration.is_empty() {
                format!("{} — {}", s.title, s.artist)
            } else {
                format!("{} — {}  ({})", s.title, s.artist, s.duration)
            };
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.library_selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}
