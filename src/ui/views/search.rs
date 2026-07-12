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
        Constraint::Length(crate::ui::control_box::docked_rows(app)), // docked player bar
        Constraint::Length(1), // help
    ])
    .split(inner);

    // A transient confirmation (e.g. "Added to queue: …" after enqueuing a search result while a
    // track is already playing) rides the otherwise-empty top band so it's visible without leaving
    // the search screen. It auto-clears after STATUS_TTL via the global StatusTick. A just-set
    // message types itself in while the toast animation's window runs.
    if !app.status.text.is_empty() && !app.control_box_active() {
        if let Some(line) = crate::ui::anim::status_toast_line(app, rows[0].width) {
            frame.render_widget(Paragraph::new(line), rows[0]);
        } else {
            let role = match app.status.kind {
                StatusKind::Error => R::Error,
                StatusKind::Info => R::Success,
            };
            frame.render_widget(
                Paragraph::new(
                    Line::from(app.status.text.as_str())
                        .style(app.theme.style(role))
                        .alignment(Alignment::Center),
                ),
                rows[0],
            );
        }
    }

    render_input(frame, app, rows[1]);
    render_results(frame, app, rows[2]);
    crate::ui::control_box::render_docked(frame, app, rows[3]);

    buttons::render_help_button(frame, app, rows[4]);
    if app.dropdowns.search_source_open {
        render_source_dropdown(frame, app, inner);
    }
    // The results-filter popup draws over the whole search screen (below the global
    // overlays, like the queue window on the player). Its rect is a per-frame output.
    app.search_filter.rect.set(None);
    if app.search_filter.open {
        render_filter_popup(frame, app, inner);
    }
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    // A source chip on the left, the input box in the middle, and themed Search + Filter
    // buttons on the right. Enter and the button submit; the chip changes the provider;
    // the filter button opens the results-filter popup.
    let cols = Layout::horizontal([
        Constraint::Length(SEARCH_SOURCE_W),
        Constraint::Min(0),
        Constraint::Length(SEARCH_BTN_W),
        Constraint::Length(SEARCH_FILTER_BTN_W),
    ])
    .split(area);
    let source_area = cols[0];
    let input_area = cols[1];
    let button_area = cols[2];
    let filter_area = cols[3];

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
    render_filter_button(frame, app, filter_area);
}

/// Width of the left source chip cluster (border + `ALL▾`).
const SEARCH_SOURCE_W: u16 = 6;

/// Width of the Search button cluster (border + " Search ").
const SEARCH_BTN_W: u16 = 10;

/// Width of the Filter button cluster (border + " ⌕ Filter ").
const SEARCH_FILTER_BTN_W: u16 = 10;

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

/// A themed, bordered button in the search bar's right cluster: a centered label whose whole
/// rect is the click target. `style` is passed in so the Filter button can dim when there is
/// nothing to filter. Shared by the Search and Filter buttons so the two siblings can't drift.
fn render_search_bar_button(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    label: &str,
    style: Style,
    target: MouseTarget,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderMuted))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::from(label).style(style).alignment(Alignment::Center)),
        inner,
    );
    app.register_mouse_button(area, target);
}

/// The themed Search button next to the input box: clicking it submits the query, the same
/// as pressing Enter.
fn render_search_button(frame: &mut Frame, app: &App, area: Rect) {
    render_search_bar_button(
        frame,
        app,
        area,
        t!("Search", "검색"),
        app.theme.style(R::Accent).add_modifier(Modifier::BOLD),
        MouseTarget::SearchSubmit,
    );
}

/// The themed Filter button next to the Search button: clicking it opens the results-filter
/// popup, the same as pressing `/` on the results. Muted while there is nothing to filter
/// (the click is a no-op then).
fn render_filter_button(frame: &mut Frame, app: &App, area: Rect) {
    let style = if app.search.results.is_empty() {
        app.theme.style(R::TextMuted)
    } else {
        app.theme.style(R::Accent).add_modifier(Modifier::BOLD)
    };
    render_search_bar_button(
        frame,
        app,
        area,
        t!("⌕ Filter", "⌕ 필터"),
        style,
        MouseTarget::SearchFilterOpen,
    );
}

/// The shared cells of a search-result row — the padded source tag (`[YT]`/`[PL]`, width 6),
/// the fixed-width heart gutter, and the un-marqueed `title — artist (dur)` body — so the
/// results list and the filter popup format rows identically and never drift. Each caller
/// applies its own leading marker and marquee/truncation on top.
fn result_row_cells(app: &App, song: &crate::api::Song) -> (String, &'static str, String) {
    // Fixed-width heart slot (like the library lists) so favoriting a row never shifts its
    // title relative to its neighbors.
    let heart = if app.library.is_favorite(&song.video_id) {
        "♥ "
    } else {
        "  "
    };
    // Playlist rows get their own tag so they read as containers, not tracks. Codes vary in
    // width ([YT] vs [RAD]); pad to a fixed column so titles align across mixed-source results.
    let source = if song.youtube_playlist_id().is_some() {
        "[PL]".to_owned()
    } else {
        format!("[{}]", song.source.code())
    };
    let source = crate::ui::text::pad_to_width(&source, 6);
    let title = app.display_title(song);
    let artist = app.display_artist(song);
    let text = if song.duration.is_empty() {
        format!("{title} — {artist}")
    } else {
        format!("{title} — {artist}  ({})", song.duration)
    };
    (source, heart, text)
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
    // The effective multi-selection (drag range, Shift+nav, Ctrl/Cmd picks) highlights
    // like the Library list; the cursor row additionally gets the ▶ marker below. A
    // lone cursor row is left to the ListState highlight so wheel-scrolling past it
    // keeps today's behavior.
    let selected_rows = app.search_selection_indices();
    let row_selected =
        |i: usize| selected_rows.len() > 1 && selected_rows.binary_search(&i).is_ok();
    // A cursor row that was Ctrl/Cmd-toggled OUT of the picks must not read as selected:
    // keep its ▶ marker but drop the selection colors (the Library renders the same way).
    let cursor_style = if selected_rows.binary_search(&app.search.selected).is_ok() {
        highlight
    } else {
        app.theme.style(R::TextPrimary)
    };

    // Fresh results cascade in top-to-bottom while the stagger window runs. New results
    // always land with the viewport at the top, so styling by absolute row index is the
    // visible order.
    let items: Vec<ListItem> = app
        .search
        .results
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (source, heart, text) = result_row_cells(app, s);
            // The focused, visible cursor row marquees when clipped — the source tag and
            // heart gutter stay put while the text crawls (see `anim::selected_marquee`).
            // Suppressed while the filter popup is open: its cursor row marquees instead,
            // and the two would fight over the single shared phase cell.
            let text = if focused && visible_sel == Some(i) && !app.search_filter.open {
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
            let base = if row_selected(i) {
                highlight
            } else {
                app.theme.style(R::TextPrimary)
            };
            ListItem::new(line).style(crate::ui::anim::stagger_style(
                app,
                crate::app::Mode::Search,
                i,
                base,
            ))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(cursor_style)
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
    let Some(anchor) = app.hits.rect_of_target(anchor_target) else {
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
        app.register_mouse_button(row, target.clone());
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// The results-filter popup ("추가 창"): a centered window over the Search view holding a
/// live filter input and the narrowed result rows. Typing edits the query, ↑↓/PgUp/PgDn/
/// Home/End move within the matches, Enter or a double-click plays the highlighted one, a
/// right-click enqueues it (popup stays open), Esc or a click outside closes.
fn render_filter_popup(frame: &mut Frame, app: &App, area: Rect) {
    let rows_all = app.search_filter_rows();
    let total = app.search.results.len();

    // Same proportions as the queue window, with a wider floor so `title — artist (time)`
    // rows stay readable and 4 chrome rows (border + filter input + hint) reserved.
    let popup = crate::ui::centered_list_popup(area, rows_all.len().max(1), 4, 44);
    if popup.is_empty() {
        return;
    }
    app.search_filter.rect.set(Some(popup));

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" ⌕ Filter results ", " ⌕ 결과 필터 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 3 || inner.width < 8 {
        crate::ui::seal_popup_background(frame, app, popup);
        crate::ui::mark_art_rows_for_popup(frame, app, popup);
        return;
    }

    let sections = Layout::vertical([
        Constraint::Length(1), // filter input line
        Constraint::Min(1),    // narrowed rows
        Constraint::Length(1), // hint
    ])
    .split(inner);

    // `filter: <query>█  [k/N]` — the Library filter's visual language, inside the popup.
    let count_hint = if rows_all.is_empty() {
        t!("  (no matches)", "  (일치 없음)").to_owned()
    } else if crate::i18n::is_korean() {
        format!("  [{}/{total}개]", rows_all.len())
    } else {
        format!("  [{}/{total}]", rows_all.len())
    };
    let query_shown = crate::ui::text::truncate_owned_to_width(
        app.search_filter.query.clone(),
        (sections[0].width as usize)
            .saturating_sub(10 + UnicodeWidthStr::width(count_hint.as_str())),
    );
    let input = Line::from(vec![
        Span::styled(
            t!("  filter: ", "  필터: "),
            crate::ui::popup_style(app, R::TextMuted),
        ),
        Span::styled(query_shown, crate::ui::popup_style(app, R::TextPrimary)),
        crate::ui::anim::caret_span(
            app,
            crate::ui::popup_style(app, R::Accent),
            crate::ui::popup_bg(app),
        ),
        Span::styled(count_hint, crate::ui::popup_style(app, R::TextMuted)),
    ]);
    frame.render_widget(Paragraph::new(input), sections[0]);

    let list_area = sections[1];
    if rows_all.is_empty() {
        let msg = if crate::i18n::is_korean() {
            format!("'{}' 와 일치하는 결과가 없어요.", app.search_filter.query)
        } else {
            format!("No results match \"{}\".", app.search_filter.query)
        };
        let msg =
            crate::ui::text::truncate_owned_to_width(format!("  {msg}"), list_area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(msg).style(crate::ui::popup_style(app, R::TextMuted))),
            list_area,
        );
    } else {
        let len = rows_all.len();
        let cursor = app.search_filter.cursor.min(len - 1);
        // The wheel scrolls this viewport freely; the render only nudges it to keep a
        // keyboard-moved cursor on-screen with a margin (see `ui::scroll`).
        let visible = list_area.height as usize;
        let start = app.search_filter.scroll.resolve(
            cursor,
            list_area.height,
            len,
            crate::ui::scroll::SCROLLOFF,
        );
        let body_w = list_area.width as usize;
        for (vis, (display_idx, (orig, song))) in rows_all
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .enumerate()
        {
            let y = list_area.y + vis as u16;
            let selected = display_idx == cursor;
            let marker = if selected { "▶ " } else { "  " };
            let (source, heart, text) = result_row_cells(app, song);
            // The cursor row marquees when clipped, keyed by the *original* row index so
            // retyping (which shifts display positions) doesn't restart a crawl that is
            // still on the same song. The marker/source/heart gutter (10 cols) stays put.
            let text = if selected {
                crate::ui::anim::selected_marquee(
                    app,
                    ScrollSurface::SearchFilter,
                    *orig,
                    &text,
                    body_w.saturating_sub(11),
                )
            } else {
                text
            };
            let body = crate::ui::text::truncate_owned_to_width(
                format!("{marker}{source}{heart}{text}"),
                body_w.saturating_sub(1),
            );
            let style = if selected {
                crate::ui::anim::selection_style(
                    app,
                    Style::default()
                        .fg(app.theme.color(R::SelectionFg))
                        .bg(app.theme.color(R::SelectionBg))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                crate::ui::popup_style(app, R::TextPrimary)
            };
            let row = Rect {
                x: list_area.x,
                y,
                width: list_area.width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(Line::from(body).style(style)), row);
            app.register_mouse_button(row, MouseTarget::SearchFilterRow(display_idx));
        }

        // Scrollbar on the popup's right border, hidden when the matches fit.
        buttons::render_list_scrollbar(
            frame,
            app,
            Rect {
                x: list_area.right(),
                y: list_area.y,
                width: 1,
                height: list_area.height,
            },
            ScrollSurface::SearchFilter,
            len,
            start,
            visible,
        );
    }

    frame.render_widget(
        Paragraph::new(t!(
            "↑↓ move · Enter play · Esc close",
            "↑↓ 이동 · Enter 재생 · Esc 닫기"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        sections[2],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
