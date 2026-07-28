use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::widgets::{Paragraph, Wrap};

use super::support::{FORM_WIDTH, actions, centered, finish, render_field, shell, wrapped_height};
use crate::app::{App, MouseTarget, SyncConnectionField, SyncConnectionForm, SyncRecoveryForm};
use crate::t;
use crate::theme::ThemeRole as R;

const CONNECTION_HEIGHT: u16 = 23;
const RECOVERY_HEIGHT: u16 = 15;

pub(super) fn render_connection(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    form: &SyncConnectionForm,
    join: bool,
    confirming: bool,
) {
    let popup = centered(area, FORM_WIDTH, CONNECTION_HEIGHT);
    let title = if join {
        t!(
            " Join personal sync ",
            " 개인 동기화에 연결 ",
            " 個人データ同期に参加 "
        )
    } else {
        t!(
            " Set up personal sync ",
            " 개인 동기화 설정 ",
            " 個人データ同期を設定 "
        )
    };
    let inner = shell(frame, app, popup, title, R::BorderPrimary);
    let show_description = inner.height >= 18;
    let show_hint = inner.height >= 16;
    let description = if join {
        t!(
            "Enter the same storage login and the one-time code.",
            "같은 저장소 로그인과 일회용 코드를 입력하세요.",
            "同じ保存先のログイン情報と一回限りのコードを入力します。"
        )
    } else {
        t!(
            "Your listening data is encrypted before it leaves this device.",
            "감상 데이터는 이 기기를 떠나기 전에 암호화됩니다.",
            "再生データはこのデバイスから送信される前に暗号化されます。"
        )
    };
    let prompt = confirming.then(|| {
        if join {
            t!(
                "Connect with these details? You will review the merge before saving.",
                "이 정보로 연결할까요? 저장 전 병합 내용을 확인합니다.",
                "この情報で接続しますか？保存前に統合内容を確認します。"
            )
        } else {
            t!(
                "The recovery kit must be saved to finish setup.",
                "설정을 마치려면 복구 키트를 저장해야 합니다.",
                "設定を完了するには復旧キットの保存が必要です。"
            )
        }
    });
    let description_height =
        u16::from(show_description).saturating_mul(wrapped_height(description, inner.width, 3));
    let prompt_height = prompt.map_or(0, |copy| wrapped_height(copy, inner.width, 3));
    let rows = Layout::vertical([
        Constraint::Length(description_height),
        Constraint::Length(prompt_height),
        Constraint::Min(1),
        Constraint::Length(u16::from(show_hint)),
        Constraint::Length(1),
    ])
    .split(inner);

    if show_description {
        frame.render_widget(
            Paragraph::new(description)
                .wrap(Wrap { trim: true })
                .style(crate::ui::popup_style(app, R::TextMuted)),
            rows[0],
        );
    }
    if let Some(prompt) = prompt {
        frame.render_widget(
            Paragraph::new(prompt)
                .wrap(Wrap { trim: true })
                .style(crate::ui::popup_style(app, R::Warning)),
            rows[1],
        );
    }

    render_connection_fields(frame, app, rows[2], form, join, confirming);
    if show_hint {
        let hint = if form.current_field(join).is_secret()
            && form.current_secret_is_revealed(join)
            && !confirming
        {
            t!(
                "Ctrl+R hide · Enter continue · Esc cancel",
                "Ctrl+R 숨기기 · Enter 계속 · Esc 취소",
                "Ctrl+R 隠す · Enter 続行 · Esc キャンセル"
            )
        } else if form.current_field(join).is_secret() && !confirming {
            t!(
                "Ctrl+R show · Enter continue · Esc cancel",
                "Ctrl+R 보기 · Enter 계속 · Esc 취소",
                "Ctrl+R 表示 · Enter 続行 · Esc キャンセル"
            )
        } else {
            t!(
                "↑/↓ field · Enter continue · Esc cancel",
                "↑/↓ 항목 · Enter 계속 · Esc 취소",
                "↑/↓ 項目 · Enter 続行 · Esc キャンセル"
            )
        };
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Center)
                .style(crate::ui::popup_style(app, R::TextMuted)),
            rows[3],
        );
    }
    let reveal =
        form.current_field(join)
            .is_secret()
            .then_some(if form.current_secret_is_revealed(join) {
                t!(" Hide ", " 숨기기 ", " 隠す ")
            } else {
                t!(" Show ", " 보기 ", " 表示 ")
            });
    let primary = if join {
        t!(" Join ", " 연결 ", " 接続 ")
    } else if confirming {
        t!(" Confirm ", " 확인 ", " 確認 ")
    } else {
        t!(" Continue ", " 계속 ", " 続行 ")
    };
    actions(
        frame,
        app,
        rows[4],
        reveal,
        primary,
        Some(if confirming {
            t!(" Back ", " 뒤로 ", " 戻る ")
        } else {
            t!(" Cancel ", " 취소 ", " キャンセル ")
        }),
    );
    finish(frame, app, popup);
}

fn render_connection_fields(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    form: &SyncConnectionForm,
    join: bool,
    confirming: bool,
) {
    let fields = form.fields(join);
    if area.height == 0 || fields.is_empty() {
        return;
    }
    let show_all = area.height as usize >= fields.len();
    let visible: Vec<(usize, SyncConnectionField)> = if show_all {
        fields.iter().copied().enumerate().collect()
    } else {
        let index = form.field.min(fields.len() - 1);
        vec![(index, fields[index])]
    };
    let row_height = if show_all && area.height as usize >= fields.len().saturating_mul(2) {
        2
    } else {
        1
    };
    for (visible_index, (field_index, field)) in visible.iter().copied().enumerate() {
        let y = area
            .y
            .saturating_add((visible_index as u16).saturating_mul(row_height));
        if y >= area.bottom() {
            break;
        }
        let rect = Rect {
            x: area.x,
            y,
            width: area.width,
            height: row_height.min(area.bottom().saturating_sub(y)),
        };
        app.register_mouse_button(rect, MouseTarget::SyncWizardField(field_index));
        let selected = form.field == field_index;
        let editor = (selected && !confirming).then(|| {
            (
                form.current_value(join),
                form.cursor.byte_index(form.current_value(join)),
                field.is_secret() && !form.current_secret_is_revealed(join),
            )
        });
        render_field(
            frame,
            app,
            rect,
            field_label(field),
            &form.display_value(field),
            selected,
            editor,
        );
    }
}

pub(super) fn render_recovery(frame: &mut Frame, app: &App, area: Rect, form: &SyncRecoveryForm) {
    let popup = centered(area, FORM_WIDTH, RECOVERY_HEIGHT);
    let inner = shell(
        frame,
        app,
        popup,
        t!(
            " Save recovery kit ",
            " 복구 키트 저장 ",
            " 復旧キットを保存 "
        ),
        R::BorderPrimary,
    );
    let description = t!(
        "Copy the existing kit to a safe folder. The sync password is not included.",
        "기존 키트를 안전한 폴더에 복사하세요. 동기화 비밀번호는 포함되지 않습니다.",
        "既存のキットを安全なフォルダへコピーします。同期パスワードは含まれません。"
    );
    let confirm_copy = t!(
        "Confirm that the destination is private and safe.",
        "저장 위치가 안전하고 비공개인지 확인하세요.",
        "保存先が安全で非公開であることを確認してください。"
    );
    let rows = Layout::vertical([
        Constraint::Length(wrapped_height(description, inner.width, 3)),
        Constraint::Min(4),
        Constraint::Length(u16::from(form.confirm).saturating_mul(wrapped_height(
            confirm_copy,
            inner.width,
            2,
        ))),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(description)
            .wrap(Wrap { trim: true })
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[0],
    );
    let field_height = (rows[1].height / 2).max(1);
    for (index, (label, value)) in [
        (
            t!("Current kit", "현재 키트", "現在のキット"),
            form.source.as_str(),
        ),
        (
            t!("Save in", "저장 위치", "保存先"),
            form.destination.as_str(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let y = rows[1]
            .y
            .saturating_add((index as u16).saturating_mul(field_height));
        if y >= rows[1].bottom() {
            break;
        }
        let rect = Rect {
            x: rows[1].x,
            y,
            width: rows[1].width,
            height: field_height.min(rows[1].bottom().saturating_sub(y)),
        };
        app.register_mouse_button(rect, MouseTarget::SyncWizardField(index));
        let selected = form.field == index;
        let editor =
            (selected && !form.confirm).then(|| (value, form.cursor.byte_index(value), false));
        render_field(frame, app, rect, label, value, selected, editor);
    }
    if form.confirm {
        frame.render_widget(
            Paragraph::new(confirm_copy)
                .wrap(Wrap { trim: true })
                .style(crate::ui::popup_style(app, R::Warning)),
            rows[2],
        );
    }
    actions(
        frame,
        app,
        rows[3],
        None,
        t!(" Save ", " 저장 ", " 保存 "),
        Some(if form.confirm {
            t!(" Back ", " 뒤로 ", " 戻る ")
        } else {
            t!(" Cancel ", " 취소 ", " キャンセル ")
        }),
    );
    finish(frame, app, popup);
}

fn field_label(field: SyncConnectionField) -> &'static str {
    match field {
        SyncConnectionField::Endpoint => {
            t!("Sync address", "동기화 주소", "同期アドレス")
        }
        SyncConnectionField::Username => t!("Username", "사용자 이름", "ユーザー名"),
        SyncConnectionField::Secret => t!(
            "Password or access token",
            "비밀번호 또는 액세스 토큰",
            "パスワードまたはアクセストークン"
        ),
        SyncConnectionField::DeviceName => t!("Device name", "기기 이름", "デバイス名"),
        SyncConnectionField::CustomCa => t!(
            "CA certificate (optional)",
            "CA 인증서 (선택)",
            "CA 証明書（任意）"
        ),
        SyncConnectionField::RecoveryFile => {
            t!("Recovery kit file", "복구 키트 파일", "復旧キットファイル")
        }
        SyncConnectionField::PairingCode => {
            t!("One-time code", "일회용 코드", "一回限りのコード")
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::render_sync_wizard;
    use crate::app::{App, SyncConnectionForm, SyncWizard};

    fn draw(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_sync_wizard(frame, app, frame.area()))
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
    fn join_form_masks_password_and_pairing_code_by_default() {
        let mut form = SyncConnectionForm::join();
        *form.current_value_mut(true) = "https://sync.example.invalid/dav".to_owned();
        form.select_field(true, 1);
        *form.current_value_mut(true) = "listener".to_owned();
        form.select_field(true, 1);
        *form.current_value_mut(true) = "password-sentinel".to_owned();
        form.select_field(true, 1);
        *form.current_value_mut(true) = "Laptop".to_owned();
        form.select_field(true, 1);
        *form.current_value_mut(true) = "/private/custom-ca.pem".to_owned();
        form.select_field(true, 1);
        *form.current_value_mut(true) = "PAIRING-CODE-SENTINEL".to_owned();

        let mut app = App::new(100);
        app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
            form,
            confirm: false,
        });
        let text = draw(&app, 80, 30);

        assert!(text.contains("sync.example.invalid"));
        assert!(text.contains('•'));
        assert!(!text.contains("password-sentinel"));
        assert!(!text.contains("PAIRING-CODE-SENTINEL"));
    }

    #[test]
    fn thirty_by_thirty_keeps_title_current_field_and_buttons_visible() {
        let mut form = SyncConnectionForm::join();
        form.select_field(true, 2);
        *form.current_value_mut(true) = "hidden".to_owned();
        let mut app = App::new(100);
        app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
            form,
            confirm: false,
        });

        let text = draw(&app, 30, 30);
        assert!(text.contains("Join personal sync"));
        assert!(text.contains("Password or access"));
        assert!(text.contains("Join"));
        assert!(text.contains("Cancel"));
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn thirty_column_setup_keeps_the_complete_encryption_notice_in_every_language() {
        let _guard = crate::i18n::lock_for_test();
        for (language, final_phrase) in [
            (crate::i18n::Language::English, "this device"),
            (crate::i18n::Language::Korean, "암호화됩니다"),
            (crate::i18n::Language::Japanese, "暗号化されます"),
        ] {
            crate::i18n::set_language(language);
            let mut app = App::new(100);
            app.personal_state.sync_ui.wizard = Some(SyncWizard::Setup {
                form: SyncConnectionForm::setup(),
                confirm: false,
            });
            let text = draw(&app, 30, 30);
            let compact = text
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            let expected = final_phrase
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>();
            assert!(compact.contains(&expected), "{language:?}: {text}");
        }
    }

    #[test]
    fn long_selected_value_scrolls_to_the_text_cursor() {
        let mut form = SyncConnectionForm::join();
        *form.current_value_mut(true) =
            "https://sync.example.invalid/very/long/path/TAIL-END".to_owned();
        form.cursor = crate::util::text_edit::TextCursor::at_end(form.current_value(true));
        let mut app = App::new(100);
        app.personal_state.sync_ui.wizard = Some(SyncWizard::Join {
            form,
            confirm: false,
        });

        let text = draw(&app, 30, 30);
        assert!(text.contains("TAIL-END"), "{text}");
        assert!(!text.contains("sync.example.invalid"), "{text}");
    }
}
