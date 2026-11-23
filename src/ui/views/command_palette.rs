//! Modal renderer for the fuzzy Command & Jump Palette (`Ctrl+K`).

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::app::{App, MouseTarget, PaletteCategory};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(state) = app.overlays.command_palette.as_ref() else {
        return;
    };
    if area.width < 12 || area.height < 6 {
        return;
    }

    let width = area.width.min(78).max(area.width.min(48));
    let height = area.height.min(20).max(area.height.min(10));
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );

    crate::ui::render_modal_background(frame, app, popup);

    let title = format!(
        " {} ",
        t!("Command Palette", "명령 팔레트", "コマンドパレット")
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(crate::ui::popup_style(app, R::TextPrimary))
        .border_style(crate::ui::popup_style(app, R::BorderFocused));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    app.register_mouse_button(popup, MouseTarget::CommandPaletteSurface);

    if inner.width < 4 || inner.height < 3 {
        return;
    }

    let chunks = Layout::vertical([
        Constraint::Length(1), // search input
        Constraint::Length(1), // divider / preview / error hint
        Constraint::Min(1),    // results list
        Constraint::Length(1), // footer hint
    ])
    .split(inner);

    render_input_row(frame, app, state, chunks[0]);
    render_sub_bar(frame, app, state, chunks[1]);
    render_results(frame, app, state, chunks[2]);
    render_footer(frame, app, state, chunks[3]);
}

fn render_input_row(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::CommandPaletteState,
    area: Rect,
) {
    let count = format!(" {}/{}", state.filtered.len(), state.entries.len());
    let count_w = unicode_width::UnicodeWidthStr::width(count.as_str()) as u16;
    let prompt = "> ";
    let prompt_w = 2u16;
    let query_budget = area.width.saturating_sub(prompt_w).saturating_sub(count_w) as usize;

    let mut spans = vec![Span::styled(
        prompt,
        crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
    )];
    if state.query.is_empty() {
        let placeholder = t!(
            "Jump to chapter, command, setting, theme…",
            "챕터, 명령, 설정, 테마 검색…",
            "チャプター・操作・設定・テーマを検索…"
        );
        let text = truncate_to_width(placeholder, query_budget);
        let used = unicode_width::UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(
            text,
            crate::ui::popup_style(app, R::TextMuted),
        ));
        if query_budget > used {
            spans.push(Span::raw(" ".repeat(query_budget - used)));
        }
    } else {
        let text = truncate_to_width(&state.query, query_budget.saturating_sub(1));
        let used = unicode_width::UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(
            text,
            crate::ui::popup_style(app, R::TextPrimary),
        ));
        spans.push(Span::styled(
            if app.retro_mode() { "_" } else { "▏" },
            crate::ui::popup_style(app, R::Accent),
        ));
        if query_budget > used + 1 {
            spans.push(Span::raw(" ".repeat(query_budget - used - 1)));
        }
    }
    spans.push(Span::styled(
        count,
        crate::ui::popup_style(app, R::TextSubtle),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_sub_bar(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::CommandPaletteState,
    area: Rect,
) {
    if let Some(err) = state.error_hint.as_deref() {
        let line = Line::from(Span::styled(
            truncate_to_width(err, usize::from(area.width)),
            crate::ui::popup_style(app, R::Warning),
        ));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    if state.preview_active() {
        let notice = t!(
            "Live preview (in memory only · Enter save · Esc revert)",
            "실시간 미리보기 (메모리 전용 · Enter 적용 · Esc 원복)",
            "ライブプレビュー（メモリのみ · Enter 適用 · Esc 元に戻す）"
        );
        let line = Line::from(Span::styled(
            truncate_to_width(notice, usize::from(area.width)),
            crate::ui::popup_style(app, R::Accent),
        ));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let sep = if app.retro_mode() { "-" } else { "─" };
    let line = Line::from(Span::styled(
        sep.repeat(usize::from(area.width)),
        crate::ui::popup_style(app, R::BorderMuted),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_results(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::CommandPaletteState,
    area: Rect,
) {
    let total = state.filtered.len();
    if total == 0 {
        let empty = t!(
            "No matching commands or jumps",
            "일치하는 명령이나 점프 항목이 없어요",
            "一致する操作やジャンプ先がありません"
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(empty, usize::from(area.width)),
                crate::ui::popup_style(app, R::TextMuted),
            ))),
            area,
        );
        return;
    }

    let viewport = usize::from(area.height);
    let offset = state.scroll.resolve(state.selected, area.height, total, 1);
    let needs_scrollbar = total > viewport && area.width > 6;
    let list_w = if needs_scrollbar {
        area.width.saturating_sub(1)
    } else {
        area.width
    };

    for row in 0..viewport {
        let visible_idx = offset + row;
        let Some(entry) = state.filtered_entry(visible_idx) else {
            break;
        };
        let row_area = Rect::new(area.x, area.y + row as u16, list_w, 1);
        app.register_mouse_button(row_area, MouseTarget::CommandPaletteRow(visible_idx));

        let is_selected = visible_idx == state.selected;
        let base_style = if is_selected {
            app.theme.style(R::SelectionBg)
        } else {
            crate::ui::popup_style(app, R::TextPrimary)
        };
        let badge_role = match entry.category {
            PaletteCategory::Chapter | PaletteCategory::Bookmark => R::Accent,
            PaletteCategory::Theme | PaletteCategory::Animation => R::AccentAlt,
            PaletteCategory::Eq | PaletteCategory::Setting => R::Success,
            _ => R::TextMuted,
        };
        let badge_text = format!("[{}]", entry.category.badge());
        let badge_w = unicode_width::UnicodeWidthStr::width(badge_text.as_str());

        let shortcut_text = entry
            .shortcut
            .as_deref()
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
        let shortcut_w = unicode_width::UnicodeWidthStr::width(shortcut_text.as_str());

        let prefix = if is_selected {
            if app.retro_mode() { "> " } else { "▸ " }
        } else {
            "  "
        };
        let prefix_w = 2usize;
        let middle_budget = usize::from(list_w).saturating_sub(prefix_w + badge_w + 1 + shortcut_w);

        let shown_title = truncate_to_width(&entry.title, middle_budget);
        let shown_title_w = unicode_width::UnicodeWidthStr::width(shown_title.as_str());

        let sub_text = if let Some(sub) = entry.subtitle.as_deref()
            && middle_budget > shown_title_w + 4
        {
            let rem = middle_budget - shown_title_w - 3;
            let trimmed = truncate_to_width(sub, rem);
            if trimmed.is_empty() {
                String::new()
            } else {
                format!(" · {trimmed}")
            }
        } else {
            String::new()
        };
        let sub_w = unicode_width::UnicodeWidthStr::width(sub_text.as_str());
        let pad = middle_budget.saturating_sub(shown_title_w + sub_w);

        let mut title_style = if !entry.enabled {
            base_style.fg(app.theme.color(R::TextSubtle))
        } else if is_selected {
            base_style
                .fg(app.theme.color(R::TextPrimary))
                .add_modifier(Modifier::BOLD)
        } else {
            base_style
        };
        if !entry.enabled {
            title_style = title_style.add_modifier(Modifier::DIM);
        }
        let badge_style = if entry.enabled {
            base_style.fg(app.theme.color(badge_role))
        } else {
            title_style
        };
        let muted_style = base_style.fg(app.theme.color(R::TextMuted));
        let key_style = base_style.fg(app.theme.color(if entry.enabled {
            R::Accent
        } else {
            R::TextSubtle
        }));

        let spans = vec![
            Span::styled(prefix, key_style),
            Span::styled(badge_text, badge_style),
            Span::styled(" ", base_style),
            Span::styled(shown_title, title_style),
            Span::styled(sub_text, muted_style),
            Span::styled(" ".repeat(pad), base_style),
            Span::styled(shortcut_text, key_style),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(base_style),
            row_area,
        );
    }

    if needs_scrollbar
        && let Some(thumb) =
            crate::ui::scroll::scrollbar_thumb(total, viewport, area.height, offset)
    {
        let track_style = crate::ui::popup_style(app, R::BorderMuted);
        let thumb_style = crate::ui::popup_style(app, R::Accent);
        let (track_ch, thumb_ch) = if app.retro_mode() {
            ("|", "#")
        } else {
            ("│", "┃")
        };
        let buf = frame.buffer_mut();
        for row in 0..area.height {
            let (sym, style) = if row >= thumb.start && row < thumb.start.saturating_add(thumb.len)
            {
                (thumb_ch, thumb_style)
            } else {
                (track_ch, track_style)
            };
            if let Some(cell) = buf.cell_mut((area.x + list_w, area.y + row)) {
                cell.set_symbol(sym);
                cell.set_style(style);
            }
        }
    }
}

fn render_footer(
    frame: &mut Frame,
    app: &App,
    state: &crate::app::CommandPaletteState,
    area: Rect,
) {
    let hint = if state.filtered.is_empty() {
        t!(
            "Type to filter · Esc close",
            "검색어 입력 · Esc 닫기",
            "入力して絞り込み · Esc 閉じる"
        )
    } else {
        t!(
            "↑↓ move · Enter run · Esc close",
            "↑↓ 이동 · Enter 실행 · Esc 닫기",
            "↑↓ 移動 · Enter 実行 · Esc 閉じる"
        )
    };
    let line = Line::from(Span::styled(
        truncate_to_width(hint, usize::from(area.width)),
        crate::ui::popup_style(app, R::TextMuted),
    ));
    frame.render_widget(Paragraph::new(line), area);
}
