use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::support::{
    SIMPLE_WIDTH, actions, centered, clipped_line, finish, message_modal, shell, single_line,
    wrapped_height,
};
use crate::app::App;
use crate::personal_state::ImportSummary;
use crate::t;
use crate::theme::ThemeRole as R;

pub(super) fn render_host(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    code: &str,
    expires_at_unix: i64,
    review: Option<&crate::sync::service::PairingReview>,
) {
    let popup = centered(area, SIMPLE_WIDTH, 16);
    let inner = shell(
        frame,
        app,
        popup,
        t!(" Add a device ", " 기기 추가 ", " デバイスを追加 "),
        R::BorderPrimary,
    );
    let intro = if review.is_some() {
        t!(
            "Check the device name and fingerprint before approving.",
            "승인 전에 기기 이름과 지문을 확인하세요.",
            "承認前にデバイス名とフィンガープリントを確認してください。"
        )
    } else {
        t!(
            "Enter this one-time code on the new device.",
            "새 기기에 이 일회용 코드를 입력하세요.",
            "新しいデバイスでこの一回限りのコードを入力します。"
        )
    };
    let rows = Layout::vertical([
        Constraint::Length(wrapped_height(intro, inner.width, 3)),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(intro)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[0],
    );
    let remaining = expires_at_unix.saturating_sub(crate::signals::unix_now());
    let expiry = if remaining <= 0 {
        t!("Code expired", "코드가 만료됨", "コードの期限が切れました").to_owned()
    } else {
        let minutes = remaining.saturating_add(59) / 60;
        match crate::i18n::current() {
            crate::i18n::Language::Korean => format!("{minutes}분 후 만료"),
            crate::i18n::Language::Japanese => format!("あと{minutes}分で期限切れ"),
            _ => format!("Expires in {minutes} min"),
        }
    };
    let code_lines = balanced_code_lines(code, rows[1].width as usize)
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                crate::ui::popup_style(app, R::Accent).add_modifier(Modifier::BOLD),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(code_lines).alignment(Alignment::Center),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(clipped_line(&expiry, rows[2].width as usize))
            .alignment(Alignment::Center)
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );
    if let Some(review) = review {
        frame.render_widget(
            Paragraph::new(vec![
                detail_line(
                    app,
                    t!("Device", "기기", "デバイス"),
                    &review.device_name,
                    rows[3].width,
                ),
                detail_line(
                    app,
                    t!("Fingerprint", "지문", "フィンガープリント"),
                    &review.fingerprint,
                    rows[3].width,
                ),
            ]),
            rows[3],
        );
        actions(
            frame,
            app,
            rows[4],
            None,
            t!(" Approve ", " 승인 ", " 承認 "),
            secondary_when_idle(app, t!(" Reject ", " 거절 ", " 拒否 ")),
        );
    } else {
        frame.render_widget(
            Paragraph::new(t!(
                "Waiting for the new device…",
                "새 기기를 기다리는 중…",
                "新しいデバイスを待っています…"
            ))
            .style(crate::ui::popup_style(app, R::TextMuted)),
            rows[3],
        );
        actions(
            frame,
            app,
            rows[4],
            None,
            t!(" Check again ", " 다시 확인 ", " 再確認 "),
            secondary_when_idle(app, t!(" Cancel ", " 취소 ", " キャンセル ")),
        );
    }
    finish(frame, app, popup);
}

fn balanced_code_lines(code: &str, width: usize) -> Vec<String> {
    let code = single_line(code);
    if width == 0 || UnicodeWidthStr::width(code.as_str()) <= width {
        return vec![code];
    }

    let groups = code.split('-').collect::<Vec<_>>();
    let mut best: Option<(usize, usize, String, String)> = None;
    for split in 1..groups.len() {
        let left = groups[..split].join("-");
        let right = groups[split..].join("-");
        let left_width = UnicodeWidthStr::width(left.as_str());
        let right_width = UnicodeWidthStr::width(right.as_str());
        if left_width > width || right_width > width {
            continue;
        }
        let imbalance = left_width.abs_diff(right_width);
        let widest = left_width.max(right_width);
        if best
            .as_ref()
            .is_none_or(|(best_imbalance, best_widest, _, _)| {
                (imbalance, widest) < (*best_imbalance, *best_widest)
            })
        {
            best = Some((imbalance, widest, left, right));
        }
    }
    if let Some((_, _, left, right)) = best {
        return vec![left, right];
    }

    vec![clipped_line(&code, width)]
}

fn secondary_when_idle<'a>(app: &App, label: &'a str) -> Option<&'a str> {
    app.personal_state.sync_ui.busy.is_none().then_some(label)
}

fn detail_line(app: &App, label: &str, value: &str, width: u16) -> Line<'static> {
    let prefix = format!("{label}: ");
    let prefix_width = UnicodeWidthStr::width(prefix.as_str()) as u16;
    Line::from(vec![
        Span::styled(prefix, crate::ui::popup_style(app, R::TextMuted)),
        Span::styled(
            clipped_line(value, width.saturating_sub(prefix_width) as usize),
            crate::ui::popup_style(app, R::SettingsValue),
        ),
    ])
}

pub(super) fn render_join_waiting(frame: &mut Frame, app: &App, area: Rect) {
    message_modal(
        frame,
        app,
        area,
        t!(
            " Waiting for approval ",
            " 승인 대기 중 ",
            " 承認を待っています "
        ),
        t!(
            "Approve this device on an existing device. Closing keeps the approval waiting safely.",
            "기존 기기에서 이 기기를 승인하세요. 화면을 닫아도 승인 대기 상태는 안전하게 유지됩니다.",
            "既存のデバイスでこのデバイスを承認してください。閉じても承認待ちの状態は安全に保持されます。"
        ),
        R::BorderPrimary,
        (
            t!(" Check again ", " 다시 확인 ", " 再確認 "),
            secondary_when_idle(app, t!(" Close ", " 닫기 ", " 閉じる ")),
        ),
    );
}

pub(super) fn render_join_preview(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    summary: &ImportSummary,
) {
    let popup = centered(area, SIMPLE_WIDTH, 15);
    let inner = shell(
        frame,
        app,
        popup,
        t!(" Review changes ", " 변경 사항 확인 ", " 変更内容を確認 "),
        R::BorderPrimary,
    );
    let introduction = t!(
        "The first merge keeps both sides. Nothing is deleted.",
        "첫 병합은 양쪽 데이터를 모두 유지하며 삭제하지 않습니다.",
        "最初の統合では両方のデータを残し、削除しません。"
    );
    let later_copy = t!(
        "Choose Later to keep this approved merge ready without changing local data.",
        "나중에를 선택하면 승인된 병합을 유지하며 로컬 데이터는 바꾸지 않습니다.",
        "「後で」を選ぶと、ローカルデータを変更せず承認済みの統合を保持します。"
    );
    let rows = Layout::vertical([
        Constraint::Length(wrapped_height(introduction, inner.width, 3)),
        Constraint::Min(3),
        Constraint::Length(wrapped_height(later_copy, inner.width, 3)),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(introduction)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(merge_summary_lines(summary))
            .style(crate::ui::popup_style(app, R::SettingsValue)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(later_copy)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[2],
    );
    actions(
        frame,
        app,
        rows[3],
        None,
        t!(" Merge ", " 병합 ", " 統合 "),
        secondary_when_idle(app, t!(" Later ", " 나중에 ", " 後で ")),
    );
    finish(frame, app, popup);
}

pub(super) fn render_revoke(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    device_id: &str,
    device_name: &str,
) {
    let name = if device_name.trim().is_empty() {
        device_id
    } else {
        device_name
    };
    let detail = format!(
        "{}\n{}: {}",
        t!(
            "Removing stops future changes, but cannot erase data already downloaded. Change the shared storage password if the device may be compromised.",
            "제거하면 이후 변경은 중단되지만 이미 받은 데이터는 지울 수 없습니다. 기기가 노출되었을 수 있다면 공유 저장소 비밀번호도 바꾸세요.",
            "削除すると今後の変更は止まりますが、取得済みのデータは消去できません。侵害された可能性がある場合は共有保存先のパスワードも変更してください。"
        ),
        t!("Device", "기기", "デバイス"),
        clipped_line(name, 40),
    );
    message_modal(
        frame,
        app,
        area,
        t!(" Remove device ", " 기기 제거 ", " デバイスを削除 "),
        &detail,
        R::Warning,
        (
            t!(" Remove ", " 제거 ", " 削除 "),
            secondary_when_idle(app, t!(" Cancel ", " 취소 ", " キャンセル ")),
        ),
    );
}

pub(super) fn render_discard_join(frame: &mut Frame, app: &App, area: Rect) {
    message_modal(
        frame,
        app,
        area,
        t!(
            " Discard unfinished connection? ",
            " 완료되지 않은 연결을 버릴까요? ",
            " 未完了の接続を破棄しますか？ "
        ),
        t!(
            "Only discard this attempt if it was never approved. If another device already approved it, keep it and remove the old device there before connecting again. Local listening data will not be removed.",
            "승인되지 않은 연결만 버리세요. 다른 기기에서 이미 승인했다면 이 연결을 유지하고, 다시 연결하기 전에 그 기기에서 이전 기기를 제거하세요. 이 기기의 감상 데이터는 삭제되지 않습니다.",
            "未承認の接続だけを破棄してください。別のデバイスで承認済みの場合はこの接続を保持し、再接続する前にそのデバイスで古いデバイスを削除してください。ローカルの再生データは削除されません。"
        ),
        R::Warning,
        (
            t!(" Discard ", " 버리기 ", " 破棄 "),
            secondary_when_idle(app, t!(" Keep ", " 유지 ", " 保持 ")),
        ),
    );
}

pub(super) fn render_result(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    success: bool,
    message: &str,
) {
    message_modal(
        frame,
        app,
        area,
        if success {
            t!(" Sync ready ", " 동기화 준비 완료 ", " 同期の準備完了 ")
        } else {
            t!(
                " Sync needs attention ",
                " 동기화 확인 필요 ",
                " 同期を確認してください "
            )
        },
        &single_line(message),
        if success { R::Success } else { R::Error },
        (t!(" Done ", " 완료 ", " 完了 "), None),
    );
}

fn merge_summary_lines(summary: &ImportSummary) -> Vec<Line<'static>> {
    let total_items = summary
        .favorites_added
        .saturating_add(summary.history_added)
        .saturating_add(summary.radio_favorites_added)
        .saturating_add(summary.playlists_added)
        .saturating_add(summary.playlist_entries_added)
        .saturating_add(summary.signal_tracks_added);
    match crate::i18n::current() {
        crate::i18n::Language::Korean => vec![
            Line::from(format!("새 변경 {}개", summary.operations_added)),
            Line::from(format!("유지할 항목 {total_items}개")),
            Line::from(format!(
                "이미 있는 중복 {}개 건너뜀",
                summary.duplicate_operations
            )),
        ],
        crate::i18n::Language::Japanese => vec![
            Line::from(format!("新しい変更: {}", summary.operations_added)),
            Line::from(format!("保持する項目: {total_items}")),
            Line::from(format!(
                "既存の重複をスキップ: {}",
                summary.duplicate_operations
            )),
        ],
        _ => vec![
            Line::from(format!("New changes: {}", summary.operations_added)),
            Line::from(format!("Items kept: {total_items}")),
            Line::from(format!(
                "Duplicates already present: {}",
                summary.duplicate_operations
            )),
        ],
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{DataMsg, Msg, SyncUiEvent};
    use crate::sync::service::PairingJoinWaiting;

    fn buffer_text(terminal: &Terminal<TestBackend>, width: u16) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn draw_host(app: &App, width: u16, height: u16, code: &str) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_host(
                    frame,
                    app,
                    frame.area(),
                    code,
                    crate::signals::unix_now().saturating_add(600),
                    None,
                );
            })
            .unwrap();
        buffer_text(&terminal, width)
    }

    fn draw_join_waiting(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_join_waiting(frame, app, frame.area()))
            .unwrap();
        buffer_text(&terminal, width)
    }

    fn draw_revoke(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_revoke(frame, app, frame.area(), "device-remote", "Living room");
            })
            .unwrap();
        buffer_text(&terminal, width)
    }

    fn draw_discard_join(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_discard_join(frame, app, frame.area()))
            .unwrap();
        buffer_text(&terminal, width)
    }

    #[test]
    fn merge_summary_uses_deletion_free_counts() {
        let summary = ImportSummary {
            operations_added: 4,
            duplicate_operations: 2,
            favorites_added: 1,
            history_added: 2,
            ..ImportSummary::default()
        };
        let text = merge_summary_lines(&summary)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("New changes: 4"));
        assert!(text.contains("Items kept: 3"));
        assert!(text.contains("Duplicates already present: 2"));
    }

    #[test]
    fn thirty_by_thirty_host_keeps_the_complete_pairing_code() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        let app = App::new(100);
        let code = "ABCDE-FGHIJ-KLMNO-PQRST-UVWXY-Z";

        assert_eq!(
            balanced_code_lines(code, 28),
            vec!["ABCDE-FGHIJ-KLMNO", "PQRST-UVWXY-Z"]
        );
        let text = draw_host(&app, 30, 30, code);

        assert!(text.contains("ABCDE-FGHIJ-KLMNO"));
        assert!(text.contains("PQRST-UVWXY-Z"));
        assert!(text.contains("Expires in"));
    }

    #[test]
    fn busy_lifecycle_modal_hides_its_secondary_action() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::English);
        let mut app = App::new(100);
        app.personal_state.sync_ui.flow_id = 1;
        let _ = app.update(Msg::Data(DataMsg::SyncUi(SyncUiEvent::JoinStarted {
            flow_id: 1,
            result: Box::new(Ok(PairingJoinWaiting {
                device_id: "dev-waiting".to_owned(),
                expires_at_unix: crate::signals::unix_now().saturating_add(600),
                resumed: false,
            })),
        })));

        let text = draw_join_waiting(&app, 30, 30);

        assert!(text.contains("Working"));
        assert!(!text.contains("Close"));
    }

    #[test]
    fn revoke_warns_that_downloaded_data_cannot_be_erased_in_every_language() {
        let _guard = crate::i18n::lock_for_test();
        for (language, warning) in [
            (crate::i18n::Language::English, "already downloaded"),
            (crate::i18n::Language::Korean, "이미 받은 데이터"),
            (crate::i18n::Language::Japanese, "取得済みのデータ"),
        ] {
            crate::i18n::set_language(language);
            let text = draw_revoke(&App::new(100), 30, 30);
            let compact = text
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            let expected = warning
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            assert!(compact.contains(&expected), "{language:?}: {text}");
        }
    }

    #[test]
    fn discard_join_warns_about_prior_approval_in_every_language() {
        let _guard = crate::i18n::lock_for_test();
        for (language, approval_a, approval_b, preserved) in [
            (
                crate::i18n::Language::English,
                "already",
                "approved",
                "removed",
            ),
            (crate::i18n::Language::Korean, "이미", "승인", "삭제되지"),
            (
                crate::i18n::Language::Japanese,
                "承認",
                "場合",
                "されません",
            ),
        ] {
            crate::i18n::set_language(language);
            let text = draw_discard_join(&App::new(100), 30, 30);
            let compact = text
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            for expected in [approval_a, approval_b, preserved] {
                let expected = expected
                    .chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>();
                assert!(compact.contains(&expected), "{language:?}: {text}");
            }
        }
    }
}
