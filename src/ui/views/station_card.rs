//! The session station card: bans, seeds, and one live term field over the Ctrl+R radio.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, MouseTarget};
use crate::keymap::{Action, KeyContext};
use crate::streaming::taste::{SeedPolarity, TasteEdit, TasteEntry};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

const CARD_WIDTH: u16 = 68;
const VISIBLE_ROWS: usize = 8;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(card) = app.overlays.station_card.as_ref() else {
        return;
    };
    if area.is_empty() {
        return;
    }

    let popup_width = CARD_WIDTH.min(area.width);
    let content_width = popup_width.saturating_sub(2).max(1);
    let entries: Vec<TasteEntry<'_>> = app.streaming.taste.entries().collect();
    let selected = if entries.is_empty() {
        0
    } else {
        card.selected.min(entries.len() - 1)
    };
    let list_height = entries.len().clamp(1, VISIBLE_ROWS) as u16;
    let popup_height = list_height.saturating_add(6).min(area.height).max(7);
    let popup = centered_fixed(area, popup_width, popup_height);

    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(" Station ", " 스테이션 ", " ステーション "))
        .borders(Borders::ALL)
        .border_style(crate::ui::popup_style(app, R::BorderPrimary))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    draw_input(frame, app, rows[0], card, content_width);
    draw_parse_hint(frame, app, rows[1], &card.input);
    draw_entries(frame, app, rows[2], &entries, selected, content_width);
    draw_close_hint(frame, app, rows[3]);

    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
    app.register_mouse_button(popup, MouseTarget::StationCard);
}

fn draw_input(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    card: &crate::app::StationCard,
    content_width: u16,
) {
    let label = t!("term: ", "시드: ", "シード: ");
    let input_width = usize::from(content_width).saturating_sub(UnicodeWidthStr::width(label));
    let cursor = card.cursor.byte_index(&card.input);
    let window = crate::ui::text::editable_window(&card.input, cursor, input_width);
    let line = Line::from(vec![
        Span::styled(label, crate::ui::popup_style(app, R::TextMuted)),
        Span::styled(window.before, crate::ui::popup_style(app, R::TextPrimary)),
        crate::ui::anim::caret_span(
            app,
            crate::ui::popup_style(app, R::Accent),
            crate::ui::popup_bg(app),
        ),
        Span::styled(window.after, crate::ui::popup_style(app, R::TextPrimary)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_parse_hint(frame: &mut Frame, app: &App, area: Rect, raw: &str) {
    let hint = if raw.trim().is_empty() {
        t!(
            "Enter: more like the playing artist · leading - excludes",
            "Enter: 재생 중인 아티스트와 비슷하게 · 앞에 - 는 제외",
            "Enter: 再生中のアーティストに寄せる · 先頭の - は除外"
        )
        .to_owned()
    } else {
        match TasteEdit::parse_seed(raw) {
            Ok(TasteEdit::SetSeed(seed)) => match seed.polarity {
                SeedPolarity::MoreLike => format!(
                    "{} {}",
                    t!("more like", "비슷하게", "寄せる"),
                    seed.term.as_str()
                ),
                SeedPolarity::Exclude => {
                    format!("{} {}", t!("exclude", "제외", "除外"), seed.term.as_str())
                }
            },
            _ => t!(
                "Term is empty or too long",
                "시드가 비었거나 너무 길어요",
                "シードが空か長すぎます"
            )
            .to_owned(),
        }
    };
    frame.render_widget(
        Paragraph::new(truncate_to_width(&hint, usize::from(area.width.max(1))))
            .style(crate::ui::popup_style(app, R::TextMuted)),
        area,
    );
}

fn draw_entries(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    entries: &[TasteEntry<'_>],
    selected: usize,
    content_width: u16,
) {
    if entries.is_empty() {
        frame.render_widget(
            Paragraph::new(t!(
                "No bans or seeds this session",
                "이번 세션 차단/시드가 없어요",
                "このセッションの禁止/シードはありません"
            ))
            .style(crate::ui::popup_style(app, R::TextMuted))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let start = selected.saturating_sub(VISIBLE_ROWS.saturating_sub(1));
    let heading = crate::ui::popup_style(app, R::HelpAction).add_modifier(Modifier::BOLD);
    let body = crate::ui::popup_style(app, R::TextPrimary);
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(start)
        .take(VISIBLE_ROWS)
        .map(|(index, entry)| {
            let marker = if index == selected { ">" } else { " " };
            let text = format!("{marker} {}", entry_label(*entry));
            Line::from(Span::styled(
                truncate_to_width(&text, usize::from(content_width)),
                if index == selected { heading } else { body },
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn entry_label(entry: TasteEntry<'_>) -> String {
    match entry {
        TasteEntry::BannedTrack(row) => format!("{} · {}", t!("ban", "차단", "禁止"), row.label),
        TasteEntry::BannedArtist(row) => format!(
            "{} · {}",
            t!("artist", "아티스트", "アーティスト"),
            row.display
        ),
        TasteEntry::Seed(seed) => match seed.polarity {
            SeedPolarity::MoreLike => {
                format!("{} · {}", t!("more", "비슷", "寄せ"), seed.term.as_str())
            }
            SeedPolarity::Exclude => {
                format!("{} · {}", t!("exclude", "제외", "除外"), seed.term.as_str())
            }
        },
    }
}

fn draw_close_hint(frame: &mut Frame, app: &App, area: Rect) {
    let forget_key = app.keymap.label_for_display(
        KeyContext::StationCard,
        Action::StationForget,
        app.retro_mode(),
    );
    let text = match crate::i18n::current() {
        crate::i18n::Language::Korean => {
            format!("Enter 추가 · {forget_key} 해제 · Esc 닫기")
        }
        crate::i18n::Language::Japanese => {
            format!("Enter で追加 · {forget_key} で解除 · Esc で閉じる")
        }
        _ => format!("Enter add · {forget_key} forget · Esc to close"),
    };
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        area,
    );
}

fn centered_fixed(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::api::Song;
    use crate::streaming::{TasteEdit, TasteOutcome};

    fn render_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn card_lists_a_ban_and_the_live_term_field() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        let mut app = App::new(50);
        let song = Song::remote("vid", "Night Drive", "Nova", "3:00");
        assert_eq!(
            app.streaming
                .taste
                .apply(TasteEdit::ban_track(&song).expect("id")),
            TasteOutcome::Applied
        );
        app.overlays.station_card = Some(crate::app::StationCard {
            input: "jazz".to_owned(),
            ..crate::app::StationCard::default()
        });

        let text = render_text(&app, 80, 24);
        assert!(text.contains("Station"), "{text}");
        assert!(text.contains("Night Drive"), "{text}");
        assert!(text.contains("jazz"), "{text}");
        assert!(text.contains("more like jazz"), "{text}");
    }
}
