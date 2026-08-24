//! The sleep-timer popup: a small modal input for minutes (or `off` to cancel).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::t;
use crate::theme::ThemeRole as R;

/// A `w`×`h` rect centered in `area`, clamped so it never exceeds the available space.
fn centered_fixed(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Render the sleep-timer popup while it is open. Geometry and styling mirror the
/// create-playlist popup so both small modals read as one family.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.sleep_popup.as_ref() else {
        return;
    };
    let popup = centered_fixed(area, 46, 9);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(
            " ⏾ Sleep timer ",
            " ⏾ 수면 타이머 ",
            " ⏾ スリープタイマー "
        ))
        .borders(Borders::ALL)
        .border_style(
            crate::ui::popup_style(app, R::Accent).add_modifier(ratatui::style::Modifier::BOLD),
        )
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // input
        Constraint::Length(1), // hint / error
        Constraint::Length(1), // spacer
        Constraint::Min(1),    // buttons
    ])
    .split(inner);

    let label = t!("  minutes: ", "  분: ", "  分: ");
    let cursor = state.cursor.byte_index(&state.input);
    let shown = crate::ui::text::editable_window(
        &state.input,
        cursor,
        (rows[1].width as usize).saturating_sub(UnicodeWidthStr::width(label)),
    );
    let input = Line::from(vec![
        Span::styled(label, crate::ui::popup_style(app, R::TextMuted)),
        Span::styled(shown.before, crate::ui::popup_style(app, R::TextPrimary)),
        crate::ui::anim::caret_span(
            app,
            crate::ui::popup_style(app, R::Accent),
            crate::ui::popup_bg(app),
        ),
        Span::styled(shown.after, crate::ui::popup_style(app, R::TextPrimary)),
    ]);
    frame.render_widget(Paragraph::new(input), rows[1]);

    let hint = if state.error {
        t!(
            "Type a number of minutes, or \"off\" to cancel",
            "분 단위 숫자를 입력하거나 \"off\"로 취소하세요",
            "分数を入力するか「off」でキャンセルしてください"
        )
    } else {
        t!(
            "A fade-out precedes the pause; \"off\" cancels",
            "일시정지 전에 볼륨이 서서히 줄어듭니다. \"off\"는 취소",
            "一時停止の前に音量がフェードアウトします。「off」でキャンセル"
        )
    };
    let hint_style = if state.error {
        crate::ui::popup_style(app, R::Error)
    } else {
        crate::ui::popup_style(app, R::TextMuted)
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, hint_style))),
        rows[2],
    );

    let segs = [
        crate::ui::buttons::Seg::button(
            crate::app::MouseTarget::ConfirmSleepTimer,
            t!(" Set (Enter) ", " 설정 (Enter) ", " 設定 (Enter) "),
        ),
        crate::ui::buttons::Seg::label("    "),
        crate::ui::buttons::Seg::button(
            crate::app::MouseTarget::CancelSleepTimer,
            t!(" Cancel (Esc) ", " 취소 (Esc) ", " キャンセル (Esc) "),
        ),
    ];
    crate::ui::buttons::render_segments(
        frame,
        app,
        rows[4],
        &segs,
        crate::ui::popup_style(app, R::Accent),
        crate::ui::popup_style(app, R::TextPrimary),
        Alignment::Center,
    );
}
