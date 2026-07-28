//! Deletion-free confirmation for creating a linked server copy from a local playlist.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, MouseTarget, ServerPlaylistCreateStage};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = app.server.library.playlist_create.as_ref() else {
        return;
    };
    let popup = centered_fixed(area, 58, 13);
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .title(t!(
            " Create linked server playlist ",
            " 서버 연결 목록 만들기 ",
            " サーバー連携リストを作成 "
        ))
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(4),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!("{}: {}", t!("Name", "이름", "名前"), modal.name()))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "Local removals: 0",
            "로컬에서 삭제: 0",
            "ローカルから削除: 0"
        ))
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::Accent)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(match crate::i18n::current() {
            crate::i18n::Language::Korean => {
                format!("서버에 추가: {}", modal.server_additions())
            }
            crate::i18n::Language::Japanese => {
                format!("サーバーに追加: {}", modal.server_additions())
            }
            _ => format!("Server additions: {}", modal.server_additions()),
        })
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::Accent)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "Your local playlist stays unchanged. No existing server playlist is removed.",
            "로컬 목록은 그대로 유지돼요. 기존 서버 목록은 삭제하지 않아요.",
            "ローカルリストは変わりません。既存のサーバーリストは削除しません。"
        ))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[3],
    );

    if modal.stage == ServerPlaylistCreateStage::Applying {
        buttons::render_segments(
            frame,
            app,
            rows[5],
            &[buttons::Seg::label(t!(
                " Creating and checking… ",
                " 만들고 확인하는 중… ",
                " 作成して確認中… "
            ))],
            crate::ui::popup_style(app, R::TextMuted),
            crate::ui::confirm_gap_style(app),
            Alignment::Center,
        );
    } else {
        let create_full = t!(
            " Create & link (Enter) ",
            " 만들고 연결 (Enter) ",
            " 作成してリンク (Enter) "
        );
        let back_full = t!(" Back (Esc) ", " 뒤로 (Esc) ", " 戻る (Esc) ");
        let full_width = buttons::text_width(create_full)
            .saturating_add(2)
            .saturating_add(buttons::text_width(back_full));
        let (create, back) = if full_width <= rows[5].width {
            (create_full, back_full)
        } else {
            (
                t!(" Create ", " 만들기 ", " 作成 "),
                t!(" Back ", " 뒤로 ", " 戻る "),
            )
        };
        buttons::render_segments(
            frame,
            app,
            rows[5],
            &[
                buttons::Seg::button(MouseTarget::ConfirmServerPlaylistCreate, create),
                buttons::Seg::label("  "),
                buttons::Seg::button(MouseTarget::CancelServerPlaylistCreate, back),
            ],
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
            Alignment::Center,
        );
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn centered_fixed(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let margin_width = area.width.saturating_sub(2);
    let width = preferred_width.min(if margin_width == 0 {
        area.width
    } else {
        margin_width
    });
    let margin_height = area.height.saturating_sub(2);
    let height = preferred_height.min(if margin_height == 0 {
        area.height
    } else {
        margin_height
    });
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
    .intersection(area)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::personal_state::{PersonalPlaylistSnapshot, PlaylistId};

    fn app(stage: ServerPlaylistCreateStage) -> App {
        let mut app = App::new(50);
        app.server.library.playlist_create = Some(crate::app::ServerPlaylistCreateModal {
            generation: 1,
            snapshot: PersonalPlaylistSnapshot {
                playlist_id: PlaylistId::new("local").unwrap(),
                name: "A local list".to_owned(),
                entries: Vec::new(),
            },
            stage,
        });
        app
    }

    fn render_at(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn confirmation_fits_thirty_columns_with_hit_targets_in_all_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, expected) in [
            (
                crate::i18n::Language::English,
                [
                    "Yourlocalplayliststaysunchanged",
                    "Noexistingserverplaylistisremoved",
                ],
            ),
            (
                crate::i18n::Language::Korean,
                ["로컬목록은그대로유지돼요", "기존서버목록은삭제하지않아요"],
            ),
            (
                crate::i18n::Language::Japanese,
                [
                    "ローカルリストは変わりません",
                    "既存のサーバーリストは削除しません",
                ],
            ),
        ] {
            crate::i18n::set_language(language);
            let app = app(ServerPlaylistCreateStage::Confirming);
            let text = render_at(&app, 30, 30);
            let comparable: String = text
                .chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '│')
                .collect();
            assert!(comparable.contains('0'), "{language:?}: {text:?}");
            for phrase in expected {
                assert!(comparable.contains(phrase), "{language:?}: {text:?}");
            }
            for target in [
                MouseTarget::ConfirmServerPlaylistCreate,
                MouseTarget::CancelServerPlaylistCreate,
            ] {
                let rect = app.hits.rect_of_target(target).expect("visible button");
                assert!(rect.right() <= 30, "{language:?}: {rect:?}");
                assert!(rect.bottom() <= 30, "{language:?}: {rect:?}");
            }
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn applying_state_has_no_live_confirmation_target() {
        let app = app(ServerPlaylistCreateStage::Applying);
        let text = render_at(&app, 30, 30);
        assert!(!text.trim().is_empty());
        assert!(
            app.hits
                .rect_of_target(MouseTarget::ConfirmServerPlaylistCreate)
                .is_none()
        );
        assert!(
            app.hits
                .rect_of_target(MouseTarget::CancelServerPlaylistCreate)
                .is_none()
        );
    }
}
