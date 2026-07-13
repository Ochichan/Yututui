use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, MouseTarget, ToolSetupContext};
use crate::keymap::{Action, KeyContext};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons::{self, Seg};

pub fn render_search_hint(frame: &mut Frame, app: &App, area: Rect) {
    let mini = crate::ui::layout::tier(area) == crate::ui::layout::UiTier::Mini;
    let layout_sufficient = search_hint_layout_sufficient(area, mini);
    if !app.search_onboarding_render_eligible(layout_sufficient) {
        return;
    }
    let key =
        app.keymap
            .label_for_display(KeyContext::Player, Action::OpenSearch, app.retro_mode());
    let text = if mini {
        format!("{}: {key}", t!("Search", "검색"))
    } else {
        format!(
            "→ {} · {} `{key}` {}",
            t!("Search starts here", "검색은 여기를 누르세요"),
            t!("press", "키보드는"),
            t!("or click Search", "또는 검색을 클릭하세요")
        )
    };
    let row = Rect {
        x: area.x.saturating_add(1),
        y: if mini {
            area.bottom().saturating_sub(1)
        } else {
            area.y.saturating_add(1)
        },
        width: area.width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(text).style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD)),
        row,
    );
}

fn search_hint_layout_sufficient(area: Rect, mini: bool) -> bool {
    area.width > 2 && area.height >= if mini { 4 } else { 3 }
}

pub fn render_tool_setup(frame: &mut Frame, app: &App, area: Rect) {
    let Some(prompt) = app.tool_setup.as_ref() else {
        return;
    };
    if area.is_empty() {
        return;
    }
    if area.width < 32 || area.height < 7 {
        frame.render_widget(
            Paragraph::new(t!(
                "Playback tools required · R check · G guide · Esc later",
                "재생 도구 설치 필요 · R 확인 · G 안내 · Esc 나중에"
            ))
            .style(app.theme.style(R::Accent).add_modifier(Modifier::BOLD))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(2).min(12);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    crate::ui::render_popup_background(frame, app, popup);
    let title = match prompt.context {
        ToolSetupContext::Startup => {
            t!(" Playback tools required ", " 재생 도구 설치가 필요합니다 ")
        }
        ToolSetupContext::Downloads => {
            t!(" Download tool required ", " 다운로드 도구가 필요합니다 ")
        }
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }

    let missing = prompt.missing.join(", ");
    let reason = match prompt.context {
        ToolSetupContext::Startup => t!(
            "YuTuTui! needs these tools to search and play music.",
            "YuTuTui!가 음악을 검색하고 재생하려면 이 도구가 필요합니다."
        ),
        ToolSetupContext::Downloads => t!(
            "Install these tools to finish the queued download.",
            "대기 중인 다운로드를 마치려면 이 도구를 설치하세요."
        ),
    };
    let command = prompt.command.as_deref().unwrap_or(t!(
        "Use the setup guide for this system.",
        "이 시스템에서는 설치 안내를 이용하세요."
    ));
    let body = vec![
        Line::from(reason),
        Line::from(vec![
            Span::styled(
                format!("{}: ", t!("Missing", "없는 도구")),
                app.theme.style(R::TextMuted),
            ),
            Span::styled(missing, app.theme.style(R::Accent)),
        ]),
        Line::from(""),
        Line::styled(command, app.theme.style(R::TextPrimary)),
    ];
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }),
        Rect {
            height: inner.height.saturating_sub(2),
            ..inner
        },
    );

    let copy = t!(" Copy command (C) ", " 명령 복사 (C) ");
    let guide = t!(" Setup guide (G) ", " 설치 안내 (G) ");
    let retry = t!(" Check again (R) ", " 다시 확인 (R) ");
    let later = if prompt.context == ToolSetupContext::Downloads {
        t!(" Cancel (Esc) ", " 취소 (Esc) ")
    } else {
        t!(" Later (Esc) ", " 나중에 (Esc) ")
    };
    let mut segments = Vec::with_capacity(7);
    if prompt.command.is_some() {
        segments.push(Seg::button(MouseTarget::ToolSetupCopy, copy));
        segments.push(Seg::label(" "));
    }
    segments.push(Seg::button(MouseTarget::ToolSetupGuide, guide));
    segments.push(Seg::label(" "));
    segments.push(Seg::button(MouseTarget::ToolSetupRetry, retry));
    segments.push(Seg::label(" "));
    segments.push(Seg::button(MouseTarget::ToolSetupLater, later));
    let button_row = Rect {
        y: inner.bottom().saturating_sub(2),
        height: 2.min(inner.height),
        ..inner
    };
    buttons::render_segments_with_hit_height(
        frame,
        app,
        button_row,
        &segments,
        (
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
        ),
        Alignment::Center,
        2,
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_hint_requires_a_nonzero_content_row() {
        assert!(!search_hint_layout_sufficient(Rect::new(0, 0, 2, 4), true));
        assert!(search_hint_layout_sufficient(Rect::new(0, 0, 3, 4), true));
        assert!(!search_hint_layout_sufficient(Rect::new(0, 0, 3, 2), false));
        assert!(search_hint_layout_sufficient(Rect::new(0, 0, 3, 3), false));
    }
}
