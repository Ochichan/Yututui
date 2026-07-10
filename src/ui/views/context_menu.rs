//! Small pointer-anchored context menu rendered entirely by ratatui.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(menu) = app.overlays.context_menu.as_ref() else {
        return;
    };
    if area.width < 3 || area.height < 3 || menu.items.is_empty() {
        menu.rect.set(None);
        return;
    }

    let label_width = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(1);
    let width = u16::try_from(label_width.saturating_add(4))
        .unwrap_or(u16::MAX)
        .min(area.width)
        .max(3);
    let height = u16::try_from(menu.items.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(area.height)
        .max(3);
    let max_x = area.right().saturating_sub(width);
    let max_y = area.bottom().saturating_sub(height);
    let popup = Rect {
        x: menu.anchor_col.clamp(area.x, max_x),
        y: menu.anchor_row.clamp(area.y, max_y),
        width,
        height,
    };
    menu.rect.set(Some(popup));

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderFocused))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let visible = usize::from(inner.height);
    let start = menu
        .selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(menu.items.len().saturating_sub(visible));
    for (visual_row, index) in (start..menu.items.len()).take(visible).enumerate() {
        let row = Rect {
            x: inner.x,
            y: inner.y + u16::try_from(visual_row).unwrap_or(u16::MAX),
            width: inner.width,
            height: 1,
        };
        let selected = index == menu.selected;
        let style = if selected {
            Style::default()
                .fg(app.theme.color(R::SelectionFg))
                .bg(app.theme.color(R::SelectionBg))
                .add_modifier(Modifier::BOLD)
        } else {
            crate::ui::popup_style(app, R::TextPrimary)
        };
        let prefix = if selected { "› " } else { "  " };
        let label = truncate_to_width(
            &menu.items[index].label(menu.target_count()),
            usize::from(inner.width.saturating_sub(2)),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(label, style),
            ]))
            .style(style),
            row,
        );
        app.register_mouse_button(row, MouseTarget::ContextMenuItem(index));
    }

    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
