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
        let body: String = body.chars().take(body_w.saturating_sub(1)).collect();

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
        .title(" ⚠ Delete downloaded files ")
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

    let noun = if count == 1 { "file" } else { "files" };
    frame.render_widget(
        Paragraph::new(format!("Delete {count} downloaded {noun} from disk?"))
            .alignment(Alignment::Center)
            .style(theme.style(R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new("This permanently removes the actual files.")
            .alignment(Alignment::Center)
            .style(theme.style(R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(MouseTarget::ConfirmDelete, " Delete (Enter) "),
        buttons::Seg::label("    "),
        buttons::Seg::button(MouseTarget::CancelDelete, " Cancel (Esc) "),
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
