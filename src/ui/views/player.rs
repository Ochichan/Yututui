//! The now-playing view: current track, a seekbar with `M:SS / M:SS`, transport state,
//! and queue indicators (position, shuffle, repeat).

use std::borrow::Cow;

use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui_image::{Resize, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DownloadState, MouseTarget, RadioModeConfirm, ScrollSurface, StatusKind};
use crate::keymap::Action;
use crate::lyrics;
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};
use crate::util::format;

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

    // Title (or an error, if playback failed). With the title/heart animations off this is the
    // plain bold line exactly as before; with them on, `anim::title_line` returns a shimmering /
    // scrolling line (and a pulsing ♥), which we render in place of it.
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
            rows[1],
        );
    } else if let Some(line) = app.queue.current().and_then(|s| {
        let title = app.display_title(s);
        let artist = app.display_artist(s);
        crate::ui::anim::title_line(
            app,
            title.as_ref(),
            artist.as_ref(),
            app.library.is_favorite(&s.video_id),
            rows[1].width,
        )
    }) {
        frame.render_widget(Paragraph::new(line), rows[1]);
    } else {
        let text = match app.queue.current() {
            Some(s) => {
                let title = app.display_title(s);
                let artist = app.display_artist(s);
                let heart = if app.library.is_favorite(&s.video_id) {
                    "♥ "
                } else {
                    ""
                };
                format!("{heart}{title} — {artist}")
            }
            None => t!(
                "Nothing playing — press / to search",
                "재생 중인 곡 없음 — / 를 눌러 검색"
            )
            .to_owned(),
        };
        frame.render_widget(
            Paragraph::new(
                Line::from(text.bold())
                    .style(app.theme.style(R::TextPrimary))
                    .alignment(Alignment::Center),
            ),
            rows[1],
        );
    }

    // Seekbar. Ratio + label (incl. the None/zero-duration handling) live in `format` so the
    // edge cases are unit-tested without a frame buffer.
    let ratio = format::seekbar_ratio(app.playback.time_pos, app.playback.duration);
    let seekbar = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(app.theme.color(R::GaugeFilled))
                .bg(app.theme.color(R::GaugeEmpty)),
        )
        .ratio(ratio)
        .label(format::seekbar_label(
            app.playback.time_pos,
            app.playback.duration,
        ));
    frame.render_widget(seekbar, rows[3]);
    // A bright comet sweeps the filled portion when the seekbar animation is on (no-op otherwise).
    crate::ui::anim::seekbar_overlay(frame, app, rows[3], ratio);
    // Publish the seekbar's screen rect so a mouse click can be hit-tested for seeking.
    app.bridges.seekbar_rect.set(Some(rows[3]));

    render_controls(frame, app, rows[5]);

    render_status_line(frame, app, rows[7]);

    // Central filler: album art (top) and/or the lyrics panel (below). With album art off
    // this is exactly the old behaviour — lyrics fill the whole area, nothing else draws.
    render_filler(frame, app, rows[8]);

    // The full key list lives in the `?` cheat-sheet now; the footer just points to it
    // (chord pulled live from the keymap, so a remap of "toggle help" updates it).
    buttons::render_help_button(frame, app, rows[9]);

    // The status-line dropdowns draw over the screen so their rows win hit-testing.
    if app.dropdowns.eq_open {
        render_eq_dropdown(frame, app, inner);
    }
    if app.dropdowns.streaming_open {
        render_streaming_dropdown(frame, app, inner);
    }
    // The queue window draws last of all so it sits on top and its rects win.
    app.queue_popup.rect.set(None);
    if app.queue_popup.open {
        render_queue_popup(frame, app, inner);
    }
}

/// The current track's tri-state rating glyph: 👍 liked, 👎 disliked, 🤔 neither. Language-neutral
/// Unicode — all three are width-2, so the status line never shifts as the state changes — so the
/// row reads identically in every UI language. `like` is favorite membership; `dislike` is the
/// streaming engine's hard-block flag — mutually exclusive, so one glyph covers both states, cycled by
/// [`Action::CycleRating`].
fn rating_glyph(liked: bool, disliked: bool) -> &'static str {
    match (liked, disliked) {
        (true, _) => "👍",
        (false, true) => "👎",
        (false, false) => "🤔",
    }
}

/// The transport status line: state, queue position, rating, shuffle, repeat, speed, EQ, etc.
///
/// Rendered as click segments rather than one string so `R:` (toggles repeat) and
/// `eq:` (opens the preset dropdown) are mouse targets — but every segment shares the same
/// cyan style, so the line looks exactly like the plain status text it replaced. `eq:` is
/// always shown now (so the dropdown is always reachable); the rest stay conditional.
fn render_status_line(frame: &mut Frame, app: &App, area: Rect) {
    let parts = status_line_parts(app);
    let segments: Vec<Seg> = parts
        .iter()
        .map(|(target, text)| match target {
            Some(t) => Seg::button(*t, text.as_ref()),
            None => Seg::label(text.as_ref()),
        })
        .collect();
    // Same style for buttons and labels so the clickable parts are visually indistinguishable.
    let style = app.theme.style(R::PlayerLabel);
    buttons::render_segments(frame, app, area, &segments, style, style, Alignment::Center);
}

/// Build the transport status-line as `(target, text)` segments from app state — split out
/// from [`render_status_line`] so the conditional assembly (queue position, rating, shuffle /
/// repeat, speed, EQ, normalize, streaming mode, download tag) is unit-testable without a frame
/// buffer. A `None` target is a static label/spacing; spacing is its own label so a clickable
/// segment's hit rect hugs just its text.
fn status_line_parts(app: &App) -> Vec<(Option<MouseTarget>, Cow<'static, str>)> {
    let mut parts: Vec<(Option<MouseTarget>, Cow<'static, str>)> = Vec::with_capacity(16);
    // A braille throbber leads the line when the spinner animation is on (no-op otherwise). It's a
    // plain label, so `render_segments` keeps every later hit rect aligned to its rendered text.
    if let Some(spin) = crate::ui::anim::spinner_prefix(app) {
        parts.push((None, Cow::Owned(format!("{spin} "))));
    }
    // EAW-neutral glyphs (one cell everywhere) — the ⏸/▶ media emoji widen to two
    // cells on some terminals (Windows), which drifts every later segment's hit rect
    // off its rendered text and makes `R:`/`eq:` unclickable. See `render_controls`.
    let state = if app.playback.paused {
        t!("‖ paused", "‖ 일시정지")
    } else {
        t!("▸ playing", "▸ 재생 중")
    };
    parts.push((None, Cow::Borrowed(state)));
    if !app.queue.is_empty() {
        let (pos, _) = app.queue.position();
        // Clickable (opens the queue window) but styled exactly like the labels around it,
        // so the line looks unchanged — same as the `S:`/`R:` toggles next to it.
        parts.push((None, Cow::Borrowed("    ")));
        parts.push((
            Some(MouseTarget::QueuePos),
            Cow::Owned(format!("{pos}/{}", app.queue.len())),
        ));
    }
    // The current track's rating: one tri-state glyph (🤔/👍/👎) that replaces the old separate
    // ♥ favorite + ✗ dislike columns. Same `PlayerLabel` style and 4-space gap as `S:`/`R:` next
    // to it; click and the `f` key hit the same Action, so it mirrors the keyboard exactly.
    if let Some(cur) = app.queue.current() {
        let liked = app.library.is_favorite(&cur.video_id);
        let disliked = app.signals.is_disliked(&cur.video_id);
        parts.push((None, Cow::Borrowed("    ")));
        parts.push((
            Some(MouseTarget::Player(Action::CycleRating)),
            Cow::Borrowed(rating_glyph(liked, disliked)),
        ));
    }
    // Shuffle and repeat are both always shown as click toggles, so the line's layout never
    // shifts as they appear/disappear. Each carries its media glyph — `S:🔀` shuffle, `R:🔁`/`R:🔂`
    // repeat — or a cross when off, so they read the same in every UI language.
    parts.push((None, Cow::Borrowed("    ")));
    parts.push((
        Some(MouseTarget::Player(Action::ToggleShuffle)),
        Cow::Owned(format!("S:{}", if app.queue.shuffle { "🔀" } else { "✗" })),
    ));
    parts.push((None, Cow::Borrowed("    ")));
    parts.push((
        Some(MouseTarget::Player(Action::CycleRepeat)),
        Cow::Owned(format!("R:{}", app.queue.repeat.label())),
    ));
    if (app.playback.speed - 1.0).abs() > f64::EPSILON {
        parts.push((None, Cow::Owned(format!("    {:.1}x", app.playback.speed))));
    }
    parts.push((None, Cow::Borrowed("    ")));
    parts.push((
        Some(MouseTarget::EqMenu),
        Cow::Owned(format!("eq:{}", app.audio.preset.label())),
    ));
    // Faux VU bars trail the EQ label when the EQ-bars animation is on (no-op otherwise).
    if let Some(bars) = crate::ui::anim::eq_bars(app) {
        parts.push((None, Cow::Owned(format!("    {bars}"))));
    }
    if app.audio.normalize {
        parts.push((None, Cow::Owned(format!("    {}", t!("norm", "평준화")))));
    }
    if app.autoplay_streaming {
        // Show the station's mode (Focused/Balanced/Discovery) as a click target that opens the
        // mode dropdown — same affordance as the `eq:` label next to it.
        parts.push((None, Cow::Borrowed("    ")));
        parts.push((
            Some(MouseTarget::StreamingMenu),
            Cow::Owned(format!(
                "streaming:{}",
                app.config.streaming.mode.label().to_lowercase()
            )),
        ));
    }
    // Download indicator for the current track, if one is in flight or finished.
    if let Some(s) = app.queue.current()
        && let Some(state) = app.downloads.active.get(&s.video_id)
    {
        let tag = match state {
            DownloadState::Running(p) => format!("⬇ {p}%"),
            DownloadState::Done => "⬇ ✓".to_owned(),
            DownloadState::Failed => "⬇ ✗".to_owned(),
        };
        parts.push((None, Cow::Owned(format!("    {tag}"))));
    }

    parts
}

/// The queue window: a themed popup listing the whole play queue (current track marked),
/// opened by clicking the `N/M` position label. Rows are click targets — single-click
/// selects (drag to extend the selection), double-click jumps playback there — and each
/// row's trailing `✗` removes that track. Keyboard nav / Delete / Enter operate it while
/// open (see `App::on_key_queue`). Drawn last over everything so its rects win.
fn render_queue_popup(frame: &mut Frame, app: &App, area: Rect) {
    let total_len = app.queue.len();
    if total_len == 0 {
        return;
    }
    let (pos, total) = app.queue.position();
    let current = app.queue.cursor_pos();

    // ~60% wide, tall enough for the list but capped to ~70% of the screen.
    let box_w = (area.width * 3 / 5).clamp(24, area.width.saturating_sub(2).max(24));
    let max_h = (area.height * 7 / 10).max(3);
    let box_h = (total_len as u16 + 2).min(max_h);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(box_w) / 2,
        y: area.y + area.height.saturating_sub(box_h) / 2,
        width: box_w,
        height: box_h,
    }
    .intersection(area);
    if popup.is_empty() {
        return;
    }
    app.queue_popup.rect.set(Some(popup));

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(format!(" {} {pos}/{total} ", t!("Queue", "대기열")))
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

    const DEL_W: u16 = 2; // "✗ " click target on the right edge
    let body_w = list.width.saturating_sub(DEL_W) as usize;
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
        let marker = if is_current { "▸ " } else { "  " };
        let title = app.display_title(song);
        let artist = app.display_artist(song);
        let text = crate::ui::text::truncate_owned_to_width(
            format!("{marker}{:>3} {title} — {artist}", i + 1),
            body_w.saturating_sub(1),
        );

        let mut base = if selected {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
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

        // Trailing ✗ delete button, kept on the row's highlight when selected.
        let del_x = row.x + row.width.saturating_sub(DEL_W);
        let del_rect = Rect {
            x: del_x,
            y,
            width: DEL_W,
            height: 1,
        };
        let mut del_style = app.theme.style(R::Error);
        if selected {
            del_style = del_style.bg(app.theme.color(R::SelectionBg));
        }
        frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
        app.register_mouse_button(del_rect, MouseTarget::QueueDel(i));

        // The row body (everything left of the ✗) selects / jumps to the track.
        let body_rect = Rect {
            x: row.x,
            y,
            width: row.width.saturating_sub(DEL_W),
            height: 1,
        };
        app.register_mouse_button(body_rect, MouseTarget::QueueRow(i));
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

/// The EQ preset dropdown, anchored under the `eq:` label and listing the built-in presets
/// (the active one highlighted). Each row is a click target that selects that preset.
fn render_eq_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<(String, bool, MouseTarget)> = crate::eq::EqPreset::CYCLE
        .iter()
        .map(|p| {
            (
                p.label().to_owned(),
                *p == app.audio.preset,
                MouseTarget::EqSelect(*p),
            )
        })
        .collect();
    render_dropdown(frame, app, area, MouseTarget::EqMenu, " EQ ", &rows);
}

/// The streaming-mode dropdown, anchored under the `streaming:` label and listing the station modes
/// (the active one highlighted). Each row is a click target that switches the mode. Mirrors
/// the `eq:` dropdown exactly.
fn render_streaming_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let cur = app.config.streaming.mode;
    let rows: Vec<(String, bool, MouseTarget)> = crate::streaming::StreamingMode::CYCLE
        .iter()
        .map(|m| {
            (
                m.label().to_owned(),
                *m == cur,
                MouseTarget::StreamingSelect(*m),
            )
        })
        .collect();
    render_dropdown(
        frame,
        app,
        area,
        MouseTarget::StreamingMenu,
        t!(" Streaming ", " 스트리밍 "),
        &rows,
    );
}

/// Compact dropdown box width: the widest label + a one-cell left inset + two border columns.
fn dropdown_width<'a>(labels: impl Iterator<Item = &'a str>) -> u16 {
    labels
        .map(|l| UnicodeWidthStr::width(l) as u16)
        .max()
        .unwrap_or(0)
        + 1
        + 2
}

/// A compact status-line dropdown shared by the `eq:` and `streaming:` labels. Anchored under the
/// label whose hit rect matches `anchor_target`; titled `title`; lists `rows` of
/// `(label, is_active, click-target)`. The active row is a full-width highlight bar (no arrow
/// gutter) so the box stays exactly as wide as the longest label. Drawn over whatever is
/// beneath it with an opaque text-cell background; dismissed by picking a row or clicking elsewhere.
fn render_dropdown(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    anchor_target: MouseTarget,
    title: &str,
    rows: &[(String, bool, MouseTarget)],
) {
    // Anchor under the label, whose hit rect the status line just published.
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
    // Drop below the label; clamp against the right/bottom edges so the box stays on screen.
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
            break; // out of room (tiny terminal) — don't register off-box rows
        }
        let row = Rect {
            x: list.x,
            y: list.y + i,
            width: list.width,
            height: 1,
        };
        // One-cell inset, then pad to the full row width so the active row's highlight bar
        // spans it (the trailing cells read as the selection bar, not as empty box).
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

/// The transport strip under the seekbar: skip-back / play-pause / skip-forward, then a
/// volume nudge cluster (`- 50% +`). No boxes — the glyphs *are* the buttons, so it
/// reads like a real now-playing bar rather than a row of GUI buttons. Each control is
/// padded to a few cells so it's an easy click target. The toggle shows the action
/// (`▸` play when paused, `‖` pause when playing) and both glyphs are one cell, so the
/// strip never reflows. Volume lives here (not the status line) to anchor the `-`/`+`
/// controls to the number they change.
///
/// Every glyph is a plain non-emoji symbol (EAW-neutral, one cell everywhere) — unlike
/// the ⏮/⏯ media emoji, which some terminals widen to two cells and so drift the click
/// rects off the rendered glyph.
fn render_controls(frame: &mut Frame, app: &App, area: Rect) {
    let toggle = if app.playback.paused {
        " ▸ "
    } else {
        " ‖ "
    };
    let vol = format!("{}%", app.playback.volume);
    let segments = [
        Seg::button(MouseTarget::Player(Action::PrevTrack), " ⇤ "),
        Seg::label("   "),
        Seg::button(MouseTarget::Player(Action::TogglePause), toggle),
        Seg::label("   "),
        Seg::button(MouseTarget::Player(Action::NextTrack), " ⇥ "),
        Seg::label("      "),
        Seg::label(t!("vol ", "볼륨 ")),
        Seg::button(MouseTarget::Player(Action::VolDown), " - "),
        Seg::label(&vol),
        Seg::button(MouseTarget::Player(Action::VolUp), " + "),
    ];
    let widths: Vec<u16> = segments
        .iter()
        .map(|s| buttons::text_width(s.text))
        .collect();
    let total = widths.iter().copied().sum::<u16>();
    let volume_offset = widths[..6].iter().copied().sum::<u16>();
    let volume_width = widths[6..].iter().copied().sum::<u16>();
    let x = area.x + area.width.saturating_sub(total) / 2 + volume_offset;
    let width = if x < area.right() {
        volume_width.min(area.right().saturating_sub(x))
    } else {
        0
    };
    app.register_mouse_button(
        Rect {
            x,
            y: area.y,
            width,
            height: area.height.min(1),
        },
        MouseTarget::VolumeArea,
    );
    let controls = crate::ui::anim::controls_style(
        app,
        app.theme
            .style(R::PlayerControl)
            .add_modifier(Modifier::BOLD),
    );
    let labels = app.theme.style(R::PlayerLabel);
    buttons::render_segments(
        frame,
        app,
        area,
        &segments,
        controls,
        labels,
        Alignment::Center,
    );
}

/// Blank rows framing the album art: one between the transport status line and the art,
/// one between the art's bottom edge and the lyrics block.
const ART_TOP_GAP: u16 = 1;
const ART_LYRICS_GAP: u16 = 1;
/// Smallest lyrics window (rows) we keep below the art when both are shown.
const MIN_LYRICS_ROWS: u16 = 3;

/// The central area below the transport strip. Album art (when enabled and ready) sits at
/// the top, just under the status line; the lyrics panel (when toggled) starts right below
/// the art's real bottom edge. When album art is off the layout is unchanged — lyrics get
/// the whole area, and an empty area draws nothing.
fn render_filler(frame: &mut Frame, app: &App, area: Rect) {
    app.art.rect.set(None);
    if app.radio_dedicated_mode {
        render_radio_filler(frame, app, area);
        return;
    }
    match (app.art_active(), app.lyrics.visible) {
        // Art on top, lyrics right under it. The art is capped so a readable lyrics window
        // always remains, then lyrics start one row below the art's actual bottom (not at a
        // fixed split), so there's no dead gap between them.
        (true, true) => {
            let after_gap = area.height.saturating_sub(ART_TOP_GAP);
            // ~50% of the filler, but never so tall that lyrics drop below MIN_LYRICS_ROWS.
            let cap = (after_gap / 2).min(after_gap.saturating_sub(MIN_LYRICS_ROWS));
            let band = art_band(area, ART_TOP_GAP, cap);
            match draw_art(frame, app, band) {
                Some(art) => {
                    let lyrics_y = art.bottom().saturating_add(ART_LYRICS_GAP);
                    let lyrics_area = Rect {
                        x: area.x,
                        y: lyrics_y,
                        width: area.width,
                        height: area.bottom().saturating_sub(lyrics_y),
                    };
                    render_lyrics(frame, app, lyrics_area);
                }
                None => render_lyrics(frame, app, area),
            }
        }
        // Art only: medium size, capped to ~55% of the filler height, top-anchored. Canvas
        // animations fill the blank region below the art's real bottom edge (never over the art).
        (true, false) => {
            let cap = (u32::from(area.height) * 11 / 20) as u16;
            let band = art_band(area, ART_TOP_GAP, cap);
            let art = draw_art(frame, app, band);
            let below = art
                .map_or(area.y, |r| r.bottom())
                .saturating_add(ART_LYRICS_GAP);
            if below < area.bottom() {
                let zone = Rect {
                    x: area.x,
                    y: below,
                    width: area.width,
                    height: area.bottom() - below,
                };
                crate::ui::anim::render_canvas(frame, app, zone);
            }
        }
        (false, true) => render_lyrics(frame, app, area),
        // No art, no lyrics: the whole filler is blank — the maximum canvas. With every animation
        // off this stays empty (drawing nothing), exactly as before.
        (false, false) => crate::ui::anim::render_canvas(frame, app, area),
    }
}

fn render_radio_filler(frame: &mut Frame, app: &App, area: Rect) {
    let after_gap = area.height.saturating_sub(ART_TOP_GAP);
    let cap = if app.lyrics.visible {
        after_gap.saturating_sub(MIN_LYRICS_ROWS).min(12)
    } else {
        after_gap.min(14)
    };
    let band = art_band(area, ART_TOP_GAP, cap);
    match draw_radio_ascii(frame, app, band) {
        Some(radio) if app.lyrics.visible => {
            let lyrics_y = radio.bottom().saturating_add(ART_LYRICS_GAP);
            let lyrics_area = Rect {
                x: area.x,
                y: lyrics_y,
                width: area.width,
                height: area.bottom().saturating_sub(lyrics_y),
            };
            render_lyrics(frame, app, lyrics_area);
        }
        None if app.lyrics.visible => render_lyrics(frame, app, area),
        _ => {}
    }
}

fn draw_radio_ascii(frame: &mut Frame, app: &App, area: Rect) -> Option<Rect> {
    if area.width < 12 || area.height < 3 {
        return None;
    }
    const LARGE: &[&str] = &[
        "          .----------------.",
        "         / .--------------. \\",
        "        / /     RADIO      \\ \\",
        "       | |   .---------.   | |",
        "       | |   |  .---.  |   | |",
        "       | |   |  |   |  |   | |",
        "       | |   '---------'   | |",
        "       | |  o   o   o   o  | |",
        "        \\ \\  ~~~~~~~~~~~  / /",
        "         \\ '--------------' /",
        "          '------.  .------'",
        "                 |__|       ",
    ];
    const SMALL: &[&str] = &[
        "  .----------.  ",
        " /   RADIO   \\ ",
        "|  o  ===  o  |",
        "|  [_______]  |",
        " \\__________/ ",
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
    let lines: Vec<Line> = art
        .iter()
        .take(height as usize)
        .map(|line| {
            Line::from((*line).to_owned())
                .style(style)
                .alignment(Alignment::Center)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rect);
    Some(rect)
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

/// Draw the current track's album art / thumbnail top-anchored within `band` and centered
/// horizontally by its true aspect ratio (square covers stay square, 16:9 thumbnails stay
/// wide — the picker knows the terminal's font cell size). Returns the rect the art
/// actually occupies so the caller can start the lyrics right below it, or `None` when the
/// band is too small to render anything legible. Only called when `app.art_active()`, so
/// the protocol is present; with no graphics protocol this renders as unicode half-blocks.
/// `Resize::Scale` lets a small source thumbnail grow to fill the rect (the default `Fit`
/// never upscales).
fn draw_art(frame: &mut Frame, app: &App, band: Rect) -> Option<Rect> {
    // Below a few cells there's nothing but mush; skip so a tiny terminal stays clean.
    if band.width < 6 || band.height < 3 {
        return None;
    }
    let mut rect = app.art_fit_rect(band);
    rect.y = band.y; // art_fit_rect centers vertically; re-anchor to the top of the band.
    app.art.rect.set(Some(rect));
    if let Some(proto) = app.art.protocol.borrow_mut().as_mut() {
        frame.render_stateful_widget(
            StatefulImage::new().resize(Resize::Scale(Some(FilterType::Lanczos3))),
            rect,
            proto,
        );
    }
    Some(rect)
}

/// The synced-lyrics panel: a window of lines centered on the current one, which is
/// highlighted. Auto-scrolls as `time-pos` advances.
fn render_lyrics(frame: &mut Frame, app: &App, area: Rect) {
    let centered = |s: &str, style: Style| {
        Line::from(s.to_owned())
            .style(style)
            .alignment(Alignment::Center)
    };
    let dim = app.theme.style(R::LyricsDim);

    let lines = match &app.lyrics.track {
        Some(t) if !t.lines.is_empty() => &t.lines,
        _ => {
            let msg = if app.lyrics.loading {
                t!("Searching lyrics…", "가사 검색 중…")
            } else if app.lyrics.track.is_some() {
                t!("No synced lyrics found.", "동기화된 가사가 없어요.")
            } else {
                t!("Fetching lyrics…", "가사 가져오는 중…")
            };
            frame.render_widget(Paragraph::new(centered(msg, dim)), area);
            return;
        }
    };

    let height = area.height as usize;
    if height == 0 {
        return;
    }
    let pos = app.playback.time_pos.unwrap_or(0.0);
    let cur = lyrics::current_index(lines, pos);
    // Keep the current line vertically centered.
    let start = cur.unwrap_or(0).saturating_sub(height / 2);

    let current_style = app
        .theme
        .style(R::LyricsCurrent)
        .add_modifier(Modifier::BOLD);
    let rendered: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(i, l)| {
            let style = if Some(i) == cur { current_style } else { dim };
            centered(&l.text, style)
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), area);
}

/// A modal confirmation for entering/leaving dedicated Radio mode. It intentionally mirrors the
/// Settings reset confirmation shape so broad UI-mode switches use the same interaction pattern.
pub fn render_radio_mode_confirm(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    confirm: RadioModeConfirm,
) {
    let popup = centered_fixed(area, 60, 9);
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
        Constraint::Min(1),
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
            t!(" Confirm (Enter) ", " 확인 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelRadioMode,
            t!(" Cancel (Esc) ", " 취소 (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;

    fn has_target(
        parts: &[(Option<MouseTarget>, Cow<'static, str>)],
        pred: impl Fn(&MouseTarget) -> bool,
    ) -> bool {
        parts.iter().any(|(t, _)| t.as_ref().is_some_and(&pred))
    }

    #[test]
    fn status_line_always_offers_shuffle_repeat_and_eq() {
        let app = App::new(100);
        let parts = status_line_parts(&app);
        // Shuffle, repeat and the EQ menu are always present so the line never reflows.
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::ToggleShuffle)
        )));
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRepeat)
        )));
        assert!(has_target(&parts, |t| matches!(t, MouseTarget::EqMenu)));
        // Idle queue: no position label and no rating glyph (nothing is current).
        assert!(!has_target(&parts, |t| matches!(t, MouseTarget::QueuePos)));
        assert!(!has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRating)
        )));
    }

    #[test]
    fn status_line_shows_position_and_rating_once_a_track_is_current() {
        let mut app = App::new(100);
        app.queue.set(vec![Song::remote("a", "A", "x", "1:00")], 0);
        let parts = status_line_parts(&app);
        // The clickable N/M position label appears, reading "1/1".
        assert!(
            parts
                .iter()
                .any(|(t, s)| matches!(t, Some(MouseTarget::QueuePos)) && s == "1/1")
        );
        // A current track means the tri-state rating glyph is offered.
        assert!(has_target(&parts, |t| matches!(
            t,
            MouseTarget::Player(Action::CycleRating)
        )));
    }
}
