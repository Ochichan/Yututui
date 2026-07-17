//! Spotify-specific settings controls and playlist picker overlay.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{HL_SYMBOL, centered_fixed, field_value_text, other_label_width};
use crate::app::{App, MouseTarget};
use crate::settings::{Field, SettingsState};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;
use crate::ui::text::pad_to_width;

pub(super) fn render_spotify_import_mode_dropdown(
    frame: &mut Frame,
    app: &App,
    st: &SettingsState,
    area: Rect,
    offset: usize,
    display_to_field: &[Option<usize>],
) {
    let Some(selected) = st.spotify_import_mode_dropdown else {
        return;
    };
    let fields = st.fields();
    let Some(field_index) = fields
        .iter()
        .position(|field| *field == Field::SpotifyImportMode)
    else {
        return;
    };
    let Some(display_row) = display_to_field
        .iter()
        .position(|mapped| *mapped == Some(field_index))
    else {
        return;
    };
    if display_row < offset {
        return;
    }
    let visible = display_row - offset;
    if visible >= area.height as usize {
        return;
    }

    let theme = &st.draft.theme;
    let gutter = buttons::text_width(HL_SYMBOL);
    let vx = area.x + gutter + other_label_width(st.tab) as u16;
    let available_width = area.right().saturating_sub(vx);
    if available_width == 0 {
        return;
    }
    let value = field_value_text(
        app,
        st,
        Field::SpotifyImportMode,
        field_index == st.row,
        available_width as usize,
    );
    let label_width = buttons::text_width(&value)
        .max(
            crate::config::SpotifyImportMode::ALL
                .iter()
                .map(|mode| buttons::text_width(mode.label()))
                .max()
                .unwrap_or(0)
                + 4,
        )
        .min(available_width);
    let leading = vx.saturating_sub(area.x) as usize;
    // Drop below the row; when the list is short (e.g. the docked player bar shrank it)
    // and the options wouldn't fit, drop up above the row instead of clipping them away.
    let modes = crate::config::SpotifyImportMode::ALL.len() as u16;
    let anchor_y = area.y + visible as u16;
    let below = area.bottom().saturating_sub(anchor_y + 1);
    let above = anchor_y.saturating_sub(area.y);
    let start_y = if below >= modes || above < modes {
        anchor_y + 1
    } else {
        anchor_y - modes
    };
    for (idx, mode) in crate::config::SpotifyImportMode::ALL
        .iter()
        .copied()
        .enumerate()
    {
        let y = start_y + idx as u16;
        if y >= area.bottom() {
            break;
        }
        let focused = idx == selected;
        let marker = if focused { ">" } else { " " };
        let text = format!(
            "{marker} {}",
            pad_to_width(mode.label(), label_width.saturating_sub(2) as usize)
        );
        let text = pad_to_width(
            &format!("{}{}", " ".repeat(leading), text),
            area.width as usize,
        );
        let role = if focused {
            R::SettingsValueFocused
        } else {
            R::SettingsValue
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, theme.style(role)))),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
        app.register_mouse_button(
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
            MouseTarget::SettingsSpotifyImportModeSelect(mode),
        );
    }
}

pub(in crate::ui::views) fn render_spotify_import_mode_dropdown_popup(
    frame: &mut Frame,
    app: &App,
    area: Rect,
) {
    let Some(st) = app.settings.as_deref() else {
        return;
    };
    let Some(selected) = st.spotify_import_mode_dropdown else {
        return;
    };
    let popup =
        crate::ui::centered_list_popup(area, crate::config::SpotifyImportMode::ALL.len(), 3, 32);
    if popup.is_empty() {
        return;
    }

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(
            " Spotify import mode ",
            " Spotify 가져오기 모드 ",
            " Spotifyインポートモード "
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 2 || inner.width < 8 {
        crate::ui::seal_popup_background(frame, app, popup);
        crate::ui::mark_art_rows_for_popup(frame, app, popup);
        return;
    }

    let sections = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let list_area = sections[0];
    let row_w = list_area.width as usize;
    let visible = list_area.height as usize;
    let start = selected.saturating_sub(visible.saturating_sub(1));
    for (idx, mode) in crate::config::SpotifyImportMode::ALL
        .iter()
        .copied()
        .enumerate()
        .skip(start)
        .take(list_area.height as usize)
    {
        let focused = idx == selected;
        let marker = if focused { "▶ " } else { "  " };
        let text = pad_to_width(
            &format!("{marker}{}", mode.label()),
            row_w.saturating_sub(1),
        );
        let style = if focused {
            crate::ui::anim::selection_style(
                app,
                Style::default()
                    .fg(app.theme.color(R::SelectionFg))
                    .bg(app.theme.color(R::SelectionBg))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            crate::ui::popup_style(app, R::TextPrimary)
        };
        let row = Rect {
            x: list_area.x,
            y: list_area.y + (idx - start) as u16,
            width: list_area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(Line::from(text).style(style)), row);
        app.register_mouse_button(row, MouseTarget::SettingsSpotifyImportModeSelect(mode));
    }
    frame.render_widget(
        Paragraph::new(t!(
            "↑↓ choose · Enter save · Esc close",
            "↑↓ 선택 · Enter 저장 · Esc 닫기",
            "↑↓ 選択 · Enter 保存 · Esc 閉じる"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        sections[1],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

/// The Spotify playlist picker overlay (Import from Spotify…): ↑/↓ select, Enter
/// imports into a local Library playlist, Esc closes.
pub fn render_spotify_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = app.overlays.spotify_picker.as_ref() else {
        return;
    };
    let h = (picker.items.len() as u16 + 6).clamp(9, 20);
    let popup = centered_fixed(area, 72, h);
    crate::ui::render_popup_background(frame, app, popup);

    let block = Block::default()
        .title(t!(
            " Import from Spotify ",
            " Spotify에서 가져오기 ",
            " Spotifyからインポート "
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let import_mode = app
        .settings
        .as_ref()
        .map(|st| st.draft.spotify_import_mode)
        .unwrap_or(app.config.spotify.import_mode);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(t!(
                "Destination: Library > Playlists",
                "대상: Library > Playlists",
                "取り込み先: Library > Playlists"
            )),
            Line::from(format!(
                "{}: {} · {}",
                t!("Mode", "모드", "モード"),
                import_mode.label(),
                t!(
                    "change in Settings > Accounts",
                    "Settings > Accounts에서 변경",
                    "Settings > Accountsで変更"
                )
            )),
        ])
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[0],
    );
    let visible = rows[1].height as usize;
    // Keep the selection in view for long playlist lists.
    let first = picker
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(picker.items.len().saturating_sub(visible.max(1)));
    let lines: Vec<ratatui::text::Line> = picker
        .items
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, item)| {
            let marker = if i == picker.selected { "▸ " } else { "  " };
            let count = if item.total > 0 {
                format!("  ({})", item.total)
            } else {
                String::new()
            };
            let style = if i == picker.selected {
                crate::ui::popup_style(app, R::TextPrimary)
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                crate::ui::popup_style(app, R::TextMuted)
            };
            ratatui::text::Line::styled(format!("{marker}{}{count}", item.label), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[1]);
    // One click target per visible row: click selects, clicking the selected row (or a
    // double-click) imports. Indices match the `.skip(first).take(visible)` render above.
    for (vis, i) in (first..picker.items.len()).take(visible).enumerate() {
        app.register_mouse_button(
            Rect {
                x: rows[1].x,
                y: rows[1].y + vis as u16,
                width: rows[1].width,
                height: 1,
            },
            MouseTarget::SpotifyPickRow(i),
        );
    }
    frame.render_widget(
        Paragraph::new(t!(
            "↑/↓ select · Enter import · Esc close · or click",
            "↑/↓ 선택 · Enter 가져오기 · Esc 닫기 · 클릭 가능",
            "↑/↓ 選択 · Enter インポート · Esc 閉じる · クリック可"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}
