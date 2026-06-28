//! The search view: a query input box above a results list.
//!
//! The input box and the list share focus ([`SearchFocus`]); the focused one gets the
//! accent color. Rows are formatted here, per frame — the result set is small (one
//! page of songs) so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::app::{App, MouseTarget, SearchFocus};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The nav strip rides the top border line itself; `render_nav` overlays only the cells
    // its text covers, so the border keeps drawing on either side of it.
    buttons::render_nav(
        frame,
        app,
        Rect { x: inner.x, y: area.y, width: inner.width, height: 1 },
    );

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
    // Reserve a themed Search button on the right; the input box takes the rest.
    let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(SEARCH_BTN_W)]).split(area);
    let input_area = cols[0];
    let button_area = cols[1];

    let focused = app.search_focus == SearchFocus::Input;
    let border = if focused { R::BorderFocused } else { R::BorderMuted };
    // Make it obvious when we're not signed in (anonymous = search + public play only).
    let title = if app.authenticated {
        t!(" Search ", " 검색 ")
    } else {
        t!(" Search · anonymous ", " 검색 · 익명 ")
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
    frame.render_widget(
        Paragraph::new(text).style(app.theme.style(R::TextPrimary)).block(block),
        input_area,
    );
    render_search_button(frame, app, button_area);
}

/// Width of the Search button cluster (border + " Search ").
const SEARCH_BTN_W: u16 = 10;

/// The themed Search button next to the input box: clicking it submits the query, the same
/// as pressing Enter. Bordered to match the input box; the whole cluster is the click rect.
fn render_search_button(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let label = Line::from(t!("Search", "검색"))
        .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(label), inner);
    app.register_mouse_button(area, MouseTarget::SearchSubmit);
}

fn render_results(frame: &mut Frame, app: &App, area: Rect) {
    // Record the viewport height so PageUp/PageDown can move by a screenful (see app::page_step).
    app.list_viewport_rows.set(area.height);

    if app.searching {
        let msg = Line::from(t!("Searching…", "검색 중…")).style(app.theme.style(R::Warning));
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
    // Each visible row is a click target: single-click selects, double-click plays.
    buttons::register_list_rows(app, area, state.offset(), app.search_results.len(), Some);
    // Scrollbar on the right border, tracking the cursor; hidden when results fit.
    buttons::render_list_scrollbar(
        frame,
        app,
        area,
        app.search_results.len(),
        app.search_selected,
        area.height as usize,
    );
}
