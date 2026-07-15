//! Centered local audio-output picker rendered over Settings (and the responsive mini shell).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, AudioOutputRowKind, MouseTarget};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::text::{pad_to_width, truncate_owned_to_width};

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = app.overlays.audio_output_picker.as_ref() else {
        return;
    };
    let rows = app.audio_output_rows();
    let popup = crate::ui::centered_list_popup(area, rows.len(), 6, 48);
    let popup = Rect {
        width: popup.width.min(area.width),
        height: popup.height.min(area.height),
        ..popup
    };
    picker.rect.set(Some(popup));
    if popup.is_empty() {
        return;
    }

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" Audio output ", " 오디오 출력 "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width < 8 || inner.height < 3 {
        finish_popup(frame, app, popup);
        return;
    }

    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    render_status(frame, app, sections[0]);
    render_rows(frame, app, &rows, sections[1]);
    render_error(frame, app, sections[2]);
    render_hint(frame, app, sections[3]);
    finish_popup(frame, app, popup);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let status = if app
        .overlays
        .audio_output_picker
        .as_ref()
        .is_some_and(|picker| picker.applying)
    {
        t!("Applying selection…", "선택을 적용하는 중…").to_owned()
    } else if app.audio_devices.loading {
        format!(
            "{}{}",
            t!("Detecting devices", "장치 탐지 중"),
            crate::ui::anim::activity_dots(app).unwrap_or("…")
        )
    } else {
        let count = app
            .audio_devices
            .devices
            .iter()
            .filter(|device| !device.name.eq_ignore_ascii_case("auto"))
            .count();
        if crate::i18n::is_korean() {
            format!("출력 {count}개 · 기본값 사용 가능")
        } else {
            format!("{count} outputs · default available")
        }
    };
    frame.render_widget(
        Paragraph::new(truncate_owned_to_width(status, area.width as usize))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        area,
    );
}

fn render_rows(frame: &mut Frame, app: &App, rows: &[crate::app::AudioOutputRow], area: Rect) {
    if area.is_empty() {
        return;
    }
    let Some(picker) = app.overlays.audio_output_picker.as_ref() else {
        return;
    };
    let selected = picker.selected.min(rows.len().saturating_sub(1));
    let visible = area.height as usize;
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(rows.len().saturating_sub(visible));
    let saved = app
        .settings
        .as_ref()
        .map(|settings| settings.draft.audio_mpv_device.trim())
        .unwrap_or_default();
    let current = app.audio_devices.current_device.as_deref();

    for (index, row) in rows.iter().enumerate().skip(start).take(visible) {
        let focused = index == selected;
        let row_current = !app.audio_devices.loading
            && match &row.kind {
                AudioOutputRowKind::SystemDefault => current.is_none(),
                AudioOutputRowKind::Device(name) => current == Some(name.as_str()),
                _ => false,
            };
        let row_saved = match &row.kind {
            AudioOutputRowKind::SystemDefault => {
                saved.is_empty() || saved.eq_ignore_ascii_case("auto")
            }
            AudioOutputRowKind::Device(name) | AudioOutputRowKind::SavedUnavailable(name) => {
                saved == name
            }
            AudioOutputRowKind::Manual => false,
        };
        let marker = if focused { "▶ " } else { "  " };
        let width = area.width.saturating_sub(1) as usize;
        let label_width = width.saturating_sub(UnicodeWidthStr::width(marker));
        let mut label = if matches!(&row.kind, AudioOutputRowKind::Manual) && picker.editing_manual
        {
            manual_editor_label(app, picker, label_width)
        } else {
            row.label.clone()
        };
        let badge = if row_current {
            t!("[now] ", "[현재] ")
        } else if row_saved {
            t!("[saved] ", "[저장] ")
        } else {
            ""
        };
        if !badge.is_empty() {
            label.insert_str(0, badge);
        }
        let text = pad_to_width(
            &truncate_owned_to_width(format!("{marker}{label}"), width),
            width,
        );
        let style = if focused {
            crate::ui::anim::selection_style(
                app,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg))
                    .add_modifier(Modifier::BOLD),
            )
        } else if row.selectable {
            crate::ui::popup_style(app, R::TextPrimary)
        } else {
            crate::ui::popup_style(app, R::TextMuted)
        };
        let rect = Rect {
            x: area.x,
            y: area.y + (index - start) as u16,
            width: area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), rect);
        app.register_mouse_button(rect, MouseTarget::AudioOutputRow(index));
    }
}

fn render_error(frame: &mut Frame, app: &App, area: Rect) {
    let modal_error = app
        .overlays
        .audio_output_picker
        .as_ref()
        .and_then(|picker| picker.error.as_deref());
    let error = modal_error
        .or(app.audio_devices.error.as_deref())
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(truncate_owned_to_width(
            error.to_owned(),
            area.width as usize,
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::Error)),
        area,
    );
}

fn render_hint(frame: &mut Frame, app: &App, area: Rect) {
    let editing = app
        .overlays
        .audio_output_picker
        .as_ref()
        .is_some_and(|picker| picker.editing_manual);
    let hint = if editing {
        t!("Enter apply · Esc return", "Enter 적용 · Esc 돌아가기")
    } else if area.width < 50 {
        t!(
            "↑↓ choose · Enter · r refresh · Esc",
            "↑↓ 선택 · Enter · r 새로고침 · Esc"
        )
    } else {
        t!(
            "↑↓ choose · Enter apply · r refresh · Esc close",
            "↑↓ 선택 · Enter 적용 · r 새로고침 · Esc 닫기"
        )
    };
    frame.render_widget(
        Paragraph::new(truncate_owned_to_width(
            hint.to_owned(),
            area.width as usize,
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        area,
    );
}

fn manual_editor_label(app: &App, picker: &crate::app::AudioOutputPicker, width: usize) -> String {
    let caret = crate::ui::anim::caret_char(app);
    let prefix = format!("{} ", t!("Device ID:", "장치 ID:"));
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    if prefix_width < width {
        let cursor = picker.manual_cursor.byte_index(&picker.manual_input);
        let window = crate::ui::text::editable_window(
            &picker.manual_input,
            cursor,
            width.saturating_sub(prefix_width),
        );
        return format!("{prefix}{}{caret}{}", window.before, window.after);
    }
    let cursor = picker.manual_cursor.byte_index(&picker.manual_input);
    let window = crate::ui::text::editable_window(&picker.manual_input, cursor, width);
    format!("{}{caret}{}", window.before, window.after)
}

fn finish_popup(frame: &mut Frame, app: &App, popup: Rect) {
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
