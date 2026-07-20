//! Trailing actions shared by every row in the player queue popup.
//!
//! Keeping the action geometry here leaves `player.rs` below its size ratchet and, more
//! importantly, gives the row body one source of truth for the cells it must not claim.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::{App, MouseTarget};
use crate::theme::ThemeRole as R;

const ACTION_WIDTH: u16 = 2;

/// Width reserved at the right edge for the always-present delete action and, when the
/// row has provenance, its WhyGem action.
pub(super) fn reserved_width(app: &App, video_id: &str) -> u16 {
    ACTION_WIDTH
        + if app.why_gem_for(video_id).is_some() {
            ACTION_WIDTH
        } else {
            0
        }
}

/// Draw and register trailing row actions. Returns the same reserved width used by the
/// caller to size the selectable row body.
pub(super) fn render(
    frame: &mut Frame,
    app: &App,
    row: Rect,
    index: usize,
    video_id: &str,
    selected: bool,
) -> u16 {
    let reserved = reserved_width(app, video_id).min(row.width);
    let selected_bg = selected.then(|| app.theme.color(R::SelectionBg));
    let action_rect = |offset: u16| Rect {
        x: row.right().saturating_sub(offset.min(row.width)),
        y: row.y,
        width: ACTION_WIDTH.min(row.width),
        height: row.height.min(1),
    };

    // Delete owns the final two cells on every queue row.
    let del_rect = action_rect(ACTION_WIDTH);
    let del_style = selected_bg.map_or_else(
        || app.theme.style(R::Error),
        |bg| Style::default().fg(app.theme.color(R::Error)).bg(bg),
    );
    frame.render_widget(Paragraph::new(Line::from("✗").style(del_style)), del_rect);
    if !del_rect.is_empty() {
        app.register_mouse_button(del_rect, MouseTarget::QueueDel(index));
    }

    // Provenance is intentionally per video ID, so duplicate rows publish independent hit
    // targets but open the same latest explanation.
    if app.why_gem_for(video_id).is_some() && row.width >= ACTION_WIDTH.saturating_mul(2) {
        let why_rect = action_rect(ACTION_WIDTH.saturating_mul(2));
        let why_style = selected_bg.map_or_else(
            || app.theme.style(R::Accent),
            |bg| Style::default().fg(app.theme.color(R::Accent)).bg(bg),
        );
        frame.render_widget(Paragraph::new(Line::from("?").style(why_style)), why_rect);
        app.register_mouse_button(why_rect, MouseTarget::QueueWhyGem(index));
    }

    reserved
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::remote::proto::WhyGemModel;

    #[test]
    fn provenance_adds_a_separate_action_without_stealing_delete_cells() {
        let mut app = App::new(50);
        app.why_gem.upsert(
            "track".to_owned(),
            WhyGemModel {
                slot: "Balanced".to_owned(),
                reasons: Vec::new(),
                confidence: None,
            },
        );
        let backend = TestBackend::new(14, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &app, Rect::new(2, 1, 10, 1), 3, "track", false);
            })
            .unwrap();

        assert_eq!(reserved_width(&app, "track"), 4);
        assert_eq!(app.hits.target_at(8, 1), Some(MouseTarget::QueueWhyGem(3)));
        assert_eq!(app.hits.target_at(9, 1), Some(MouseTarget::QueueWhyGem(3)));
        assert_eq!(app.hits.target_at(10, 1), Some(MouseTarget::QueueDel(3)));
        assert_eq!(app.hits.target_at(11, 1), Some(MouseTarget::QueueDel(3)));
    }

    #[test]
    fn manual_rows_keep_only_the_delete_action() {
        let app = App::new(50);
        assert_eq!(reserved_width(&app, "manual"), 2);
    }
}
