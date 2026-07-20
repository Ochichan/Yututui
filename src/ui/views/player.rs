//! The now-playing view: current track, a seekbar with `M:SS / M:SS`, transport state,
//! and queue indicators (position, shuffle, repeat).

use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_image::{Resize, StatefulImage};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, MouseTarget, RadioModeConfirm, ScrollSurface};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

use super::player_layout::calculate_player_filler_layout;
use super::queue_actions;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(crate::ui::anim::border_style(
            app,
            app.theme.style(R::BorderPrimary),
        ))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // A bright comet chasing the outer border (no-op unless its flag is on). Drawn before
    // the nav strip, so the comet ducks under the nav labels along the top edge.
    crate::ui::anim::border_chase_overlay(frame, app, area);

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

    if app.player_bar_position() == crate::config::PlayerBarPosition::Bottom {
        // Docked layout: the filler gets the top of the screen and the control block sits
        // just above the footer, matching the box every other screen shows in this mode.
        let rows = Layout::vertical([
            Constraint::Length(1), // gap (border → filler)
            Constraint::Min(0),    // filler
            Constraint::Length(crate::ui::control_box::DOCKED_BOX_ROWS),
            Constraint::Length(1), // help
        ])
        .split(inner);
        render_filler(frame, app, inner, rows[1]);
        crate::ui::control_box::render_docked(frame, app, rows[2]);
        buttons::render_help_button(frame, app, rows[3]);
    } else {
        let rows = Layout::vertical([
            Constraint::Length(1), // gap (border → title)
            Constraint::Length(1), // title
            Constraint::Length(1), // gap (title → seekbar)
            Constraint::Length(1), // seekbar
            Constraint::Length(1), // gap (seekbar → controls)
            Constraint::Length(1), // mouse controls
            Constraint::Length(1), // gap (controls → status)
            Constraint::Length(1), // transport status
            Constraint::Min(0),    // filler
            Constraint::Length(1), // help
        ])
        .split(inner);

        // Title / seekbar / transport strip / status line — the shared control block
        // (see `ui::control_box`); the rects are this view's legacy rows, so bytes are unchanged.
        // The canvas composes before every foreground player surface. With animations off this
        // reordering is cell-identical; with a field effect on, the control widgets cover it in
        // one deterministic foreground pass.
        render_filler(frame, app, inner, rows[8]);
        crate::ui::control_box::render_at(frame, app, rows[1], rows[3], rows[5], rows[7]);

        // The full key list lives in the `?` cheat-sheet now; the footer just points to it
        // (chord pulled live from the keymap, so a remap of "toggle help" updates it).
        buttons::render_help_button(frame, app, rows[9]);
    }

    // The status-line dropdowns draw over the screen so their rows win hit-testing.
    if app.dropdowns.eq_open {
        crate::ui::control_box::render_eq_dropdown(frame, app, inner);
    }
    if app.dropdowns.streaming_open {
        crate::ui::control_box::render_streaming_dropdown(frame, app, inner);
    }
    // The queue window draws last of all so it sits on top and its rects win.
    app.queue_popup.rect.set(None);
    if app.queue_popup.open {
        render_queue_popup(frame, app, inner);
    }
}

/// The queue window: a themed popup listing the whole play queue (current track marked),
/// opened by clicking the `N/M` position label. Rows are click targets — single-click
/// selects (drag to extend the selection), double-click jumps playback there — and each
/// row's trailing `✗` removes that track. Keyboard nav / Delete / Enter operate it while
/// open (see `App::on_key_queue`). Drawn last over everything so its rects win.
pub(in crate::ui) fn render_queue_popup(frame: &mut Frame, app: &App, area: Rect) {
    let total_len = app.queue.len();
    if total_len == 0 {
        return;
    }
    let (pos, total) = app.queue.position();
    let current = app.queue.cursor_pos();

    // ~60% wide, tall enough for the list (+2 for the border) but capped to ~70% of the screen.
    let popup = crate::ui::centered_list_popup(area, total_len, 2, 24);
    if popup.is_empty() {
        return;
    }
    app.queue_popup.rect.set(Some(popup));

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(format!(
            " {} {pos}/{total} ",
            t!("Queue", "대기열", "キュー")
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let list = block.inner(popup);
    frame.render_widget(block, popup);
    if list.height == 0 || list.width < 4 {
        crate::ui::seal_popup_background(frame, app, popup);
        crate::ui::mark_art_rows_for_popup(frame, app, popup);
        return;
    }

    // The wheel scrolls this viewport freely; the render only nudges it to keep a
    // keyboard-moved cursor on-screen with a margin (see `ui::scroll`).
    let visible = list.height as usize;
    let cursor = app.queue_popup.cursor.min(total_len - 1);
    let start = app.queue_popup.scroll.resolve(
        cursor,
        list.height,
        total_len,
        crate::ui::scroll::SCROLLOFF,
    );
    let sel_lo = app.queue_popup.cursor.min(app.queue_popup.anchor);
    let sel_hi = app.queue_popup.cursor.max(app.queue_popup.anchor);

    for (vis, (i, song)) in app
        .queue
        .ordered_iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = list.y + vis as u16;
        let selected = i >= sel_lo && i <= sel_hi;
        let is_current = i == current;
        // The now-playing marker becomes a two-cell mini VU while the EQ-bars animation is on
        // and something is actually playing (same width as `▸ `, so nothing shifts).
        let vu = if is_current {
            crate::ui::anim::queue_marker(app)
        } else {
            None
        };
        let marker = match &vu {
            Some(bars) => bars.as_str(),
            None if is_current => "▸ ",
            None => "  ",
        };
        let title = app.display_title(song);
        let artist = app.display_artist(song);
        let actions_w = queue_actions::reserved_width(app, &song.video_id);
        let body_w = list.width.saturating_sub(actions_w) as usize;
        let text = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{:>3} {title} — {artist}", i + 1),
            body_w.saturating_sub(1),
        );

        let mut base = if selected {
            crate::ui::anim::selection_style(
                app,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg)),
            )
        } else if is_current {
            app.theme.style(R::Accent)
        } else {
            app.theme.style(R::TextPrimary)
        };
        if is_current {
            base = base.add_modifier(Modifier::BOLD);
        }
        let row = Rect {
            x: list.x,
            y,
            width: list.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(base)), row);

        let actions_w = queue_actions::render(frame, app, row, i, &song.video_id, selected);

        // The row body (everything left of its trailing actions) selects / jumps to the track.
        let body_rect = Rect {
            x: row.x,
            y,
            width: row.width.saturating_sub(actions_w),
            height: 1,
        };
        if !body_rect.is_empty() {
            app.register_mouse_button(body_rect, MouseTarget::QueueRow(i));
        }
    }

    // Scrollbar on the right border, tracking the viewport position; hidden when it all fits.
    buttons::render_list_scrollbar(
        frame,
        app,
        Rect {
            x: list.right(),
            y: list.y,
            width: 1,
            height: list.height,
        },
        ScrollSurface::Queue,
        total_len,
        start,
        visible,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// Blank rows framing the album art: one between the transport status line and the art,
/// one between the art's bottom edge and the lyrics block.
const ART_TOP_GAP: u16 = 1;
const ART_LYRICS_GAP: u16 = 1;
/// Breathing room between the radio set piece's bottom edge and the one-line art. A
/// luxury, not a reservation: it collapses row-by-row on short terminals before the
/// separator or the lyrics window would lose space (see [`render_radio_filler`]).
const RADIO_ART_SEP_GAP: u16 = 2;
/// Smallest lyrics window (rows) we keep below the art when both are shown.
const MIN_LYRICS_ROWS: u16 = 3;

/// Compose the central Player stage once, then draw its artwork and lyrics foreground. The
/// responsive geometry helper preserves the classic Top stack and prefers a side-by-side Bottom
/// layout; when artwork is unavailable, lyrics receive the whole filler.
fn render_filler(frame: &mut Frame, app: &App, player_area: Rect, area: Rect) {
    app.art.rect.set(None);
    // The radio set piece rides the album-art toggle: with it off, radio mode falls
    // through to the plain music-mode arms below (`art_active()` is hard-false in radio
    // mode, so those resolve to the no-art layout — lyrics, or the bare animation canvas).
    if app.radio_dedicated_mode && app.config.effective_album_art() {
        crate::ui::anim::render_canvas_composite(
            frame,
            app,
            player_area,
            area,
            None,
            app.lyrics.visible.then_some(area),
            false,
        );
        render_radio_filler(frame, app, area);
        return;
    }
    let layout = calculate_player_filler_layout(app, area, app.art_active(), app.lyrics.visible);
    crate::ui::anim::render_canvas_composite(
        frame,
        app,
        player_area,
        area,
        layout.art,
        layout.lyrics,
        app.player_bar_position() == crate::config::PlayerBarPosition::Bottom
            && layout.art.is_some(),
    );
    if let Some(art) = layout.art {
        draw_art(frame, app, art);
    }
    if let Some(lyrics) = layout.lyrics {
        super::player_lyrics::render(frame, app, lyrics);
    }
}

fn render_art_animation_separator(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The trailing space is deliberate: the motif is tiled edge-to-edge, and it keeps
    // each repetition from butting straight into the next one's leading note.
    const MOTIF: &str = "♫♪.ılılıll|̲̅●̲̅|̲̅=̲̅|̲̅●̲̅|llılılı.♫♪ ";
    let width = usize::from(area.width);
    let offset = if radio_art_animation_on(app) {
        (app.anim_frame() / 6) as usize
    } else {
        0
    };
    let line = repeated_motif_line(MOTIF, width, offset);
    frame.render_widget(
        Paragraph::new(
            Line::from(line)
                .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
                .alignment(Alignment::Center),
        ),
        area,
    );
}

fn repeated_motif_line(motif: &str, width: usize, offset: usize) -> String {
    let clusters = display_clusters(motif);
    if clusters.is_empty() {
        return " ".repeat(width);
    }
    let mut line = String::new();
    let mut i = offset % clusters.len();
    while UnicodeWidthStr::width(line.as_str()) < width {
        line.push_str(&clusters[i]);
        i = (i + 1) % clusters.len();
    }
    crate::ui::text::pad_to_width(
        &crate::ui::text::truncate_owned_to_width(line, width),
        width,
    )
}

fn display_clusters(s: &str) -> Vec<String> {
    let mut clusters = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        if UnicodeWidthChar::width(ch).unwrap_or(0) > 0 && !current.is_empty() {
            clusters.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        clusters.push(current);
    }
    clusters
}

fn radio_art_animation_on(app: &App) -> bool {
    app.radio_dedicated_mode
        && app.animations().master
        && !app.playback.paused
        && app.queue.current().is_some()
}

fn render_radio_filler(frame: &mut Frame, app: &App, area: Rect) {
    let after_gap = area.height.saturating_sub(ART_TOP_GAP);
    let cap = if app.lyrics.visible {
        after_gap.saturating_sub(MIN_LYRICS_ROWS).min(12)
    } else {
        after_gap.saturating_sub(ART_LYRICS_GAP).min(15)
    };
    let band = art_band(area, ART_TOP_GAP, cap);
    match draw_radio_ascii(frame, app, band) {
        Some(radio) => {
            // Give the one-line art breathing room below the set piece, shrinking the
            // gap first when the terminal is short: the separator row itself and (with
            // lyrics on) a readable lyrics window always win over the luxury rows.
            let reserved_below = 1 + if app.lyrics.visible {
                ART_LYRICS_GAP + MIN_LYRICS_ROWS
            } else {
                0
            };
            let slack = area
                .bottom()
                .saturating_sub(radio.bottom())
                .saturating_sub(reserved_below);
            let separator_y = radio.bottom() + RADIO_ART_SEP_GAP.min(slack);
            if separator_y < area.bottom() {
                render_art_animation_separator(
                    frame,
                    app,
                    Rect {
                        x: area.x,
                        y: separator_y,
                        width: area.width,
                        height: 1,
                    },
                );
            }
            if !app.lyrics.visible {
                return;
            }
            let lyrics_y = separator_y.saturating_add(ART_LYRICS_GAP);
            let lyrics_area = Rect {
                x: area.x,
                y: lyrics_y,
                width: area.width,
                height: area.bottom().saturating_sub(lyrics_y),
            };
            super::player_lyrics::render(frame, app, lyrics_area);
        }
        None if app.lyrics.visible => super::player_lyrics::render(frame, app, area),
        None => {}
    }
}

fn draw_radio_ascii(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    if area.width < 12 || area.height < 3 {
        return None;
    }
    const LARGE: &[&str] = &[
        "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⠀⠀⠀⠀⠀⠀⠀⠀",
        "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢰⡟⠷⣶⣤⠀⠀⠀⠀⠀",
        "⠀⠀⠀⢸⠟⠛⠃⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣼⡇⢀⣸⡇⠀⠀⠀⠀⠀",
        "⠀⠀⠀⠘⣧⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠸⠿⠿⠻⣿⣿⡿⠀⠀⠀⠀⠀",
        "⠀⠀⠀⣾⣿⡿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        "⠀⠀⠀⠈⠉⠀⠀⠀⠀⣿⠛⠛⠛⠛⠛⠛⠛⠛⠛⠛⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀",
        "⠀⢀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⡀⠀",
        "⠀⢸⣿⣿⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣉⣉⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣿⣿⡇⠀",
        "⠀⢸⣿⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣿⡇⠀",
        "⠀⢸⣿⡿⠋⣉⣭⣍⡙⢿⣿⡏⢉⣭⣭⣭⣭⡉⢹⣿⡿⢋⣩⣭⣉⠙⢿⣿⡇⠀",
        "⠀⢸⡟⢠⣾⠟⠛⠻⣿⣆⠹⡇⢸⣿⣿⣿⣿⡇⢸⠏⣰⣿⠟⠛⠻⣷⡄⢻⡇⠀",
        "⠀⢸⡇⢸⣿⡀⠛⢀⣿⡿⢀⣧⣤⣤⣤⣤⣤⣤⣼⡀⢿⣿⡀⠛⢀⣿⡇⢸⡇⠀",
        "⠀⢸⣿⣄⠙⠿⣿⡿⠟⣡⣾⣿⡏⢹⡏⢹⡏⢹⣿⣷⣌⠻⢿⣿⠿⠋⣠⣿⡇⠀",
        "⠀⠸⠿⠿⠷⠶⠶⠶⠾⠿⠿⠿⠿⠾⠿⠿⠷⠿⠿⠿⠿⠷⠶⠶⠶⠾⠿⠿⠇⠀",
        "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀",
    ];
    const SMALL: &[&str] = &[
        "⠀⢀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣉⣀⣀⣀⣀⣀⣀⣀⡀⠀",
        "⠀⢸⣿⣿⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣉⣉⣉⣉⣉⣹⣿⣿⣏⣉⣉⣉⣿⣿⡇⠀",
        "⠀⢸⣿⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣉⣿⡇⠀",
        "⠀⢸⣿⡿⠋⣉⣭⣍⡙⢿⣿⡏⢉⣭⣭⣭⣭⡉⢹⣿⡿⢋⣩⣭⣉⠙⢿⣿⡇⠀",
        "⠀⢸⡟⢠⣾⠟⠛⠻⣿⣆⠹⡇⢸⣿⣿⣿⣿⡇⢸⠏⣰⣿⠟⠛⠻⣷⡄⢻⡇⠀",
        "⠀⢸⡇⢸⣿⡀⠛⢀⣿⡿⢀⣧⣤⣤⣤⣤⣤⣤⣼⡀⢿⣿⡀⠛⢀⣿⡇⢸⡇⠀",
        "⠀⢸⣿⣄⠙⠿⣿⡿⠟⣡⣾⣿⡏⢹⡏⢹⡏⢹⣿⣷⣌⠻⢿⣿⠿⠋⣠⣿⡇⠀",
        "⠀⠸⠿⠿⠷⠶⠶⠶⠾⠿⠿⠿⠿⠾⠿⠿⠷⠿⠿⠿⠿⠷⠶⠶⠶⠾⠿⠿⠇⠀",
    ];
    let art = if area.width >= 34 && area.height >= LARGE.len() as u16 {
        LARGE
    } else {
        SMALL
    };
    let height = (art.len() as u16).min(area.height);
    if height == 0 {
        return None;
    }
    let y = area.y + area.height.saturating_sub(height) / 2;
    let rect = Rect {
        x: area.x,
        y,
        width: area.width,
        height,
    };
    let style = app.theme.style(R::Accent).add_modifier(Modifier::BOLD);
    let rendered = radio_art_lines(art, height as usize, radio_art_animation_on(app), app);
    let lines: Vec<Line> = rendered
        .into_iter()
        .map(|line| Line::from(line).style(style).alignment(Alignment::Center))
        .collect();
    frame.render_widget(Paragraph::new(lines), rect);
    Some(rect)
}

fn radio_art_lines(art: &[&str], height: usize, animated: bool, app: &App) -> Vec<String> {
    let mut lines: Vec<String> = art
        .iter()
        .take(height)
        .map(|line| (*line).to_owned())
        .collect();
    if !animated || lines.is_empty() {
        return lines;
    }

    let slow = app.anim_frame() / 2;
    let width = art
        .iter()
        .map(|line| UnicodeWidthStr::width(*line))
        .max()
        .unwrap_or(0);
    let note_sway = [-1, 0, 1, 1, 0, -1, -1, 0][((slow / 8) as usize) % 8];
    let body_sway = [0, 1, 0, -1][((slow / 12) as usize) % 4];

    for (i, line) in lines.iter_mut().enumerate() {
        if art.len() > 8 && i == 5 {
            *line = compose_radio_handle_line(line, note_sway, body_sway, width);
            continue;
        }
        let delta = if art.len() > 8 && i < 6 {
            note_sway
        } else if i >= 3 {
            body_sway
        } else {
            0
        };
        if delta != 0 {
            *line = shift_display_line(line, delta, width);
        }
    }

    if ((slow / 10) % 2) == 1 {
        for line in &mut lines {
            *line = line.replace("⣿⣿⣿⣿", "⣿⣿⣶⣿");
        }
    }
    lines
}

fn compose_radio_handle_line(line: &str, note_sway: i32, body_sway: i32, width: usize) -> String {
    let Some(handle_start_byte) = line.find('⣿') else {
        return shift_display_line(line, body_sway, width);
    };
    let (note, handle) = line.split_at(handle_start_byte);
    let handle_start = UnicodeWidthStr::width(note);
    let mut cells = vec!['⠀'; width];
    overlay_radio_segment(&mut cells, 0, note, note_sway);
    overlay_radio_segment(&mut cells, handle_start, handle, body_sway);
    cells.into_iter().collect()
}

fn overlay_radio_segment(cells: &mut [char], start: usize, segment: &str, delta: i32) {
    let mut col = 0i32;
    let start = start as i32 + delta;
    for ch in segment.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        let target = start + col;
        if ch != '⠀'
            && ch != ' '
            && target >= 0
            && let Some(cell) = cells.get_mut(target as usize)
        {
            *cell = ch;
        }
        col += ch_width as i32;
    }
}

fn shift_display_line(line: &str, delta: i32, width: usize) -> String {
    if delta > 0 {
        let shifted = format!("{}{}", "⠀".repeat(delta as usize), line);
        return crate::ui::text::pad_to_width(
            &crate::ui::text::truncate_owned_to_width(shifted, width),
            width,
        );
    }

    let mut skipped = 0usize;
    let mut out = String::new();
    for ch in line.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if skipped < delta.unsigned_abs() as usize {
            skipped = skipped.saturating_add(ch_width);
            continue;
        }
        out.push(ch);
    }
    crate::ui::text::pad_to_width(&crate::ui::text::truncate_owned_to_width(out, width), width)
}

/// The top slice of `area` the art may occupy: skip `gap` rows under the status line, then
/// cap the height to `max_h` (clamped to whatever space is left).
fn art_band(area: Rect, gap: u16, max_h: u16) -> Rect {
    let avail = area.height.saturating_sub(gap);
    Rect {
        x: area.x,
        y: area.y + gap,
        width: area.width,
        height: max_h.min(avail),
    }
}

/// Draw the current track's album art into the already aspect-fitted final rectangle. The layout
/// and the canvas hard mask share this exact value, so native protocols never expose animation
/// glyphs through placeholder cells.
fn draw_art(frame: &mut Frame, app: &App, rect: Rect) {
    app.art.rect.set(Some(rect));
    // Retro mode draws the cover itself as luminance-ramp ASCII art: a basic console has no
    // graphics protocol, and the half-block fallback needs truecolor cells the scrubber
    // would flatten into `#` mush anyway.
    if app.retro_mode() {
        if let Some((id, img)) = app.art_source_image() {
            crate::ui::ascii_art::render_image(
                frame,
                crate::ui::ascii_art::Slot::AlbumArt,
                id,
                img,
                rect,
            );
        }
        return;
    }
    if let Some(proto) = app.art.protocol.borrow_mut().as_mut() {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Scale(Some(FilterType::Lanczos3))),
            rect,
            proto,
        );
    }
}

/// A modal confirmation for entering/leaving dedicated Radio mode. It intentionally mirrors the
/// Settings reset confirmation shape so broad UI-mode switches use the same interaction pattern.
pub fn render_radio_mode_confirm(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    confirm: RadioModeConfirm,
) {
    let popup = centered_fixed(area, 60, 11);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(confirm.title())
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(confirm.prompt())
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(confirm.detail())
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmRadioMode,
            t!(" Confirm (Enter) ", " 확인 (Enter) ", " 確認 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelRadioMode,
            t!(" Cancel (Esc) ", " 취소 (Esc) ", " キャンセル (Esc) "),
        ),
    ];
    buttons::render_segments_with_hit_height(
        frame,
        app,
        rows[4],
        &segs,
        (
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
        ),
        Alignment::Center,
        3,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
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
