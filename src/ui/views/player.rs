//! The now-playing view: current track, a seekbar with `M:SS / M:SS`, transport state,
//! and queue indicators (position, shuffle, repeat).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph};

use crate::app::{App, DownloadState, MouseTarget};
use crate::keymap::Action;
use crate::lyrics;
use crate::ui::buttons::{self, Seg};
use crate::util::format;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" ytm-tui ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Length(1), // spacer
        Constraint::Length(1), // seekbar
        Constraint::Length(1), // mouse controls
        Constraint::Length(1), // transport status
        Constraint::Min(0),    // filler
        Constraint::Length(1), // help
    ])
    .split(inner);

    // Title (or an error, if playback failed).
    let title = if !app.status.is_empty() {
        Line::from(app.status.clone()).fg(Color::Red).alignment(Alignment::Center)
    } else {
        let text = match app.queue.current() {
            Some(s) => {
                let heart = if app.library.is_favorite(&s.video_id) { "♥ " } else { "" };
                format!("{heart}{} — {}", s.title, s.artist)
            }
            None => "Nothing playing — press / to search".to_owned(),
        };
        Line::from(text.bold()).alignment(Alignment::Center)
    };
    frame.render_widget(Paragraph::new(title), rows[0]);

    // Seekbar.
    let pos = app.time_pos.unwrap_or(0.0);
    let dur = app.duration.unwrap_or(0.0);
    let ratio = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) } else { 0.0 };
    let seekbar = Gauge::default()
        .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
        .ratio(ratio)
        .label(format!(
            "{} / {}",
            format::time(pos),
            if dur > 0.0 { format::time(dur) } else { "--:--".to_owned() }
        ));
    frame.render_widget(seekbar, rows[2]);
    // Publish the seekbar's screen rect so a mouse click can be hit-tested for seeking.
    app.seekbar_rect.set(Some(rows[2]));

    render_controls(frame, app, rows[3]);

    render_status_line(frame, app, rows[4]);

    // Lyrics panel (toggled with `L`) fills the central area when shown.
    if app.lyrics_visible {
        render_lyrics(frame, app, rows[5]);
    }

    // The full key list lives in the `?` cheat-sheet now; the footer just points to it
    // (chord pulled live from the keymap, so a remap of "toggle help" updates it).
    buttons::render_help_button(frame, app, rows[6], Alignment::Center);

    // The EQ preset dropdown draws last so its rows sit on top and win hit-testing.
    if app.eq_dropdown_open {
        render_eq_dropdown(frame, app, inner);
    }
}

/// The transport status line: state, queue position, shuffle, repeat, speed, EQ, etc.
///
/// Rendered as click segments rather than one string so `repeat:` (toggles repeat) and
/// `eq:` (opens the preset dropdown) are mouse targets — but every segment shares the same
/// cyan style, so the line looks exactly like the plain status text it replaced. `eq:` is
/// always shown now (so the dropdown is always reachable); the rest stay conditional.
fn render_status_line(frame: &mut Frame, app: &App, area: Rect) {
    // (target, text); a `None` target is static label/spacing. Spacing is split into its
    // own label so a clickable segment's hit rect hugs just its text.
    let mut parts: Vec<(Option<MouseTarget>, String)> = Vec::new();
    let state = if app.paused { "⏸  paused" } else { "▶ playing" };
    parts.push((None, state.to_owned()));
    if !app.queue.is_empty() {
        let (pos, _) = app.queue.position();
        parts.push((None, format!("    {pos}/{}", app.queue.len())));
    }
    if app.queue.shuffle {
        parts.push((None, "    shuffle".to_owned()));
    }
    parts.push((None, "    ".to_owned()));
    parts.push((
        Some(MouseTarget::Player(Action::CycleRepeat)),
        format!("repeat:{}", app.queue.repeat.label()),
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
        parts.push((None, "    radio".to_owned()));
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
    let style = Style::default().fg(Color::Cyan);
    buttons::render_segments(frame, app, area, &segments, style, style, Alignment::Center);
}

/// The EQ preset dropdown, anchored under the `eq:` label and listing the built-in presets
/// (the active one marked). Each row is a click target that selects that preset. Drawn over
/// whatever is beneath it (`Clear`); dismissed by picking a preset or clicking elsewhere.
fn render_eq_dropdown(frame: &mut Frame, app: &App, area: Rect) {
    // Anchor under the `eq:` label, whose hit rect the status line just published.
    let Some(anchor) = app
        .mouse_buttons
        .borrow()
        .iter()
        .find(|b| b.target == MouseTarget::EqMenu)
        .map(|b| b.rect)
    else {
        return;
    };

    let presets = crate::eq::EqPreset::CYCLE;
    // Widest label is "Treble Boost" (12) + a 2-cell marker + 2 border columns.
    let box_w = 16u16;
    let box_h = presets.len() as u16 + 2;
    // Drop below the label; clamp against the right/bottom edges so the box stays on screen.
    let x = anchor.x.min(area.right().saturating_sub(box_w));
    let y = (anchor.y + 1).min(area.bottom().saturating_sub(box_h));
    let popup = Rect { x, y, width: box_w, height: box_h }.intersection(area);
    if popup.is_empty() {
        return;
    }

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(" EQ ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let list = block.inner(popup);
    frame.render_widget(block, popup);

    for (i, preset) in presets.iter().enumerate() {
        let i = i as u16;
        if i >= list.height {
            break; // out of room (tiny terminal) — don't register off-box rows
        }
        let row = Rect { x: list.x, y: list.y + i, width: list.width, height: 1 };
        let selected = *preset == app.eq_preset;
        let marker = if selected { "▸ " } else { "  " };
        let style = if selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let line = Line::from(format!("{marker}{}", preset.label())).style(style);
        frame.render_widget(Paragraph::new(line), row);
        app.register_mouse_button(row, MouseTarget::EqSelect(*preset));
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
    let controls = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
    let labels = Style::default().fg(Color::Cyan);
    buttons::render_segments(frame, app, area, &segments, controls, labels, Alignment::Center);
}

/// The synced-lyrics panel: a window of lines centered on the current one, which is
/// highlighted. Auto-scrolls as `time-pos` advances.
fn render_lyrics(frame: &mut Frame, app: &App, area: Rect) {
    let centered = |s: &str, style: Style| Line::from(s.to_owned()).style(style).alignment(Alignment::Center);
    let dim = Style::default().fg(Color::DarkGray);

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

    let current_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
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
