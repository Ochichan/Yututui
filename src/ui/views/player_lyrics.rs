//! Synced-lyrics rendering and its frame-local mouse hit map.

use std::sync::Arc;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
use crate::t;
use crate::theme::ThemeRole as R;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let dim = app.theme.style(R::LyricsDim);
    let Some(track) = app.current_loaded_lyrics() else {
        render_empty(frame, app, area, dim);
        return;
    };
    if area.height == 0 {
        return;
    }

    let current = app
        .lyrics
        .active_index
        .filter(|index| *index < track.lines.len());
    let start = current
        .unwrap_or(0)
        .saturating_sub(usize::from(area.height) / 2);
    let current_style = crate::ui::anim::lyrics_current_style(
        app,
        app.theme
            .style(R::LyricsCurrent)
            .add_modifier(Modifier::BOLD),
    );

    for (row, (line_index, lyric)) in track
        .lines
        .iter()
        .enumerate()
        .skip(start)
        .take(usize::from(area.height))
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + row as u16, area.width, 1);
        let style = if Some(line_index) == current {
            current_style
        } else {
            let distance = current.map_or(0, |index| index.abs_diff(line_index));
            crate::ui::anim::lyrics_dim_style(app, dim, distance)
        };
        frame.render_widget(Paragraph::new(centered(&lyric.text, style)), rect);
        app.register_mouse_button(
            rect,
            MouseTarget::LyricsLine {
                video_id: Arc::clone(&track.video_id),
                line_index,
            },
        );
    }

    render_delay_osd(frame, app, area, &track.video_id);
}

fn render_empty(frame: &mut Frame, app: &App, area: Rect, dim: Style) {
    let base = if app.lyrics.loading {
        t!("Searching lyrics", "가사 검색 중")
    } else if app.lyrics.track.is_some() {
        t!("No synced lyrics found.", "동기화된 가사가 없어요.")
    } else {
        t!("Fetching lyrics", "가사 가져오는 중")
    };
    let message = if app.lyrics.track.is_some() && !app.lyrics.loading {
        base.to_owned()
    } else if let Some(dots) = crate::ui::anim::activity_dots(app) {
        format!("{base}{dots}")
    } else {
        format!("{base}…")
    };
    frame.render_widget(Paragraph::new(centered(&message, dim)), area);
}

fn render_delay_osd(frame: &mut Frame, app: &App, area: Rect, video_id: &Arc<str>) {
    if area.width < 3 || area.height == 0 {
        return;
    }
    let expanded = app
        .lyrics
        .delay_osd_until
        .is_some_and(|deadline| deadline > Instant::now());
    if !expanded {
        return render_handle(frame, app, area, video_id);
    }
    let value = delay_label(app);
    let expanded_label = format!("[ − {value} + ]");
    if expanded_label.width() > usize::from(area.width) {
        return render_handle(frame, app, area, video_id);
    }

    let width = expanded_label.width() as u16;
    let rect = Rect::new(area.right() - width, area.bottom() - 1, width, 1);
    let base = Style::default()
        .fg(app.theme.color(R::TextMuted))
        .bg(app.theme.color(R::Background));
    let accent = Style::default()
        .fg(app.theme.color(R::Accent))
        .bg(app.theme.color(R::Background))
        .add_modifier(Modifier::BOLD);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[ ", base),
            Span::styled("−", accent),
            Span::styled(format!(" {value} "), base),
            Span::styled("+", accent),
            Span::styled(" ]", base),
        ])),
        rect,
    );

    // Rows were registered first. This full blocker consumes the value and padding, and the
    // two one-cell action targets are registered last so only the visible buttons win.
    app.register_mouse_button(rect, MouseTarget::LyricsDelayBlock);
    app.register_mouse_button(
        Rect::new(rect.x + 2, rect.y, 1, 1),
        MouseTarget::LyricsDelayEarlier {
            video_id: Arc::clone(video_id),
        },
    );
    app.register_mouse_button(
        Rect::new(rect.right() - 3, rect.y, 1, 1),
        MouseTarget::LyricsDelayLater {
            video_id: Arc::clone(video_id),
        },
    );
}

fn render_handle(frame: &mut Frame, app: &App, area: Rect, video_id: &Arc<str>) {
    let rect = Rect::new(area.right() - 3, area.bottom() - 1, 3, 1);
    let style = Style::default()
        .fg(app.theme.color(R::Accent))
        .bg(app.theme.color(R::Background))
        .add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(Line::styled("[±]", style)), rect);
    app.register_mouse_button(
        rect,
        MouseTarget::LyricsDelayHandle {
            video_id: Arc::clone(video_id),
        },
    );
}

fn delay_label(app: &App) -> String {
    let seconds = app.lyrics.delay.seconds();
    if app.lyrics.delay.steps() == 0 {
        "0.0s".to_owned()
    } else if seconds.is_sign_negative() {
        format!("−{:.1}s", seconds.abs())
    } else {
        format!("+{seconds:.1}s")
    }
}

fn centered(text: &str, style: Style) -> Line<'_> {
    Line::from(text).style(style).alignment(Alignment::Center)
}
