//! The now-playing view: current track, a seekbar with `M:SS / M:SS`, transport state,
//! and queue indicators (position, shuffle, repeat).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph};
use ratatui_image::{Resize, StatefulImage};
use image::imageops::FilterType;

use crate::app::{App, DownloadState, MouseTarget};
use crate::keymap::Action;
use crate::lyrics;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};
use crate::util::format;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // nav bar
        Constraint::Length(1), // title
        Constraint::Length(1), // spacer
        Constraint::Length(1), // seekbar
        Constraint::Length(1), // mouse controls
        Constraint::Length(1), // transport status
        Constraint::Min(0),    // filler
        Constraint::Length(1), // help
    ])
    .split(inner);

    buttons::render_nav(frame, app, rows[0]);

    // Title (or an error, if playback failed).
    let title = if !app.status.is_empty() {
        Line::from(app.status.clone())
            .style(app.theme.style(R::Error))
            .alignment(Alignment::Center)
    } else {
        let text = match app.queue.current() {
            Some(s) => {
                let heart = if app.library.is_favorite(&s.video_id) { "♥ " } else { "" };
                format!("{heart}{} — {}", s.title, s.artist)
            }
            None => "Nothing playing — press / to search".to_owned(),
        };
        Line::from(text.bold())
            .style(app.theme.style(R::TextPrimary))
            .alignment(Alignment::Center)
    };
    frame.render_widget(Paragraph::new(title), rows[1]);

    // Seekbar.
    let pos = app.time_pos.unwrap_or(0.0);
    let dur = app.duration.unwrap_or(0.0);
    let ratio = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) } else { 0.0 };
    let seekbar = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(app.theme.color(R::GaugeFilled))
                .bg(app.theme.color(R::GaugeEmpty)),
        )
        .ratio(ratio)
        .label(format!(
            "{} / {}",
            format::time(pos),
            if dur > 0.0 { format::time(dur) } else { "--:--".to_owned() }
        ));
    frame.render_widget(seekbar, rows[3]);
    // Publish the seekbar's screen rect so a mouse click can be hit-tested for seeking.
    app.seekbar_rect.set(Some(rows[3]));

    render_controls(frame, app, rows[4]);

    render_status_line(frame, app, rows[5]);

    // Central filler: album art (top) and/or the lyrics panel (below). With album art off
    // this is exactly the old behaviour — lyrics fill the whole area, nothing else draws.
    render_filler(frame, app, rows[6]);

    // The full key list lives in the `?` cheat-sheet now; the footer just points to it
    // (chord pulled live from the keymap, so a remap of "toggle help" updates it).
    buttons::render_help_button(frame, app, rows[7]);

    // The status-line dropdowns draw over the screen so their rows win hit-testing.
    if app.eq_dropdown_open {
        render_eq_dropdown(frame, app, inner);
    }
    if app.radio_dropdown_open {
        render_radio_dropdown(frame, app, inner);
    }
    // The queue window draws last of all so it sits on top and its rects win.
    app.queue_popup_rect.set(None);
    if app.queue_popup_open {
        render_queue_popup(frame, app, inner);
    }
}

/// The transport status line: state, queue position, shuffle, repeat, speed, EQ, etc.
///
/// Rendered as click segments rather than one string so `R:` (toggles repeat) and
/// `eq:` (opens the preset dropdown) are mouse targets — but every segment shares the same
/// cyan style, so the line looks exactly like the plain status text it replaced. `eq:` is
/// always shown now (so the dropdown is always reachable); the rest stay conditional.
fn render_status_line(frame: &mut Frame, app: &App, area: Rect) {
    // (target, text); a `None` target is static label/spacing. Spacing is split into its
    // own label so a clickable segment's hit rect hugs just its text.
    let mut parts: Vec<(Option<MouseTarget>, String)> = Vec::new();
    // EAW-neutral glyphs (one cell everywhere) — the ⏸/▶ media emoji widen to two
    // cells on some terminals (Windows), which drifts every later segment's hit rect
    // off its rendered text and makes `R:`/`eq:` unclickable. See `render_controls`.
    let state = if app.paused { "‖ paused" } else { "▸ playing" };
    parts.push((None, state.to_owned()));
    if !app.queue.is_empty() {
        let (pos, _) = app.queue.position();
        // Clickable (opens the queue window) but styled exactly like the labels around it,
        // so the line looks unchanged — same as the `S:`/`R:` toggles next to it.
        parts.push((None, "    ".to_owned()));
        parts.push((Some(MouseTarget::QueuePos), format!("{pos}/{}", app.queue.len())));
    }
    // Like / dislike toggles for the current track. Same `PlayerLabel` style and 4-space
    // gaps as `S:`/`R:` next to them, so they read as one aligned, non-intrusive row; click
    // and key (`f` / `x`) hit the same Action, so the buttons mirror the keyboard exactly.
    if let Some(cur) = app.queue.current() {
        let liked = app.library.is_favorite(&cur.video_id);
        let disliked = app.signals.is_disliked(&cur.video_id);
        parts.push((None, "    ".to_owned()));
        parts.push((
            Some(MouseTarget::Player(Action::Favorite)),
            format!("♥:{}", if liked { "on" } else { "off" }),
        ));
        parts.push((None, "    ".to_owned()));
        parts.push((
            Some(MouseTarget::Player(Action::Dislike)),
            format!("✗:{}", if disliked { "on" } else { "off" }),
        ));
    }
    // Shuffle and repeat are both always shown as click toggles, so the line's layout never
    // shifts as they flip — `S:on/off` mirrors the `R:` button next to it (stable UI).
    parts.push((None, "    ".to_owned()));
    parts.push((
        Some(MouseTarget::Player(Action::ToggleShuffle)),
        format!("S:{}", if app.queue.shuffle { "on" } else { "off" }),
    ));
    parts.push((None, "    ".to_owned()));
    parts.push((
        Some(MouseTarget::Player(Action::CycleRepeat)),
        format!("R: {}", app.queue.repeat.label()),
    ));
    if (app.speed - 1.0).abs() > f64::EPSILON {
        parts.push((None, format!("    {:.1}x", app.speed)));
    }
    parts.push((None, "    ".to_owned()));
    parts.push((Some(MouseTarget::EqMenu), format!("eq:{}", app.eq_preset.label())));
    if app.normalize {
        parts.push((None, "    norm".to_owned()));
    }
    if app.autoplay_radio {
        // Show the station's mode (Focused/Balanced/Discovery) as a click target that opens the
        // mode dropdown — same affordance as the `eq:` label next to it.
        parts.push((None, "    ".to_owned()));
        parts.push((
            Some(MouseTarget::RadioMenu),
            format!("radio:{}", app.config.radio.mode.label().to_lowercase()),
        ));
    }
    // Download indicator for the current track, if one is in flight or finished.
    if let Some(s) = app.queue.current()
        && let Some(state) = app.downloads.get(&s.video_id)
    {
        let tag = match state {
            DownloadState::Running(p) => format!("⬇ {p}%"),
            DownloadState::Done => "⬇ ✓".to_owned(),
            DownloadState::Failed => "⬇ ✗".to_owned(),
        };
        parts.push((None, format!("    {tag}")));
    }

    let segments: Vec<Seg> = parts
        .iter()
        .map(|(target, text)| match target {
            Some(t) => Seg::button(*t, text.as_str()),
            None => Seg::label(text.as_str()),
        })
        .collect();
    // Same style for buttons and labels so the clickable parts are visually indistinguishable.
    let style = app.theme.style(R::PlayerLabel);
    buttons::render_segments(frame, app, area, &segments, style, style, Alignment::Center);
}

/// The queue window: a themed popup listing the whole play queue (current track marked),
/// opened by clicking the `N/M` position label. Rows are click targets — single-click
/// selects (drag to extend the selection), double-click jumps playback there — and each
/// row's trailing `✗` removes that track. Keyboard nav / Delete / Enter operate it while
/// open (see `App::on_key_queue`). Drawn last over everything (`Clear`) so its rects win.
fn render_queue_popup(frame: &mut Frame, app: &App, area: Rect) {
    let songs = app.queue.ordered();
    if songs.is_empty() {
        return;
    }
    let (pos, total) = app.queue.position();
    let current = app.queue.cursor_pos();

    // ~60% wide, tall enough for the list but capped to ~70% of the screen.
    let box_w = (area.width * 3 / 5).clamp(24, area.width.saturating_sub(2).max(24));
    let max_h = (area.height * 7 / 10).max(3);
    let box_h = (songs.len() as u16 + 2).min(max_h);
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
    app.queue_popup_rect.set(Some(popup));

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(" Queue {pos}/{total} "))
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let list = block.inner(popup);
    frame.render_widget(block, popup);
    if list.height == 0 || list.width < 4 {
        return;
    }

    // Scroll so the cursor row stays visible (window roughly centered on it).
    let visible = list.height as usize;
    let cursor = app.queue_popup_cursor.min(songs.len() - 1);
    let start = cursor
        .saturating_sub(visible / 2)
        .min(songs.len().saturating_sub(visible));
    let sel_lo = app.queue_popup_cursor.min(app.queue_popup_anchor);
    let sel_hi = app.queue_popup_cursor.max(app.queue_popup_anchor);

    const DEL_W: u16 = 2; // "✗ " click target on the right edge
    let body_w = list.width.saturating_sub(DEL_W) as usize;
    for (vis, (i, song)) in songs.iter().enumerate().skip(start).take(visible).enumerate() {
        let y = list.y + vis as u16;
        let selected = i >= sel_lo && i <= sel_hi;
        let is_current = i == current;
        let marker = if is_current { "▸ " } else { "  " };
        let text = format!("{marker}{:>3} {} — {}", i + 1, song.title, song.artist);
        let text: String = text.chars().take(body_w.saturating_sub(1)).collect();

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
        let row = Rect { x: list.x, y, width: list.width, height: 1 };
        frame.render_widget(Paragraph::new(Line::from(text).style(base)), row);

        // Trailing ✗ delete button, kept on the row's highlight when selected.
        let del_x = row.x + row.width.saturating_sub(DEL_W);
        let del_rect = Rect { x: del_x, y, width: DEL_W, height: 1 };
        let mut del_style = app.theme.style(R::Error);
        if selected {
            del_style = del_style.bg(app.theme.color(R::SelectionBg));
        }
        frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
        app.register_mouse_button(del_rect, MouseTarget::QueueDel(i));

        // The row body (everything left of the ✗) selects / jumps to the track.
        let body_rect = Rect { x: row.x, y, width: row.width.saturating_sub(DEL_W), height: 1 };
        app.register_mouse_button(body_rect, MouseTarget::QueueRow(i));
    }
}

/// The EQ preset dropdown, anchored under the `eq:` label and listing the built-in presets
/// (the active one highlighted). Each row is a click target that selects that preset.
fn render_eq_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<(String, bool, MouseTarget)> = crate::eq::EqPreset::CYCLE
        .iter()
        .map(|p| (p.label().to_owned(), *p == app.eq_preset, MouseTarget::EqSelect(*p)))
        .collect();
    render_dropdown(frame, app, area, MouseTarget::EqMenu, " EQ ", &rows);
}

/// The radio-mode dropdown, anchored under the `radio:` label and listing the station modes
/// (the active one highlighted). Each row is a click target that switches the mode. Mirrors
/// the `eq:` dropdown exactly.
fn render_radio_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    let cur = app.config.radio.mode;
    let rows: Vec<(String, bool, MouseTarget)> = crate::radio::RadioMode::CYCLE
        .iter()
        .map(|m| (m.label().to_owned(), *m == cur, MouseTarget::RadioSelect(*m)))
        .collect();
    render_dropdown(frame, app, area, MouseTarget::RadioMenu, " Radio ", &rows);
}

/// Compact dropdown box width: the widest label + a one-cell left inset + two border columns.
fn dropdown_width<'a>(labels: impl Iterator<Item = &'a str>) -> u16 {
    labels.map(|l| l.chars().count() as u16).max().unwrap_or(0) + 1 + 2
}

/// A compact status-line dropdown shared by the `eq:` and `radio:` labels. Anchored under the
/// label whose hit rect matches `anchor_target`; titled `title`; lists `rows` of
/// `(label, is_active, click-target)`. The active row is a full-width highlight bar (no arrow
/// gutter) so the box stays exactly as wide as the longest label. Drawn over whatever is
/// beneath it (`Clear`); dismissed by picking a row or clicking elsewhere.
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
    let popup = Rect { x, y, width: box_w, height: box_h }.intersection(area);
    if popup.is_empty() {
        return;
    }

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::BorderPrimary))
        .style(app.theme.style(R::TextPrimary));
    let list = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, (label, active, target)) in rows.iter().enumerate() {
        let i = i as u16;
        if i >= list.height {
            break; // out of room (tiny terminal) — don't register off-box rows
        }
        let row = Rect { x: list.x, y: list.y + i, width: list.width, height: 1 };
        // One-cell inset, then pad to the full row width so the active row's highlight bar
        // spans it (the trailing cells read as the selection bar, not as empty box).
        let mut text = format!(" {label}");
        let pad = (list.width as usize).saturating_sub(text.chars().count());
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
    let toggle = if app.paused { " ▸ " } else { " ‖ " };
    let vol = format!("{}%", app.volume);
    let segments = [
        Seg::button(MouseTarget::Player(Action::PrevTrack), " ⇤ "),
        Seg::label("   "),
        Seg::button(MouseTarget::Player(Action::TogglePause), toggle),
        Seg::label("   "),
        Seg::button(MouseTarget::Player(Action::NextTrack), " ⇥ "),
        Seg::label("      "),
        Seg::label("vol "),
        Seg::button(MouseTarget::Player(Action::VolDown), " - "),
        Seg::label(&vol),
        Seg::button(MouseTarget::Player(Action::VolUp), " + "),
    ];
    let controls = app.theme.style(R::PlayerControl).add_modifier(Modifier::BOLD);
    let labels = app.theme.style(R::PlayerLabel);
    buttons::render_segments(frame, app, area, &segments, controls, labels, Alignment::Center);
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
    match (app.art_active(), app.lyrics_visible) {
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
        // Art only: medium size, capped to ~55% of the filler height, top-anchored.
        (true, false) => {
            let cap = (u32::from(area.height) * 11 / 20) as u16;
            let band = art_band(area, ART_TOP_GAP, cap);
            draw_art(frame, app, band);
        }
        (false, true) => render_lyrics(frame, app, area),
        (false, false) => {}
    }
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
    if let Some(proto) = app.art.borrow_mut().as_mut() {
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
    let centered = |s: &str, style: Style| Line::from(s.to_owned()).style(style).alignment(Alignment::Center);
    let dim = app.theme.style(R::LyricsDim);

    let lines = match &app.lyrics {
        Some(t) if !t.lines.is_empty() => &t.lines,
        _ => {
            let msg = if app.lyrics_loading {
                "Searching lyrics…"
            } else if app.lyrics.is_some() {
                "No synced lyrics found."
            } else {
                "Fetching lyrics…"
            };
            frame.render_widget(Paragraph::new(centered(msg, dim)), area);
            return;
        }
    };

    let height = area.height as usize;
    if height == 0 {
        return;
    }
    let pos = app.time_pos.unwrap_or(0.0);
    let cur = lyrics::current_index(lines, pos);
    // Keep the current line vertically centered.
    let start = cur.unwrap_or(0).saturating_sub(height / 2);

    let current_style = app.theme.style(R::LyricsCurrent).add_modifier(Modifier::BOLD);
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
