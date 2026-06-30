//! The search view: a query input box above a results list.
//!
//! The input box and the list share focus ([`SearchFocus`]); the focused one gets the
//! accent color. Rows are formatted here, per frame — the result set is small (one
//! page of songs) so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget, ScrollSurface, SearchFocus, StatusKind};
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
        Rect {
            x: inner.x,
            y: area.y,
            width: inner.width,
            height: 1,
        },
    );

    let rows = Layout::vertical([
        Constraint::Length(2), // reserved top band (aligns with Settings/Library tab row + spacer)
        Constraint::Length(3), // input box
        Constraint::Min(0),    // results
        Constraint::Length(1), // help
    ])
    .split(inner);

    // A transient confirmation (e.g. "Added to queue: …" after enqueuing a search result while a
    // track is already playing) rides the otherwise-empty top band so it's visible without leaving
    // the search screen. It auto-clears after STATUS_TTL via the global StatusTick.
    if !app.status.text.is_empty() {
        let role = match app.status.kind {
            StatusKind::Error => R::Error,
            StatusKind::Info => R::Success,
        };
        frame.render_widget(
            Paragraph::new(
                Line::from(app.status.text.clone())
                    .style(app.theme.style(role))
                    .alignment(Alignment::Center),
            ),
            rows[0],
        );
    }

    render_input(frame, app, rows[1]);
    render_results(frame, app, rows[2]);

    buttons::render_help_button(frame, app, rows[3]);
    if app.dropdowns.search_source_open {
        render_source_dropdown(frame, app, inner);
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    // A source chip on the left, the input box in the middle, and a themed Search button on
    // the right. Enter and the button submit; the chip changes the provider.
    let cols = Layout::horizontal([
        Constraint::Length(SEARCH_SOURCE_W),
        Constraint::Min(0),
        Constraint::Length(SEARCH_BTN_W),
    ])
    .split(area);
    let source_area = cols[0];
    let input_area = cols[1];
    let button_area = cols[2];

    render_source_chip(frame, app, source_area);

    let focused = app.search.focus == SearchFocus::Input;
    let border = if focused {
        R::BorderFocused
    } else {
        R::BorderMuted
    };
    // Make it obvious when we're not signed in (anonymous = search + public play only).
    let title = if app.authenticated {
        if crate::i18n::is_korean() {
            format!(" 검색 · {} ", app.search.source.code())
        } else {
            format!(" Search · {} ", app.search.source.code())
        }
    } else {
        if crate::i18n::is_korean() {
            format!(" 검색 · 익명 · {} ", app.search.source.code())
        } else {
            format!(" Search · anonymous · {} ", app.search.source.code())
        }
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(app.theme.style(border))
        .style(app.theme.style(R::TextPrimary));

    // Ctrl+A selects the whole query: paint it with the selection colors. Otherwise show a
    // trailing block cursor while focused, or plain text when not.
    let para = if focused && app.search.select_all && !app.search.input.is_empty() {
        let hl = Style::default()
            .fg(app.theme.color(R::SelectionFg))
            .bg(app.theme.color(R::SelectionBg));
        Paragraph::new(Line::from(Span::styled(app.search.input.clone(), hl)))
    } else {
        let text = if focused {
            format!("{}\u{2588}", app.search.input)
        } else {
            app.search.input.clone()
        };
        Paragraph::new(text).style(app.theme.style(R::TextPrimary))
    };
    frame.render_widget(para.block(block), input_area);
    app.register_mouse_button(input_area, MouseTarget::SearchInput);
    render_search_button(frame, app, button_area);
}

/// Width of the left source chip cluster (border + `ALL▾`).
const SEARCH_SOURCE_W: u16 = 6;

/// Width of the Search button cluster (border + " Search ").
const SEARCH_BTN_W: u16 = 10;

/// The source chip to the left of the input box. Clicking it opens the provider dropdown.
fn render_source_chip(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let label = format!("{}▾", app.search.source.code());
    let chip = Line::from(label)
        .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(chip), inner);
    app.register_mouse_button(area, MouseTarget::SearchSourceMenu);
}

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
    app.bridges.list_viewport_rows.set(area.height);

    if app.search.searching {
        let msg = Line::from(t!("Searching…", "검색 중…")).style(app.theme.style(R::Warning));
        frame.render_widget(Paragraph::new(msg), area);
        return;
    }

    let focused = app.search.focus == SearchFocus::Results;
    let items: Vec<ListItem> = app
        .search
        .results
        .iter()
        .map(|s| {
            let title = app.display_title(s);
            let artist = app.display_artist(s);
            let heart = if app.library.is_favorite(&s.video_id) {
                "♥ "
            } else {
                ""
            };
            let source = format!("[{}] ", s.source.code());
            let line = if s.duration.is_empty() {
                format!("{source}{heart}{title} — {artist}")
            } else {
                format!("{source}{heart}{title} — {artist}  ({})", s.duration)
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

    // The wheel scrolls this viewport freely; the render keeps a keyboard-moved cursor on
    // screen with a margin (see `ui::scroll`). Pre-seed the offset so ratatui honors it; only
    // highlight the selection while it is actually visible, so the wheel can scroll past it.
    let len = app.search.results.len();
    let offset = app.bridges.search_scroll.resolve(
        app.search.selected.min(len.saturating_sub(1)),
        area.height,
        len,
        crate::ui::scroll::SCROLLOFF,
    );
    let mut state = ListState::default().with_offset(offset);
    if len > 0 {
        let sel = app.search.selected.min(len - 1);
        if (offset..offset + area.height as usize).contains(&sel) {
            state.select(Some(sel));
        }
    }
    frame.render_stateful_widget(list, area, &mut state);
    // Each visible row is a click target: single-click selects, double-click plays.
    buttons::register_list_rows(app, area, state.offset(), len, Some);
    // Scrollbar on the right border, tracking the viewport position; hidden when results fit.
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: area.right(),
            y: area.y,
            width: 1,
            height: area.height,
        },
        ScrollSurface::Search,
        len,
        state.offset(),
        area.height as usize,
    );
}

fn render_source_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<(String, bool, MouseTarget)> = app
        .config
        .effective_search()
        .selectable_sources()
        .into_iter()
        .map(|source| {
            (
                format!("{}  {}", source.code(), source.label()),
                source == app.search.source,
                MouseTarget::SearchSourceSelect(source),
            )
        })
        .collect();
    render_dropdown(
        frame,
        app,
        area,
        MouseTarget::SearchSourceMenu,
        t!(" Source ", " 소스 "),
        &rows,
    );
}

fn dropdown_width<'a>(labels: impl Iterator<Item = &'a str>) -> u16 {
    labels
        .map(|l| UnicodeWidthStr::width(l) as u16)
        .max()
        .unwrap_or(0)
        + 1
        + 2
}

fn render_dropdown(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    anchor_target: MouseTarget,
    title: &str,
    rows: &[(String, bool, MouseTarget)],
) {
    let Some(anchor) = app
        .bridges
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == anchor_target)
        .map(|b| b.rect)
    else {
        return;
    };

    let box_w = dropdown_width(rows.iter().map(|(l, _, _)| l.as_str()));
    let box_h = rows.len() as u16 + 2;
    let x = anchor.x.min(area.right().saturating_sub(box_w));
    let y = (anchor.y + 1).min(area.bottom().saturating_sub(box_h));
    let popup = Rect {
        x,
        y,
        width: box_w,
        height: box_h,
    }
    .intersection(area);
    if popup.is_empty() {
        return;
    }

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let list = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, (label, active, target)) in rows.iter().enumerate() {
        let i = i as u16;
        if i >= list.height {
            break;
        }
        let row = Rect {
            x: list.x,
            y: list.y + i,
            width: list.width,
            height: 1,
        };
        let mut text = format!(" {label}");
        let pad = (list.width as usize).saturating_sub(UnicodeWidthStr::width(text.as_str()));
        text.push_str(&" ".repeat(pad));
        let style = if *active {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextPrimary)
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), row);
        app.register_mouse_button(row, *target);
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
