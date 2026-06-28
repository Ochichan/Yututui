//! The library view: saved tracks, recent history, and local download-folder audio.
//!
//! Rows are formatted per frame. The underlying collections are bounded and only the
//! visible window is laid out by ratatui, so there's no need to pre-format into state.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, LibraryTab, MouseTarget};
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
        Constraint::Length(1), // tab bar
        Constraint::Length(1), // spacer
        Constraint::Min(0),    // list
        Constraint::Length(1), // help
    ])
    .split(inner);

    render_tabs(frame, app, rows[0]);
    render_list(frame, app, rows[2]);

    buttons::render_help_button(frame, app, rows[3]);
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
    // Record the viewport height so PageUp/PageDown can move by a screenful (see app::page_step).
    app.list_viewport_rows.set(area.height);

    let rows = app.library_rows();

    if rows.is_empty() {
        let msg = match app.library_tab {
            LibraryTab::All => t!(
                "No library tracks yet — play, favorite, or download something.",
                "아직 라이브러리에 곡이 없어요 — 재생하거나 즐겨찾기하거나 다운로드해 보세요."
            ),
            LibraryTab::Favorites => t!(
                "No favorites yet — press f on a track to save it.",
                "즐겨찾기가 없어요 — 곡에서 f 를 눌러 저장하세요."
            ),
            LibraryTab::History => {
                t!("No history yet — play something.", "재생 기록이 없어요 — 뭐든 재생해 보세요.")
            }
            LibraryTab::Downloads => t!(
                "No downloaded tracks found in the download folder.",
                "다운로드 폴더에 받은 곡이 없어요."
            ),
        };
        frame.render_widget(Paragraph::new(Line::from(msg).style(app.theme.style(R::TextMuted))), area);
        return;
    }

    if area.height == 0 || area.width < 4 {
        return;
    }

    // Rows are rendered by hand (like the queue window) so a mouse drag can highlight the
    // inclusive anchor..=cursor range and each deletable row can carry a trailing ✗ button.
    // The All tab is an aggregate view (a track may live in several tabs at once), so it has
    // no per-row delete — manage tracks from their own Favorites/History/Downloads tab.
    let len = rows.len();
    let cursor = app.library_selected.min(len - 1);
    let sel_lo = cursor.min(app.library_anchor);
    let sel_hi = cursor.max(app.library_anchor);
    let deletable = app.library_tab != LibraryTab::All;
    let del_w: u16 = if deletable { 2 } else { 0 };

    // Scroll so the cursor row stays visible (window roughly centered on it).
    let visible = area.height as usize;
    let start = cursor
        .saturating_sub(visible / 2)
        .min(len.saturating_sub(visible));

    let body_w = area.width.saturating_sub(del_w) as usize;
    for (vis, (i, song)) in rows.iter().enumerate().skip(start).take(visible).enumerate() {
        let y = area.y + vis as u16;
        let selected = i >= sel_lo && i <= sel_hi;
        let marker = if i == cursor { "▶ " } else { "  " };
        let body = if song.duration.is_empty() {
            format!("{marker}{} — {}", song.title, song.artist)
        } else {
            format!("{marker}{} — {}  ({})", song.title, song.artist, song.duration)
        };
        let body = crate::ui::text::truncate_to_width(&body, body_w.saturating_sub(1));

        let base = if selected {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            app.theme.style(R::TextPrimary)
        };
        let row = Rect { x: area.x, y, width: area.width, height: 1 };
        frame.render_widget(Paragraph::new(Line::from(body).style(base)), row);

        // The row body selects (single-click) / plays (double-click).
        let body_rect = Rect { x: row.x, y, width: row.width.saturating_sub(del_w), height: 1 };
        app.register_mouse_button(body_rect, MouseTarget::ListRow(i));

        if deletable {
            // Trailing ✗ delete button, kept on the row's highlight when selected.
            let del_x = row.x + row.width.saturating_sub(del_w);
            let del_rect = Rect { x: del_x, y, width: del_w, height: 1 };
            let mut del_style = app.theme.style(R::Error);
            if selected {
                del_style = del_style.bg(app.theme.color(R::SelectionBg));
            }
            frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
            app.register_mouse_button(del_rect, MouseTarget::LibraryDel(i));
        }
    }

    // Scrollbar on the right border, tracking the cursor; hidden when the list fits.
    buttons::render_list_scrollbar(frame, app, area, len, cursor, visible);
}

/// A modal confirming deletion of downloaded files from disk. Deleting a real file is
/// irreversible, so it's gated behind an explicit choice — Enter/`y` or the Delete button
/// confirm, Esc or a click elsewhere cancels. The buttons publish `ConfirmDelete` /
/// `CancelDelete` hit rects so the modal is fully usable by mouse as well as keyboard.
pub fn render_confirm_delete(frame: &mut Frame, app: &App, area: Rect) {
    let count = app.confirm_delete_files.as_ref().map_or(0, Vec::len);
    let theme = &app.theme;
    let popup = centered_fixed(area, 56, 9);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(t!(" ⚠ Delete downloaded files ", " ⚠ 다운로드한 파일 삭제 "))
        .borders(Borders::ALL)
        .border_style(theme.style(R::Error).add_modifier(Modifier::BOLD))
        .style(theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // prompt
        Constraint::Length(1), // detail
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // buttons
    ])
    .split(inner);

    let prompt = if crate::i18n::is_korean() {
        // Korean has no plural form, so the count carries the quantity on its own.
        format!("다운로드한 파일 {count}개를 디스크에서 삭제할까요?")
    } else {
        let noun = if count == 1 { "file" } else { "files" };
        format!("Delete {count} downloaded {noun} from disk?")
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Center)
            .style(theme.style(R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "This permanently removes the actual files.",
            "실제 파일이 영구적으로 삭제됩니다."
        ))
        .alignment(Alignment::Center)
        .style(theme.style(R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(MouseTarget::ConfirmDelete, t!(" Delete (Enter) ", " 삭제 (Enter) ")),
        buttons::Seg::label("    "),
        buttons::Seg::button(MouseTarget::CancelDelete, t!(" Cancel (Esc) ", " 취소 (Esc) ")),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        theme.style(R::Error).add_modifier(Modifier::BOLD),
        theme.style(R::Accent).add_modifier(Modifier::BOLD),
        Alignment::Center,
    );
}

/// A `w`×`h` rect centered in `area`, clamped so it never exceeds the available space.
fn centered_fixed(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}
