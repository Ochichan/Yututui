//! Confirmation and conflict dialogs owned by the settings view.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::centered_fixed;
use crate::app::{App, MouseTarget};
use crate::keymap::{self, Conflict};
use crate::settings::SettingsConfirm;
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

/// A modal warning shown when a rebind on the Keys tab collides with an existing binding.
/// Names the offending chord, the action already holding it, and where it lives, so the
/// failure is loud and actionable rather than a silently-dropped remap.
pub fn render_conflict(frame: &mut Frame, app: &App, area: Rect, conflict: &Conflict) {
    let popup = centered_fixed(area, 54, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(
            " ⚠ Keybinding conflict ",
            " ⚠ 단축키 충돌 ",
            " ⚠ ショートカット競合 "
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Warning).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chord = keymap::format_chord_for_display(conflict.chord, app.retro_mode());
    let where_line = match crate::i18n::current() {
        crate::i18n::Language::Korean => format!("{} 화면", conflict.ctx.title()),
        crate::i18n::Language::Japanese => format!("{} 画面", conflict.ctx.title()),
        _ => format!("in {}", conflict.ctx.title()),
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                chord,
                crate::ui::popup_style(app, R::Warning).add_modifier(Modifier::BOLD),
            ),
            Span::raw(t!(
                " is already bound to",
                " 은(는) 이미 사용 중",
                " は既に使用中"
            )),
        ]),
        Line::from(Span::styled(
            format!(
                "\u{201c}{}\u{201d}",
                conflict.existing.human_label_for(conflict.ctx)
            ),
            crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            where_line,
            crate::ui::popup_style(app, R::TextMuted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            t!(
                "Binding unchanged · press any key",
                "단축키 변경 안 됨 · 아무 키나 누르세요",
                "割り当ては変更なし · 任意のキーを押してください"
            ),
            crate::ui::popup_style(app, R::TextMuted),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        inner,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// A modal confirmation for settings actions that have broad side effects.
pub fn render_confirm(frame: &mut Frame, app: &App, area: Rect, confirm: SettingsConfirm) {
    let popup = centered_fixed(area, 56, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(confirm.title())
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);
    let enabling_retro = app.settings.as_ref().is_some_and(|s| !s.draft.retro_mode);
    frame.render_widget(
        Paragraph::new(confirm.prompt(enabling_retro))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(confirm.detail(enabling_retro))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );

    let segs = [
        buttons::Seg::button(
            MouseTarget::ConfirmSettings,
            t!(" Confirm (Enter) ", " 확인 (Enter) ", " 確認 (Enter) "),
        ),
        buttons::Seg::label("    "),
        buttons::Seg::button(
            MouseTarget::CancelSettings,
            t!(" Cancel (Esc) ", " 취소 (Esc) ", " キャンセル (Esc) "),
        ),
    ];
    buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
