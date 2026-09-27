//! Radio-mode decoration around the set piece: the scrolling motif separator and when the
//! radio art animates.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::theme::ThemeRole as R;

pub(super) fn render_art_animation_separator(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // The trailing space is deliberate: the motif is tiled edge-to-edge, and it keeps
    // each repetition from butting straight into the next one's leading note.
    const MOTIF: &str = "♫♪.ılılıll|̲̅●̲̅|̲̅=̲̅|̲̅●̲̅|llılılı.♫♪ ";
    // Clustered once — the motif is a compile-time constant and this runs every radio frame.
    static MOTIF_CLUSTERS: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| display_clusters(MOTIF));
    let width = usize::from(area.width);
    let offset = if radio_art_animation_on(app) {
        (app.anim_frame() / 6) as usize
    } else {
        0
    };
    let line = repeated_motif_line(&MOTIF_CLUSTERS, width, offset);
    frame.render_widget(
        Paragraph::new(
            Line::from(line)
                .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
                .alignment(Alignment::Center),
        ),
        area,
    );
}

fn repeated_motif_line(clusters: &[String], width: usize, offset: usize) -> String {
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

pub(super) fn radio_art_animation_on(app: &App) -> bool {
    app.radio_dedicated_mode
        && app.animations().master
        && !app.playback.paused
        && app.queue.current().is_some()
}
