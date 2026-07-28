//! Linked server-playlist recovery and destructive confirmation modal.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{
    App, MouseTarget, ServerPlaylistRecoveryAction as Action, ServerPlaylistRecoveryStage as Stage,
};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = app.server.library.playlist_recovery.as_ref() else {
        return;
    };
    let popup = centered_fixed(area, 58, 13);
    crate::ui::render_popup_background(frame, app, popup);
    let destructive = modal.action.destructive();
    let block = Block::default()
        .title(if destructive {
            t!(
                " Confirm playlist deletion ",
                " 플레이리스트 삭제 확인 ",
                " プレイリスト削除の確認 "
            )
        } else {
            t!(
                " Playlist recovery ",
                " 플레이리스트 복구 ",
                " プレイリストの復旧 "
            )
        })
        .borders(Borders::ALL)
        .border_style(
            crate::ui::popup_style(app, if destructive { R::Error } else { R::Accent })
                .add_modifier(Modifier::BOLD),
        )
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(4),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let (prompt, detail) = copy(modal.action, &modal.name, modal.stage);
    frame.render_widget(
        Paragraph::new(prompt)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(detail)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(
                app,
                if destructive {
                    R::Warning
                } else {
                    R::TextMuted
                },
            )),
        rows[1],
    );
    if modal.stage == Stage::Confirming {
        let delete_full = t!(" Delete (Enter) ", " 삭제 (Enter) ", " 削除 (Enter) ");
        let back_full = t!(" Back (Esc) ", " 뒤로 (Esc) ", " 戻る (Esc) ");
        let full_width = buttons::text_width(delete_full)
            .saturating_add(2)
            .saturating_add(buttons::text_width(back_full));
        let (delete, back) = if full_width <= rows[3].width {
            (delete_full, back_full)
        } else {
            (
                t!(" Delete ", " 삭제 ", " 削除 "),
                t!(" Back ", " 뒤로 ", " 戻る "),
            )
        };
        buttons::render_segments(
            frame,
            app,
            rows[3],
            &[
                buttons::Seg::button(MouseTarget::ConfirmServerPlaylistRecovery, delete),
                buttons::Seg::label("  "),
                buttons::Seg::button(MouseTarget::CancelServerPlaylistRecovery, back),
            ],
            crate::ui::popup_style(app, R::Error).add_modifier(Modifier::BOLD),
            crate::ui::confirm_gap_style(app),
            Alignment::Center,
        );
    } else {
        buttons::render_segments(
            frame,
            app,
            rows[3],
            &[buttons::Seg::label(t!(
                " Applying… ",
                " 적용 중… ",
                " 適用中… "
            ))],
            crate::ui::popup_style(app, R::TextMuted),
            crate::ui::confirm_gap_style(app),
            Alignment::Center,
        );
    }
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn copy(action: Action, name: &str, stage: Stage) -> (String, &'static str) {
    if stage == Stage::Applying {
        return (
            format!(
                "{}: {name}",
                t!(
                    "Updating playlist",
                    "플레이리스트 업데이트",
                    "プレイリストを更新"
                )
            ),
            t!(
                "This window will close when the result is verified.",
                "결과가 확인되면 이 창이 닫혀요.",
                "結果を確認するとこの画面は閉じます。"
            ),
        );
    }
    match action {
        Action::DeleteBoth => (
            format!(
                "{} “{name}”?",
                t!(
                    "Delete both copies of",
                    "두 복사본 모두 삭제:",
                    "両方のコピーを削除:"
                )
            ),
            t!(
                "This deletes the local and server playlists and cannot be undone.",
                "로컬과 서버 목록을 삭제하며 되돌릴 수 없어요.",
                "ローカルとサーバーの両方を削除し、元に戻せません。"
            ),
        ),
        Action::DeleteLocal => (
            format!(
                "{} “{name}”?",
                t!(
                    "Delete the local copy of",
                    "로컬 복사본 삭제:",
                    "ローカル側を削除:"
                )
            ),
            t!(
                "Another server copy may exist. This deletes only the local copy and cannot be undone.",
                "다른 서버 복사본이 있을 수 있어요. 로컬 복사본만 삭제하며 되돌릴 수 없어요.",
                "別のサーバー側コピーがある場合があります。ローカル側だけを削除し、元に戻せません。"
            ),
        ),
        Action::Restore | Action::UnlinkKeepServer | Action::UnlinkKeepLocal => {
            (name.to_owned(), t!("Applying…", "적용 중…", "適用中…"))
        }
    }
}

fn centered_fixed(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width.saturating_sub(2).max(1));
    let height = preferred_height.min(area.height.saturating_sub(2).max(1));
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
    use crate::app::ServerPlaylistRecoveryModal;
    use crate::open_subsonic::ServerPlaylistId;
    use crate::personal_state::PlaylistId;

    fn recovery_app(action: Action, stage: Stage) -> App {
        let mut app = App::new(50);
        app.server.library.playlist_recovery = Some(ServerPlaylistRecoveryModal {
            generation: 3,
            action,
            server_playlist_id: ServerPlaylistId::new("remote").unwrap(),
            local_playlist_id: PlaylistId::new("local").unwrap(),
            name: "Road Trip".to_owned(),
            stage,
        });
        app
    }

    fn draw(app: &App, width: u16, height: u16) -> String {
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
    fn destructive_recovery_modal_is_complete_at_thirty_by_thirty_in_all_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, delete, back) in [
            (crate::i18n::Language::English, "Delete", "Back"),
            (crate::i18n::Language::Korean, "삭제", "뒤로"),
            (crate::i18n::Language::Japanese, "削除", "戻る"),
        ] {
            crate::i18n::set_language(language);
            let app = recovery_app(Action::DeleteBoth, Stage::Confirming);
            let text = draw(&app, 30, 30);
            let comparable: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
            let delete: String = delete.chars().filter(|ch| !ch.is_whitespace()).collect();
            let back: String = back.chars().filter(|ch| !ch.is_whitespace()).collect();
            assert!(
                comparable.contains("Road") && comparable.contains("Trip"),
                "{language:?}: {text:?}"
            );
            assert!(comparable.contains(&delete), "{language:?}: {text:?}");
            assert!(comparable.contains(&back), "{language:?}: {text:?}");
            for (target, expected_width) in [
                (
                    MouseTarget::ConfirmServerPlaylistRecovery,
                    buttons::text_width(t!(" Delete ", " 삭제 ", " 削除 ")),
                ),
                (
                    MouseTarget::CancelServerPlaylistRecovery,
                    buttons::text_width(t!(" Back ", " 뒤로 ", " 戻る ")),
                ),
            ] {
                let rect = app.hits.rect_of_target(target).expect("recovery action");
                assert_eq!(rect.width, expected_width);
                assert!(rect.right() <= 30);
                assert!(rect.bottom() <= 30);
            }
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn applying_recovery_has_no_cancellable_mouse_target_at_thirty_by_thirty() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        crate::i18n::set_language(crate::i18n::Language::English);
        let app = recovery_app(Action::DeleteLocal, Stage::Applying);
        let text = draw(&app, 30, 30);
        let comparable: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(comparable.contains("Road") && comparable.contains("Trip"));
        assert!(comparable.contains("Applying"));
        assert!(
            app.hits
                .rect_of_target(MouseTarget::ConfirmServerPlaylistRecovery)
                .is_none()
        );
        assert!(
            app.hits
                .rect_of_target(MouseTarget::CancelServerPlaylistRecovery)
                .is_none()
        );
        crate::i18n::set_language(original);
    }

    #[test]
    fn delete_local_warning_is_complete_at_thirty_by_thirty_in_every_language() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, expected) in [
            (
                crate::i18n::Language::English,
                [
                    "Anotherservercopymayexist",
                    "onlythelocalcopy",
                    "cannotbeundone",
                ],
            ),
            (
                crate::i18n::Language::Korean,
                ["다른서버복사본", "로컬복사본만", "되돌릴수없"],
            ),
            (
                crate::i18n::Language::Japanese,
                ["別のサーバー側コピー", "ローカル側だけ", "元に戻せません"],
            ),
        ] {
            crate::i18n::set_language(language);
            let app = recovery_app(Action::DeleteLocal, Stage::Confirming);
            let text = draw(&app, 30, 30);
            let comparable: String = text
                .chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '│')
                .collect();
            for phrase in expected {
                assert!(comparable.contains(phrase), "{language:?}: {text:?}");
            }
            assert!(
                app.hits
                    .rect_of_target(MouseTarget::ConfirmServerPlaylistRecovery)
                    .is_some()
            );
            assert!(
                app.hits
                    .rect_of_target(MouseTarget::CancelServerPlaylistRecovery)
                    .is_some()
            );
        }
        crate::i18n::set_language(original);
    }
}
