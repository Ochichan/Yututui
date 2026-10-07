use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::app::{App, ListeningAction, ListeningDialog, ListeningTab, MouseTarget};
use crate::app::{ListeningControl, ListeningEdit};
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::text::truncate_to_width;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(dialog) = app.overlays.listening.as_ref() else {
        return;
    };
    if area.width < 4 || area.height < 4 {
        return;
    }
    let width = area.width.min(84);
    let height = area.height.min(23);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    crate::ui::render_popup_background(frame, app, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", dialog.tab.label()))
        .style(crate::ui::popup_style(app, R::TextPrimary))
        .border_style(crate::ui::popup_style(app, R::BorderFocused));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    register(app, dialog, popup, ListeningControl::Noop);
    let editing = dialog.editing.is_some() || dialog.error.is_some();
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::from(!editing)),
        Constraint::Min(1),
        Constraint::Length(if editing {
            0
        } else if inner.height >= 10 {
            2
        } else {
            1
        }),
        Constraint::Length(if editing {
            1
        } else if inner.height >= 10 && inner.width < 45 {
            4
        } else if inner.height >= 8 {
            3
        } else {
            1
        }),
    ])
    .split(inner);
    render_tabs(frame, app, dialog, rows[0]);
    let hint = match dialog.tab {
        ListeningTab::Bookmarks => t!(
            "Saved points and unfinished tracks",
            "저장한 시점과 이어들을 음원",
            "保存した時点と途中の音源"
        ),
        ListeningTab::Presets => t!(
            "Named recommendation preferences",
            "이름을 붙여 저장한 추천 설정",
            "名前を付けたおすすめ設定"
        ),
        ListeningTab::Passport => t!(
            "Stations heard for at least 30 seconds",
            "30초 이상 들은 방송국",
            "30秒以上聴いた放送局"
        ),
    };
    let conflict = dialog.rows.get(dialog.selected).is_some_and(|row| {
        dialog
            .rows
            .iter()
            .filter(|candidate| row.same_record(candidate))
            .count()
            > 1
    });
    let hint = if conflict && dialog.editing.is_none() {
        if matches!(
            dialog.rows.get(dialog.selected),
            Some(crate::app::ListeningRow::Resume(_))
        ) {
            t!(
                "Conflicting resume points · choose where to continue",
                "서로 다른 재생 위치 · 이어들을 시점을 고르세요",
                "異なる再生位置 · 続ける時点を選んでください"
            )
        } else {
            t!(
                "Conflicting versions · edit the version you want to keep",
                "충돌한 기록 · 남길 버전을 골라 수정하세요",
                "競合した記録 · 残す版を選んで編集してください"
            )
        }
        .to_owned()
    } else if dialog.tab == ListeningTab::Presets {
        app.personal_state
            .listening
            .active_preset_name()
            .map(|name| {
                format!(
                    "{}: {name}",
                    t!("Loaded preset", "불러온 프리셋", "読み込んだプリセット")
                )
            })
            .unwrap_or_else(|| hint.to_owned())
    } else {
        hint.to_owned()
    };
    frame.render_widget(
        Paragraph::new(truncate_to_width(&hint, rows[1].width.into()))
            .style(crate::ui::popup_style(app, R::TextMuted)),
        rows[1],
    );
    if let Some(error) = &dialog.error {
        render_scrolled_text(frame, app, dialog, error, rows[2], R::Accent);
    } else if dialog.editing.is_some() {
        render_editor(frame, app, dialog, rows[2]);
    } else {
        render_rows(frame, app, dialog, rows[2]);
    }
    let detail = dialog.error.clone().unwrap_or_else(|| {
        dialog
            .rows
            .get(dialog.selected)
            .filter(|_| !editing)
            .map(|row| row.detail(app))
            .unwrap_or_default()
    });
    frame.render_widget(
        Paragraph::new(wrapped(&detail, rows[3].width)).style(crate::ui::popup_style(
            app,
            if dialog.error.is_some() {
                R::Accent
            } else {
                R::TextMuted
            },
        )),
        rows[3],
    );
    render_controls(frame, app, dialog, rows[4]);
    crate::ui::seal_popup_background(frame, app, popup);
    crate::ui::mark_art_rows_for_popup(frame, app, popup);
}

fn render_tabs(frame: &mut Frame, app: &App, dialog: &ListeningDialog, area: Rect) {
    let columns = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(area);
    for (tab, rect) in ListeningTab::ALL.into_iter().zip(columns.iter().copied()) {
        let style = crate::ui::popup_style(
            app,
            if tab == dialog.tab {
                R::Accent
            } else {
                R::TextMuted
            },
        );
        frame.render_widget(
            Paragraph::new(truncate_to_width(
                tab.label(),
                rect.width.saturating_sub(1).into(),
            ))
            .style(style.add_modifier(if tab == dialog.tab {
                Modifier::BOLD
            } else {
                Modifier::empty()
            })),
            rect,
        );
        if dialog.editing.is_none() {
            register(app, dialog, rect, ListeningControl::Tab(tab));
        }
    }
}

fn render_rows(frame: &mut Frame, app: &App, dialog: &ListeningDialog, area: Rect) {
    if dialog.rows.is_empty() {
        let empty = match dialog.tab {
            ListeningTab::Bookmarks => t!(
                "No bookmarks yet. Play a track, then choose Add to save this moment.",
                "아직 북마크가 없어요. 음원을 재생한 뒤 추가를 눌러 현재 시점을 저장하세요.",
                "ブックマークはまだありません。音源を再生して追加を選んでください。"
            ),
            ListeningTab::Presets => t!(
                "No presets yet. Set your station preferences, then choose Save.",
                "아직 프리셋이 없어요. 스테이션에서 추천 설정을 조절한 뒤 저장하세요.",
                "プリセットはまだありません。ステーションで好みを調整して保存してください。"
            ),
            ListeningTab::Passport => t!(
                "Your listening journey starts here. Listen to a station for 30 seconds to add it.",
                "청취 여권의 첫 기록을 남겨보세요. 방송국을 30초 이상 들으면 기록돼요.",
                "最初のリスニング記録を残しましょう。放送局を30秒以上聴くと記録されます。"
            ),
        };
        frame.render_widget(
            Paragraph::new(wrapped(empty, area.width))
                .style(crate::ui::popup_style(app, R::TextMuted)),
            area,
        );
        return;
    }
    let start = dialog
        .selected
        .saturating_sub(usize::from(area.height).saturating_sub(1));
    for (line, (index, row)) in dialog
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(area.height.into())
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + line as u16, area.width, 1);
        let selected = index == dialog.selected;
        let conflict = dialog
            .rows
            .iter()
            .filter(|candidate| row.same_record(candidate))
            .count()
            > 1;
        let label = format!(
            "{} {}{}",
            if selected { ">" } else { " " },
            if conflict { "! " } else { "" },
            row.label()
        );
        frame.render_widget(
            Paragraph::new(truncate_to_width(&label, area.width.into())).style(
                crate::ui::popup_style(app, if selected { R::Accent } else { R::TextPrimary }),
            ),
            rect,
        );
        register(app, dialog, rect, ListeningControl::Select(index));
    }
}

fn render_editor(frame: &mut Frame, app: &App, dialog: &ListeningDialog, area: Rect) {
    if let Some(ListeningEdit::Inspect(row)) = &dialog.editing {
        render_scrolled_text(
            frame,
            app,
            dialog,
            &row.full_detail(app),
            area,
            R::TextPrimary,
        );
        return;
    }
    let title = match dialog.editing.as_ref() {
        Some(ListeningEdit::Enable) => t!(
            "Enable automatic listening records? Update paired devices first. These records use a new sync format; older versions will stop syncing.",
            "청취 기록을 자동으로 저장할까요? 연결한 기기를 먼저 업데이트하세요. 새 동기화 형식을 쓰므로 구버전은 동기화를 멈춥니다.",
            "リスニング記録を自動保存しますか？先に連携端末を更新してください。新しい同期形式のため、旧版は同期を停止します。"
        ),
        Some(ListeningEdit::New) => t!("Name this record", "기록 이름", "記録の名前"),
        Some(ListeningEdit::Rename(_)) => t!(
            "Edit the selected record",
            "선택한 기록 수정",
            "選択した記録を編集"
        ),
        Some(ListeningEdit::Overwrite(_)) => t!(
            "Replace this preset with your current preferences?",
            "현재 추천 설정으로 이 프리셋을 덮어쓸까요?",
            "現在のおすすめ設定で上書きしますか？"
        ),
        Some(ListeningEdit::Delete(_)) => t!(
            "Delete this record on all synced devices?",
            "연결된 모든 기기에서 이 기록을 삭제할까요?",
            "同期したすべての端末からこの記録を削除しますか？"
        ),
        Some(ListeningEdit::ClearPassport) => t!(
            "Clear all passport visits and notes on all synced devices?",
            "연결된 모든 기기에서 청취 여권과 메모를 비울까요?",
            "同期したすべての端末のパスポートとメモを消去しますか？"
        ),
        Some(ListeningEdit::Inspect(_)) | None => return,
    };
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    frame.render_widget(
        Paragraph::new(wrapped(title, rows[0].width))
            .style(crate::ui::popup_style(app, R::TextPrimary)),
        rows[0],
    );
    if dialog.text_entry() {
        let window = crate::ui::text::editable_window(
            &dialog.input,
            dialog.cursor.byte_index(&dialog.input),
            area.width.saturating_sub(2).into(),
        );
        let line = Line::from(vec![
            Span::raw("> "),
            Span::raw(window.before),
            crate::ui::anim::caret_span(
                app,
                crate::ui::popup_style(app, R::Accent),
                crate::ui::popup_bg(app),
            ),
            Span::raw(window.after),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(crate::ui::popup_style(app, R::TextPrimary)),
            rows[1],
        );
    }
}

fn render_controls(frame: &mut Frame, app: &App, dialog: &ListeningDialog, area: Rect) {
    if dialog.error.is_some() || matches!(dialog.editing, Some(ListeningEdit::Inspect(_))) {
        let label = t!(
            "Esc Back · ↑/↓ Scroll",
            "Esc 뒤로 · ↑/↓ 스크롤",
            "Esc 戻る · ↑/↓ スクロール"
        );
        frame.render_widget(
            Paragraph::new(label).style(crate::ui::popup_style(app, R::HelpAction)),
            area,
        );
        register(
            app,
            dialog,
            Rect::new(area.x, area.y, 8.min(area.width), 1),
            ListeningControl::Cancel,
        );
        return;
    }
    let mut controls = Vec::new();
    if dialog.editing.is_some() {
        controls.push((
            t!("Enter Confirm", "Enter 확인", "Enter 確認"),
            ListeningControl::Confirm,
        ));
        controls.push((
            t!("Esc Cancel", "Esc 취소", "Esc 戻る"),
            ListeningControl::Cancel,
        ));
    } else {
        controls.push((
            t!("Esc Close", "Esc 닫기", "Esc 閉じる"),
            ListeningControl::Close,
        ));
        controls.push((
            t!("Enter Open", "Enter 열기", "Enter 開く"),
            ListeningControl::Activate,
        ));
        match dialog.tab {
            ListeningTab::Bookmarks => {
                controls.push((t!("n Add", "n 추가", "n 追加"), ListeningControl::New));
                controls.push((
                    t!(
                        "r Restart current",
                        "r 현재 곡 처음부터",
                        "r 再生中を最初から"
                    ),
                    ListeningControl::Restart,
                ));
                controls.push((
                    if app.listening_resume_enabled() {
                        t!("a Resume on", "a 이어듣기 켜짐", "a 再開オン")
                    } else {
                        t!("a Resume off", "a 이어듣기 꺼짐", "a 再開オフ")
                    },
                    ListeningControl::ToggleResume,
                ));
            }
            ListeningTab::Presets => {
                controls.push((t!("n Save", "n 저장", "n 保存"), ListeningControl::New));
                controls.push((
                    t!("s Overwrite", "s 덮어쓰기", "s 上書き"),
                    ListeningControl::Overwrite,
                ));
            }
            ListeningTab::Passport => controls.push((
                t!("C Clear all", "C 모두 비우기", "C 全消去"),
                ListeningControl::ClearPassport,
            )),
        }
        controls.push((t!("e Edit", "e 수정", "e 編集"), ListeningControl::Edit));
        controls.push((
            t!("i Details", "i 상세", "i 詳細"),
            ListeningControl::Details,
        ));
        controls.push((
            t!("Del Delete", "Del 삭제", "Del 削除"),
            ListeningControl::Delete,
        ));
    }
    let mut x = area.x;
    let mut y = area.y;
    for (label, control) in controls {
        let key = control_key(app, control);
        let text = label.split_once(' ').map_or(label, |(_, text)| text);
        let label = format!("{key} {text}");
        let width = crate::ui::buttons::text_width(&label)
            .saturating_add(2)
            .min(area.width);
        if x.saturating_add(width) > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(x, y, width, 1);
        frame.render_widget(
            Paragraph::new(truncate_to_width(&label, width.into()))
                .style(crate::ui::popup_style(app, R::HelpAction)),
            rect,
        );
        register(app, dialog, rect, control);
        x = x.saturating_add(width);
    }
}

fn control_key(app: &App, control: ListeningControl) -> String {
    use crate::keymap::{Action, KeyContext};
    let action = match control {
        ListeningControl::Activate => Action::Confirm,
        ListeningControl::New => Action::ListeningAdd,
        ListeningControl::Edit => Action::ListeningEdit,
        ListeningControl::Details => Action::ListeningDetails,
        ListeningControl::Overwrite => Action::ListeningOverwrite,
        ListeningControl::Delete => Action::ListeningDelete,
        ListeningControl::Restart => Action::ListeningRestart,
        ListeningControl::ToggleResume => Action::ListeningToggleResume,
        ListeningControl::ClearPassport => Action::ListeningClearPassport,
        ListeningControl::Confirm => return "Enter".to_owned(),
        _ => return "Esc".to_owned(),
    };
    app.keymap
        .label_for_display(KeyContext::Listening, action, app.retro_mode())
}

fn wrapped(text: &str, width: u16) -> Vec<Line<'static>> {
    text.lines()
        .flat_map(|line| crate::ui::text::wrap_to_width(line, usize::from(width)))
        .map(Line::from)
        .collect()
}

fn render_scrolled_text(
    frame: &mut Frame,
    app: &App,
    dialog: &ListeningDialog,
    text: &str,
    area: Rect,
    role: R,
) {
    let lines = wrapped(text, area.width);
    let max_scroll = lines
        .len()
        .saturating_sub(usize::from(area.height))
        .min(usize::from(u16::MAX)) as u16;
    dialog.detail_max_scroll.set(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((dialog.detail_scroll.min(max_scroll), 0))
            .style(crate::ui::popup_style(app, role)),
        area,
    );
}

fn register(app: &App, dialog: &ListeningDialog, area: Rect, control: ListeningControl) {
    app.register_mouse_button(
        area,
        MouseTarget::Listening(ListeningAction {
            revision: dialog.revision,
            control,
        }),
    );
}
