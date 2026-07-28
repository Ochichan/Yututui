//! Deletion-free first-sync preview for explicit server-playlist imports and links.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, MouseTarget, ServerPlaylistPreviewKind, ServerPlaylistPreviewStage};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(modal) = app.server.library.playlist_preview.as_ref() else {
        return;
    };
    let popup = centered_fixed(area, 58, 13);
    crate::ui::render_popup_background(frame, app, popup);

    let title = match modal.kind {
        ServerPlaylistPreviewKind::ImportCopy => t!(
            " Import playlist copy ",
            " 플레이리스트 복사본 가져오기 ",
            " プレイリストをコピーとしてインポート "
        ),
        ServerPlaylistPreviewKind::LinkAndSync => t!(
            " Link playlist ",
            " 플레이리스트 연결 ",
            " プレイリストをリンク "
        ),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(crate::ui::confirm_border_style(app))
        .style(crate::ui::popup_style(app, R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    match &modal.stage {
        ServerPlaylistPreviewStage::Preparing { .. } => {
            render_busy(
                frame,
                app,
                inner,
                t!(
                    "Preparing preview…",
                    "미리보기를 준비하는 중…",
                    "プレビューを準備しています…"
                ),
            );
        }
        ServerPlaylistPreviewStage::Ready(preview) => {
            render_preview(frame, app, inner, preview, false);
        }
        ServerPlaylistPreviewStage::Applying(preview) => {
            render_preview(frame, app, inner, preview, true);
        }
    }

    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn render_busy(frame: &mut Frame, app: &App, area: Rect, message: &str) {
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[0],
    );
    let segments = [buttons::Seg::button(
        MouseTarget::CancelServerPlaylistPreview,
        t!(" Back (Esc) ", " 뒤로 (Esc) ", " 戻る (Esc) "),
    )];
    buttons::render_segments(
        frame,
        app,
        rows[2],
        &segments,
        crate::ui::confirm_button_style(app),
        crate::ui::confirm_gap_style(app),
        Alignment::Center,
    );
}

fn render_preview(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    preview: &crate::open_subsonic::PlaylistMergePreview,
    applying: bool,
) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(format!("{}: {}", t!("Name", "이름", "名前"), preview.name))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(match crate::i18n::current() {
            crate::i18n::Language::Korean => {
                format!("YuTuTui에 추가: {}", preview.add_to_local)
            }
            crate::i18n::Language::Japanese => {
                format!("YuTuTui に追加: {}", preview.add_to_local)
            }
            _ => format!("Add to YuTuTui: {}", preview.add_to_local),
        })
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::Accent)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(match crate::i18n::current() {
            crate::i18n::Language::Korean => {
                format!("서버에 추가: {}", preview.add_to_server)
            }
            crate::i18n::Language::Japanese => {
                format!("サーバーに追加: {}", preview.add_to_server)
            }
            _ => format!("Add to server: {}", preview.add_to_server),
        })
        .alignment(Alignment::Center)
        .style(crate::ui::popup_style(app, R::Accent)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(first_sync_message())
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[4],
    );

    if applying {
        let segments = [buttons::Seg::label(t!(
            " Applying… ",
            " 적용 중… ",
            " 適用中… "
        ))];
        buttons::render_segments(
            frame,
            app,
            rows[6],
            &segments,
            crate::ui::popup_style(app, R::TextMuted),
            crate::ui::confirm_gap_style(app),
            Alignment::Center,
        );
    } else {
        let segments = [
            buttons::Seg::button(
                MouseTarget::ConfirmServerPlaylistPreview,
                t!(" Apply ", " 적용 ", " 適用 "),
            ),
            buttons::Seg::label("  "),
            buttons::Seg::button(
                MouseTarget::CancelServerPlaylistPreview,
                t!(" Back ", " 뒤로 ", " 戻る "),
            ),
        ];
        buttons::render_segments(
            frame,
            app,
            rows[6],
            &segments,
            crate::ui::confirm_button_style(app),
            crate::ui::confirm_gap_style(app),
            Alignment::Center,
        );
    }
}

pub(crate) fn first_sync_message() -> &'static str {
    t!(
        "Nothing will be removed in this first sync.",
        "첫 동기화에서는 아무것도 삭제하지 않아요.",
        "初回同期では何も削除しません。"
    )
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
    use crate::open_subsonic::{PlaylistMergePreview, PlaylistPreviewMode, ServerPlaylistId};
    use crate::personal_state::PlaylistId;

    fn ready_app() -> App {
        let mut app = App::new(50);
        app.server.library.playlist_preview = Some(crate::app::ServerPlaylistPreviewModal {
            generation: 7,
            kind: ServerPlaylistPreviewKind::LinkAndSync,
            stage: ServerPlaylistPreviewStage::Ready(PlaylistMergePreview {
                preview_id: "preview".to_owned(),
                mode: PlaylistPreviewMode::LinkNew,
                server_playlist_id: ServerPlaylistId::new("remote").unwrap(),
                local_playlist_id: PlaylistId::new("local").unwrap(),
                name: "Road Trip".to_owned(),
                remote_tracks: 12,
                add_to_local: 12,
                add_to_server: 3,
            }),
        });
        app
    }

    #[test]
    fn first_sync_copy_is_exact_in_all_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, expected) in [
            (
                crate::i18n::Language::English,
                "Nothing will be removed in this first sync.",
            ),
            (
                crate::i18n::Language::Korean,
                "첫 동기화에서는 아무것도 삭제하지 않아요.",
            ),
            (
                crate::i18n::Language::Japanese,
                "初回同期では何も削除しません。",
            ),
        ] {
            crate::i18n::set_language(language);
            assert_eq!(first_sync_message(), expected);
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn ready_preview_fits_thirty_by_thirty_and_registers_both_actions() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        crate::i18n::set_language(crate::i18n::Language::English);
        let app = ready_app();
        let backend = TestBackend::new(30, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Road Trip"));
        assert!(text.contains("12"));
        assert!(text.contains("3"));
        assert!(
            app.hits
                .rect_of_target(MouseTarget::ConfirmServerPlaylistPreview)
                .is_some()
        );
        assert!(
            app.hits
                .rect_of_target(MouseTarget::CancelServerPlaylistPreview)
                .is_some()
        );
        crate::i18n::set_language(original);
    }
}
