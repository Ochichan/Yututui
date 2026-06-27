use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, MouseTarget};
use crate::keymap::Action;

#[derive(Clone, Copy)]
pub struct ButtonSpec<'a> {
    pub target: MouseTarget,
    pub label: &'a str,
}

impl<'a> ButtonSpec<'a> {
    pub fn new(target: MouseTarget, label: &'a str) -> Self {
        Self { target, label }
    }
}

pub fn render_button_row(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    buttons: &[ButtonSpec<'_>],
    alignment: Alignment,
) {
    let gap = 1u16;
    let widths: Vec<u16> = buttons.iter().map(|b| button_text_width(b.label)).collect();
    let total_width =
        widths.iter().sum::<u16>() + gap.saturating_mul(buttons.len().saturating_sub(1) as u16);
    let mut x = match alignment {
        Alignment::Center => area.x + area.width.saturating_sub(total_width) / 2,
        Alignment::Right => area.x + area.width.saturating_sub(total_width),
        Alignment::Left => area.x,
    };

    let button_style = Style::default()
        .fg(Color::White)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    for (i, button) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
            x = x.saturating_add(gap);
        }
        let text = button_text(button.label);
        let width = widths[i];
        app.register_mouse_button(
            Rect {
                x,
                y: area.y,
                width,
                height: area.height.min(1),
            },
            button.target,
        );
        spans.push(Span::styled(text, button_style));
        x = x.saturating_add(width);
    }

    frame.render_widget(Paragraph::new(Line::from(spans).alignment(alignment)), area);
}

pub fn render_help_button(frame: &mut Frame, app: &App, area: Rect, alignment: Alignment) {
    let label = app.help_footer();
    let buttons = [ButtonSpec::new(
        MouseTarget::Global(Action::ToggleHelp),
        label.as_str(),
    )];
    render_button_row(frame, app, area, &buttons, alignment);
}

fn button_text(label: &str) -> String {
    format!("[{label}]")
}

fn button_text_width(label: &str) -> u16 {
    button_text(label).chars().count() as u16
}
