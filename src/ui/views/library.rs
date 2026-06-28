//! The library view: saved tracks, recent history, and local download-folder audio.
//!
//! Rows are formatted per frame. The underlying collections are bounded and only the
//! visible window is laid out by ratatui, so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::app::{App, LibraryTab, MouseTarget};
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Library ")
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // nav bar
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // list
        Constraint::Length(1), // help
    ])
    .split(inner);

    buttons::render_nav(frame, app, rows[0]);
    render_tabs(frame, app, rows[1]);
    render_list(frame, app, rows[3]);

    buttons::render_help_button(frame, app, rows[4]);
}

fn render_tabs(frame: &mut Frame, app: &App, area: Rect) {
    // Each tab is a click target. Walk left to right tracking the x cursor with the same
    // cell-width math ratatui lays the spans out with, so the hit rects line up exactly.
    let mut spans = Vec::new();
    let mut x = area.x;
    for (i, t) in LibraryTab::ALL.iter().copied().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x = x.saturating_add(2);
        }
        let label = format!(" {} ({}) ", t.label(), app.library_count(t));
        let w = buttons::text_width(&label);
        app.register_mouse_button(
            Rect { x, y: area.y, width: w, height: 1 },
            MouseTarget::LibraryTab(t),
        );
        x = x.saturating_add(w);
        let style = if app.library_tab == t {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextMuted)
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.library_rows();

    if rows.is_empty() {
        let msg = match app.library_tab {
            LibraryTab::All => "No library tracks yet — play, favorite, or download something.",
            LibraryTab::Favorites => "No favorites yet — press f on a track to save it.",
            LibraryTab::History => "No history yet — play something.",
            LibraryTab::Downloads => "No downloaded tracks found in the download folder.",
        };
        frame.render_widget(Paragraph::new(Line::from(msg).style(app.theme.style(R::TextMuted))), area);
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
            ListItem::new(line).style(app.theme.style(R::TextPrimary))
        })
        .collect();

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD),
        )
        .style(app.theme.style(R::TextPrimary))
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.library_selected.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
    // Each visible row is a click target: single-click selects, double-click plays.
    buttons::register_list_rows(app, area, state.offset(), rows.len(), Some);
}
