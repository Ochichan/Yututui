//! The search view: a query input box above a results list.
//!
//! The input box and the list share focus ([`SearchFocus`]); the focused one gets the
//! accent color. Rows are formatted here, per frame — the result set is small (one
//! page of songs) so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget, ScrollSurface, SearchFocus, SearchKind, StatusKind};
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
    // the search screen. It auto-clears after STATUS_TTL via the global StatusTick. A just-set
    // message types itself in while the toast animation's window runs.
    if !app.status.text.is_empty() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[0].width) {
            frame.render_widget(Paragraph::new(line), rows[0]);
        } else {
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
    let current_source = app
        .search_config_for_mode()
        .normalized_source(app.search.source);
    // Playlist kind fixes the provider (YouTube playlists), so its tag replaces the
    // source code in the title.
    let tail = if app.search.kind == SearchKind::Playlists {
        t!("playlists (^P)", "플레이리스트 (^P)").to_owned()
    } else {
        current_source.code().to_owned()
    };
    let title = if app.authenticated {
        if crate::i18n::is_korean() {
            format!(" 검색 · {tail} ")
        } else {
            format!(" Search · {tail} ")
        }
    } else {
        if crate::i18n::is_korean() {
            format!(" 검색 · 익명 · {tail} ")
        } else {
            format!(" Search · anonymous · {tail} ")
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
        if focused {
            // The caret is its own span so the caret animation can blink it (the plain solid
            // block in the text's own style when that flag is off, exactly as before).
            let caret = crate::ui::anim::caret_span(
                app,
                app.theme.style(R::TextPrimary),
                app.theme.color(R::Background),
            );
            Paragraph::new(Line::from(vec![
                Span::styled(app.search.input.clone(), app.theme.style(R::TextPrimary)),
                caret,
            ]))
        } else {
            Paragraph::new(app.search.input.clone()).style(app.theme.style(R::TextPrimary))
        }
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
    let current_source = app
        .search_config_for_mode()
        .normalized_source(app.search.source);
    let label = format!("{}▾", current_source.code());
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
        // Animated trailing dots while a search is in flight (static ellipsis when off).
        let text = match crate::ui::anim::activity_dots(app) {
            Some(dots) => format!("{}{dots}", t!("Searching", "검색 중")),
            None => t!("Searching…", "검색 중…").to_owned(),
        };
        let msg = Line::from(text).style(app.theme.style(R::Warning));
        frame.render_widget(Paragraph::new(msg), area);
        return;
    }

    let focused = app.search.focus == SearchFocus::Results;
    // The wheel scrolls this viewport freely; the render keeps a keyboard-moved cursor on
    // screen with a margin (see `ui::scroll`). Resolved before the rows are built because
    // the selection only highlights — and only marquees — while actually inside the window.
    let len = app.search.results.len();
    let offset = app.bridges.search_scroll.resolve(
        app.search.selected.min(len.saturating_sub(1)),
        area.height,
        len,
        crate::ui::scroll::SCROLLOFF,
    );
    let visible_sel = (len > 0)
        .then(|| app.search.selected.min(len - 1))
        .filter(|sel| (offset..offset + area.height as usize).contains(sel));

    // Fresh results cascade in top-to-bottom while the stagger window runs. New results
    // always land with the viewport at the top, so styling by absolute row index is the
    // visible order.
    let items: Vec<ListItem> = app
        .search
        .results
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let title = app.display_title(s);
            let artist = app.display_artist(s);
            // Fixed-width heart slot (like the library lists) so favoriting a row never
            // shifts its title relative to its neighbors.
            let heart = if app.library.is_favorite(&s.video_id) {
                "♥ "
            } else {
                "  "
            };
            // Playlist rows get their own tag so they read as containers, not tracks.
            // Codes vary in width ([YT] vs [RAD]); pad to one column so titles align
            // across mixed-source (ALL) results.
            let source = if s.youtube_playlist_id().is_some() {
                "[PL]".to_owned()
            } else {
                format!("[{}]", s.source.code())
            };
            let source = crate::ui::text::pad_to_width(&source, 6);
            let text = if s.duration.is_empty() {
                format!("{title} — {artist}")
            } else {
                format!("{title} — {artist}  ({})", s.duration)
            };
            // The focused, visible cursor row marquees when clipped — the source tag and
            // heart gutter stay put while the text crawls (see `anim::selected_marquee`).
            let text = if focused && visible_sel == Some(i) {
                crate::ui::anim::selected_marquee(
                    app,
                    crate::app::ScrollSurface::Search,
                    i,
                    &text,
                    (area.width as usize).saturating_sub(2 + 8),
                )
            } else {
                text
            };
            let line = format!("{source}{heart}{text}");
            ListItem::new(line).style(crate::ui::anim::stagger_style(
                app,
                crate::app::Mode::Search,
                i,
                app.theme.style(R::TextPrimary),
            ))
        })
        .collect();

    let highlight = if focused {
        crate::ui::anim::selection_style(
            app,
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Style::default()
            .fg(app.theme.color(R::SelectionInactiveFg))
            .bg(app.theme.color(R::SelectionInactiveBg))
    };
    let list = List::new(items)
        .highlight_style(highlight)
        .style(app.theme.style(R::TextPrimary))
        .highlight_symbol("▶ ")
        // Reserve the ▶ gutter even while the selection is scrolled off-view
        // (`state.select` below is skipped then) — otherwise every visible row
        // shifts 2 cells left the moment the wheel moves the cursor out of the
        // viewport and snaps back when it returns.
        .highlight_spacing(HighlightSpacing::Always);

    // Pre-seed the wheel offset so ratatui honors it; only highlight the selection while
    // it is actually visible, so the wheel can scroll past it.
    let mut state = ListState::default().with_offset(offset);
    if let Some(sel) = visible_sel {
        state.select(Some(sel));
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
    let current_source = app
        .search_config_for_mode()
        .normalized_source(app.search.source);
    let rows: Vec<(String, bool, MouseTarget)> = app
        .search_config_for_mode()
        .selectable_sources()
        .into_iter()
        .map(|source| {
            (
                format!("{}  {}", source.code(), source.label()),
                source == current_source,
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
            crate::ui::anim::selection_style(
                app,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            app.theme.style(R::TextPrimary)
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), row);
        app.register_mouse_button(row, *target);
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
