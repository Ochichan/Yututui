use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, MouseTarget};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};

pub(super) const FORM_WIDTH: u16 = 68;
pub(super) const SIMPLE_WIDTH: u16 = 58;

pub(super) fn message_modal(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    message: &str,
    border_role: R,
    modal_actions: (&str, Option<&str>),
) {
    let popup = centered(area, SIMPLE_WIDTH, 15);
    let inner = shell(frame, app, popup, title, border_role);
    let rows = Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[0],
    );
    actions(frame, app, rows[1], None, modal_actions.0, modal_actions.1);
    finish(frame, app, popup);
}

pub(super) fn wrapped_height(value: &str, width: u16, maximum: u16) -> u16 {
    if value.is_empty() || width == 0 || maximum == 0 {
        return 0;
    }
    let cells = UnicodeWidthStr::width(value);
    let width = usize::from(width);
    let hard_wrapped = cells.saturating_add(width - 1) / width;
    // Ratatui wraps at word boundaries, which can consume one more row than a pure cell-count
    // estimate on a narrow popup. Reserve that row rather than clipping safety copy.
    let conservative = hard_wrapped.saturating_add(usize::from(width < 40));
    u16::try_from(conservative)
        .unwrap_or(maximum)
        .clamp(1, maximum)
}

pub(super) fn render_field(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    label: &str,
    value: &str,
    selected: bool,
    editor: Option<(&str, usize, bool)>,
) {
    if area.height == 0 {
        return;
    }
    let marker = if selected { "› " } else { "  " };
    let marker_style = crate::ui::popup_style(app, if selected { R::Accent } else { R::TextMuted });
    let label_style = crate::ui::popup_style(
        app,
        if selected {
            R::SettingsLabel
        } else {
            R::TextMuted
        },
    );
    let value_style = crate::ui::popup_style(
        app,
        if selected {
            R::SettingsValue
        } else {
            R::TextPrimary
        },
    );
    if area.height >= 2 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(
                    clipped_line(label, area.width.saturating_sub(2) as usize),
                    label_style,
                ),
            ])),
            Rect { height: 1, ..area },
        );
        frame.render_widget(
            field_value(
                app,
                value,
                editor,
                area.width.saturating_sub(4) as usize,
                value_style,
            ),
            Rect {
                x: area.x.saturating_add(4),
                y: area.y.saturating_add(1),
                width: area.width.saturating_sub(4),
                height: 1,
            },
        );
    } else {
        let prefix = format!("{marker}{label}: ");
        let available = area
            .width
            .saturating_sub(UnicodeWidthStr::width(prefix.as_str()) as u16);
        frame.render_widget(
            Paragraph::new({
                let mut line = vec![Span::styled(prefix, label_style)];
                line.extend(field_value(app, value, editor, available as usize, value_style).spans);
                Line::from(line)
            }),
            area,
        );
    }
}

fn field_value<'a>(
    app: &'a App,
    value: &str,
    editor: Option<(&str, usize, bool)>,
    width: usize,
    style: ratatui::style::Style,
) -> Line<'a> {
    let Some((raw, cursor, masked)) = editor else {
        return Line::from(Span::styled(
            clipped_line(&display_or_placeholder(value), width),
            style,
        ));
    };
    let window = if masked {
        crate::ui::text::masked_editable_window(raw, cursor, width)
    } else {
        crate::ui::text::editable_window(raw, cursor, width)
    };
    Line::from(vec![
        Span::styled(window.before, style),
        crate::ui::anim::caret_span(app, style, crate::ui::popup_bg(app)),
        Span::styled(window.after, style),
    ])
}

pub(super) fn actions(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    reveal: Option<&str>,
    primary: &str,
    secondary: Option<&str>,
) {
    let mut segments = Vec::with_capacity(5);
    if let Some(label) = reveal {
        segments.push(Seg::button(MouseTarget::SyncWizardReveal, label));
        segments.push(Seg::label("  "));
    }
    if app.personal_state.sync_ui.busy.is_some() {
        segments.push(Seg::label(t!(" Working… ", " 처리 중… ", " 処理中… ")));
    } else {
        segments.push(Seg::button(MouseTarget::SyncWizardPrimary, primary));
    }
    if let Some(label) = secondary {
        segments.push(Seg::label("  "));
        segments.push(Seg::button(MouseTarget::SyncWizardSecondary, label));
    }
    buttons::render_segments(
        frame,
        app,
        area,
        &segments,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
}

pub(super) fn shell(
    frame: &mut Frame,
    app: &App,
    popup: Rect,
    title: &str,
    border_role: R,
) -> Rect {
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, border_role).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    inner
}

pub(super) fn finish(frame: &mut Frame, app: &App, popup: Rect) {
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

pub(super) fn centered(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let horizontal_margin = u16::from(area.width > 34).saturating_mul(2);
    let vertical_margin = u16::from(area.height > 16).saturating_mul(2);
    let width = preferred_width.min(area.width.saturating_sub(horizontal_margin));
    let height = preferred_height.min(area.height.saturating_sub(vertical_margin));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn display_or_placeholder(value: &str) -> String {
    if value.is_empty() {
        t!("(not set)", "(입력 안 됨)", "（未入力）").to_owned()
    } else {
        single_line(value)
    }
}

pub(super) fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || is_bidi_control(character) {
                '�'
            } else {
                character
            }
        })
        .collect()
}

pub(super) fn clipped_line(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let value = single_line(value);
    if UnicodeWidthStr::width(value.as_str()) <= width {
        return value;
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut clipped = String::new();
    let limit = width - 1;
    let mut used = 0usize;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > limit {
            break;
        }
        clipped.push(character);
        used = used.saturating_add(character_width);
    }
    clipped.push('…');
    clipped
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_replaces_line_and_bidi_controls() {
        let clipped = clipped_line("folder\nsafe\u{202e}txt/long-name", 12);
        assert!(!clipped.contains('\n'));
        assert!(!clipped.contains('\u{202e}'));
        assert!(clipped.ends_with('…'));
        assert!(UnicodeWidthStr::width(clipped.as_str()) <= 12);
    }

    #[test]
    fn wrapped_height_counts_narrow_localized_copy() {
        assert_eq!(
            wrapped_height(
                "Your listening data is encrypted before it leaves this device.",
                28,
                4,
            ),
            4
        );
        assert_eq!(
            wrapped_height("감상 데이터는 이 기기를 떠나기 전에 암호화됩니다.", 28, 4,),
            3
        );
    }
}
