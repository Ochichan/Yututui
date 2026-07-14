//! Responsive Settings color-picker modal.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{App, MouseTarget};
use crate::settings::{
    COLOR_PICKER_CHOICE_COUNT, ColorPickerChoice, ColorPickerSelection, ColorPickerState,
    color_picker_choice,
};
use crate::t;
use crate::theme::ThemeRole as R;

const DESIRED_WIDTH: u16 = 58;
const DESIRED_HEIGHT: u16 = 20;
const CELL_PITCH: u16 = 3;
const PREFERRED_COLUMNS: usize = 18;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = app
        .settings
        .as_ref()
        .and_then(|state| state.color_picker.as_ref())
    else {
        return;
    };
    let popup = centered_picker_rect(area);
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {} · {} ",
            t!("Color", "색상"),
            picker.role.label()
        ))
        .border_style(crate::ui::popup_style(app, R::BorderFocused))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    app.register_mouse_button(popup, MouseTarget::SettingsColorPickerSurface);

    if inner.height == 0 || inner.width == 0 {
        crate::ui::seal_popup_background(frame, app, popup);
        return;
    }
    let current = Rect { height: 1, ..inner };
    let help_height = u16::from(inner.height >= 2);
    let grid = Rect {
        x: inner.x,
        y: inner.y.saturating_add(1),
        width: inner.width,
        height: inner.height.saturating_sub(1 + help_height),
    };
    let help = Rect {
        x: inner.x,
        y: inner.bottom().saturating_sub(help_height),
        width: inner.width,
        height: help_height,
    };
    let columns = usize::from((grid.width / CELL_PITCH).max(1)).min(PREFERRED_COLUMNS);
    picker.set_layout(columns, usize::from(grid.height.max(1)));

    render_current(frame, app, picker, current);
    render_grid(frame, app, picker, grid);
    if help.height > 0 {
        let hint = if help.width < 40 {
            t!("arrows · Enter apply · Esc", "방향키 · Enter 적용 · Esc")
        } else {
            t!(
                "arrows/wheel · Enter/click apply · Esc/outside cancel",
                "방향키/휠 · Enter/클릭 적용 · Esc/바깥 취소"
            )
        };
        frame.render_widget(
            Paragraph::new(hint)
                .centered()
                .style(crate::ui::popup_style(app, R::TextMuted)),
            help,
        );
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn render_current(frame: &mut Frame, app: &App, picker: &ColorPickerState, area: Rect) {
    let selected = picker.selection() == ColorPickerSelection::Current;
    let label_style = crate::ui::popup_style(
        app,
        if selected {
            R::SettingsValueFocused
        } else {
            R::SettingsLabel
        },
    )
    .add_modifier(if selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    });
    let swatch = swatch_span(app, picker.current(), selected);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(if selected { "> " } else { "  " }, label_style),
            Span::styled(t!("Current ", "현재 "), label_style),
            swatch,
            Span::raw(" "),
            Span::styled(picker.current().to_owned(), label_style),
        ])),
        area,
    );
    app.register_mouse_button(area, MouseTarget::SettingsColorPickerCurrent);
}

fn render_grid(frame: &mut Frame, app: &App, picker: &ColorPickerState, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let columns = picker.columns();
    let first_row = picker.row_offset();
    let last_row = first_row + picker.visible_rows();
    let start = first_row.saturating_mul(columns);
    let end = last_row
        .saturating_mul(columns)
        .min(COLOR_PICKER_CHOICE_COUNT);
    for index in start..end {
        let row = index / columns;
        let column = index % columns;
        let x = area
            .x
            .saturating_add((column as u16).saturating_mul(CELL_PITCH));
        let y = area.y.saturating_add((row - first_row) as u16);
        if x >= area.right() || y >= area.bottom() {
            continue;
        }
        let cell = Rect {
            x,
            y,
            width: 2.min(area.right() - x),
            height: 1,
        };
        let selected = picker.selection() == ColorPickerSelection::Choice(index);
        let choice = color_picker_choice(index).expect("rendered picker choice is in range");
        let (text, style) = choice_cell(app, choice, selected);
        frame.render_widget(Paragraph::new(text).style(style), cell);
        app.register_mouse_button(cell, MouseTarget::SettingsColorPickerChoice(index));
    }
}

fn choice_cell(app: &App, choice: ColorPickerChoice, selected: bool) -> (&'static str, Style) {
    match choice {
        ColorPickerChoice::Transparent => (
            if selected { "[]" } else { "▒▒" },
            crate::ui::popup_style(app, if selected { R::Accent } else { R::TextMuted })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        ColorPickerChoice::Rgb(r, g, b) => {
            let fg = contrast_color(r, g, b);
            (
                if selected { "[]" } else { "  " },
                Style::default()
                    .fg(fg)
                    .bg(Color::Rgb(r, g, b))
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )
        }
    }
}

fn swatch_span(app: &App, value: &str, selected: bool) -> Span<'static> {
    let Some((r, g, b)) = parse_hex(value) else {
        return Span::styled(
            if selected { "[]" } else { "▒▒" },
            crate::ui::popup_style(app, if selected { R::Accent } else { R::TextMuted }),
        );
    };
    Span::styled(
        if selected { "[]" } else { "  " },
        Style::default()
            .fg(contrast_color(r, g, b))
            .bg(Color::Rgb(r, g, b)),
    )
}

fn parse_hex(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

fn contrast_color(r: u8, g: u8, b: u8) -> Color {
    let luma = u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114;
    if luma >= 128_000 {
        Color::Black
    } else {
        Color::White
    }
}

fn centered_picker_rect(area: Rect) -> Rect {
    let available_width = if area.width > 2 {
        area.width - 2
    } else {
        area.width
    };
    let available_height = if area.height > 2 {
        area.height - 2
    } else {
        area.height
    };
    let width = DESIRED_WIDTH.min(available_width);
    let height = DESIRED_HEIGHT.min(available_height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eighty_by_twenty_four_has_room_for_the_full_palette() {
        let popup = centered_picker_rect(Rect::new(0, 0, 80, 24));
        let inner_width = popup.width - 2;
        let inner_height = popup.height - 2;
        let columns = usize::from(inner_width / CELL_PITCH).min(PREFERRED_COLUMNS);
        let grid_rows = usize::from(inner_height - 2);
        assert_eq!(columns, 18);
        assert!(grid_rows >= COLOR_PICKER_CHOICE_COUNT.div_ceil(columns));
    }

    #[test]
    fn narrow_picker_reduces_columns_without_overflowing() {
        let popup = centered_picker_rect(Rect::new(0, 0, 30, 30));
        assert_eq!(popup.width, 28);
        assert_eq!((popup.width - 2) / CELL_PITCH, 8);
        assert!(popup.right() <= 30);
        assert!(popup.bottom() <= 30);
    }
}
