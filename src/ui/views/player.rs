//! The now-playing view: current track, a seekbar with `M:SS / M:SS`, transport state,
//! and queue indicators (position, shuffle, repeat).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

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

    // Transport status line: state, queue position, shuffle, repeat. (Volume sits in
    // the transport strip above, next to its `-`/`+` controls.)
    let state = if app.paused { "⏸  paused" } else { "▶ playing" };
    let mut info = state.to_owned();
    if !app.queue.is_empty() {
        let (pos, _) = app.queue.position();
        info.push_str(&format!("    {pos}/{}", app.queue.len()));
    }
    if app.queue.shuffle {
        info.push_str("    shuffle");
    }
    info.push_str(&format!("    repeat:{}", app.queue.repeat.label()));
    if (app.speed - 1.0).abs() > f64::EPSILON {
        info.push_str(&format!("    {:.1}x", app.speed));
    }
    if app.eq_preset != crate::eq::EqPreset::Flat {
        info.push_str(&format!("    eq:{}", app.eq_preset.label()));
    }
    if app.normalize {
        info.push_str("    norm");
    }
    if app.autoplay_radio {
        info.push_str("    radio");
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
        info.push_str(&format!("    {tag}"));
    }
    let status = Line::from(info).fg(Color::Cyan).alignment(Alignment::Center);
    frame.render_widget(Paragraph::new(status), rows[4]);

    // Lyrics panel (toggled with `L`) fills the central area when shown.
    if app.lyrics_visible {
        render_lyrics(frame, app, rows[5]);
    }

    // The full key list lives in the `?` cheat-sheet now; the footer just points to it
    // (chord pulled live from the keymap, so a remap of "toggle help" updates it).
    buttons::render_help_button(frame, app, rows[6], Alignment::Center);
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
