//! Plain-language Sync area selector and music-server settings/wizard rendering.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, ListItem, Paragraph, Wrap};
use zeroize::Zeroizing;

use crate::app::{
    App, MouseTarget, MusicServerBusy, MusicServerCredentialMode, MusicServerHealth,
    MusicServerHistoryHealth, MusicServerSetupField, MusicServerWizard, SyncArea,
};
use crate::open_subsonic::{PlaylistCreateAttention, PlaylistCreateRecoveryState};
use crate::settings::SettingsState;
use crate::settings::sync::health_label;
use crate::t;
use crate::theme::ThemeRole as R;
use crate::ui::buttons;

pub(crate) fn render_sync_area_selector(
    frame: &mut Frame,
    app: &App,
    settings: &SettingsState,
    area: Rect,
) {
    if area.is_empty() {
        return;
    }
    let theme = &settings.draft.theme;
    // Full names when all four fit; the compact names otherwise (they fit 28 columns).
    let separators = 3 * (SyncArea::ALL.len() - 1);
    let full_width: usize = SyncArea::ALL
        .iter()
        .map(|area| usize::from(buttons::text_width(area.label())))
        .sum::<usize>()
        + separators;
    let full = full_width <= usize::from(area.width);
    let mut x = area.x;
    let mut spans = Vec::new();
    for (index, sync_area) in SyncArea::ALL.iter().copied().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme.style(R::TextMuted)));
            x = x.saturating_add(3);
        }
        let label = if full {
            sync_area.label()
        } else {
            compact_area_label(sync_area)
        };
        let width = buttons::text_width(label).min(area.right().saturating_sub(x));
        let selected = app.server.settings.area == sync_area;
        spans.push(Span::styled(
            label,
            if selected {
                Style::default()
                    .fg(theme.color(R::SelectionFg))
                    .bg(theme.color(R::SelectionBg))
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.style(R::TextMuted)
            },
        ));
        if width > 0 {
            app.register_mouse_button(
                Rect {
                    x,
                    y: area.y,
                    width,
                    height: 1,
                },
                MouseTarget::SettingsSyncArea(sync_area),
            );
        }
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn compact_area_label(area: SyncArea) -> &'static str {
    match area {
        SyncArea::Status => t!("Status", "상태", "状態"),
        SyncArea::PersonalState => t!("Data", "개인", "個人"),
        SyncArea::MusicServer => t!("Server", "서버", "音楽"),
        SyncArea::DevicesRecovery => t!("Dev", "기기", "機器"),
    }
}

/// A block heading: bold title, then a colored dot and the state in the same color.
fn state_heading(settings: &SettingsState, title: &str, state: &str, role: R) -> Line<'static> {
    let theme = &settings.draft.theme;
    Line::from(vec![
        Span::styled(
            title.to_owned(),
            theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ● ", theme.style(role)),
        Span::styled(state.to_owned(), theme.style(role)),
    ])
}

/// Wrap every `\n`-separated line of `text` to `width` cells without dropping any of it. The
/// first source line takes `first`; the rest (the recovery step in attention copy) take `rest`.
fn wrapped(text: &str, width: usize, first: Style, rest: Style) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for (index, source) in text.lines().enumerate() {
        let style = if index == 0 { first } else { rest };
        for part in crate::ui::text::wrap_to_width(source, width) {
            lines.push(Line::from(Span::styled(part, style)));
        }
    }
    lines
}

fn server_health_label(health: MusicServerHealth) -> (&'static str, R) {
    match health {
        MusicServerHealth::Off => (t!("Off", "꺼짐", "オフ"), R::TextMuted),
        MusicServerHealth::UpToDate => (t!("Up to date", "최신 상태", "最新"), R::Success),
        MusicServerHealth::NeedsAttention => {
            (t!("Needs attention", "확인 필요", "要確認"), R::Error)
        }
    }
}

/// The attention message for the most urgent pending server decision, if any.
fn server_attention(summary: &crate::app::MusicServerSummary) -> Option<String> {
    if summary.playlist_creates_needing_decision > 0 {
        Some(playlist_create_attention_detail(
            summary.playlist_creates_needing_decision,
            &summary.playlist_create_attention,
        ))
    } else if summary.playlist_links_needing_decision > 0 {
        Some(playlist_link_attention_detail(
            summary.playlist_links_needing_decision,
        ))
    } else if summary.playlist_contents_needing_decision > 0 {
        Some(playlist_content_attention_detail(
            summary.playlist_contents_needing_decision,
        ))
    } else if summary.playlist_projections_needing_decision > 0 {
        Some(playlist_projection_attention_detail(
            summary.playlist_projections_needing_decision,
        ))
    } else if summary.playback_reports_needing_decision > 0 {
        Some(playback_report_attention_detail(
            summary.playback_reports_needing_decision,
        ))
    } else {
        None
    }
}

pub(crate) fn render_status(frame: &mut Frame, app: &App, settings: &SettingsState, area: Rect) {
    if area.is_empty() {
        return;
    }
    let theme = &settings.draft.theme;
    let personal = app.sync_settings_model();
    let server = &app.server.settings.summary;
    let muted = theme.style(R::TextMuted);
    let width = super::sync::pane_text_width(area);
    let mut lines = vec![state_heading(
        settings,
        t!("Personal state", "개인 상태", "個人データ"),
        health_label(personal.health),
        super::sync::health_role(personal.health),
    )];
    lines.extend(wrapped(
        t!(
            "Encrypted changes stay local when the network is unavailable.",
            "네트워크를 사용할 수 없어도 암호화된 변경 사항은 로컬에 보관돼요.",
            "ネットワークが使えない間も暗号化された変更はローカルに保持されます。"
        ),
        width,
        muted,
        muted,
    ));
    lines.push(Line::default());
    let (state, role) = server_health_label(server.health);
    lines.push(state_heading(
        settings,
        t!("Music server", "음악 서버", "音楽サーバー"),
        state,
        role,
    ));
    match server_attention(server) {
        Some(detail) => lines.extend(wrapped(
            &detail,
            width,
            theme.style(R::Error),
            theme.style(R::SettingsValue),
        )),
        None => lines.extend(wrapped(
            if server.configured {
                t!(
                    "Server browsing is optional; local search and playback stay independent.",
                    "서버 탐색은 선택 사항이며 로컬 검색과 재생은 독립적으로 동작해요.",
                    "サーバー閲覧は任意で、ローカル検索と再生は独立して動作します。"
                )
            } else {
                t!(
                    "No music server is connected.",
                    "연결된 음악 서버가 없어요.",
                    "音楽サーバーは接続されていません。"
                )
            },
            width,
            muted,
            muted,
        )),
    }
    // Text only: the pane scrolls with the wheel and scrollbar when it runs past the bottom.
    super::sync::render_pane(
        frame,
        app,
        settings,
        area,
        lines,
        Vec::new(),
        None,
        MouseTarget::SettingsMusicServerRow,
    );
}

/// One `key  value` row of the server connection summary, the value wrapped under itself.
fn summary_rows(
    settings: &SettingsState,
    rows: &[(&str, String)],
    width: usize,
) -> Vec<Line<'static>> {
    let theme = &settings.draft.theme;
    let key_width = rows
        .iter()
        .map(|(key, _)| usize::from(buttons::text_width(key)))
        .max()
        .unwrap_or(0)
        + 2;
    let value_width = width.saturating_sub(key_width).max(8);
    let mut lines = Vec::new();
    for (key, value) in rows {
        for (index, part) in crate::ui::text::wrap_to_width(value, value_width)
            .into_iter()
            .enumerate()
        {
            let key = if index == 0 { *key } else { "" };
            lines.push(Line::from(vec![
                Span::styled(
                    crate::ui::text::pad_to_width(key, key_width),
                    theme.style(R::SettingsLabel),
                ),
                Span::styled(part, theme.style(R::SettingsValue)),
            ]));
        }
    }
    lines
}

pub(crate) fn render_music_server(
    frame: &mut Frame,
    app: &App,
    settings: &SettingsState,
    area: Rect,
) {
    if area.is_empty() {
        return;
    }
    let theme = &settings.draft.theme;
    let summary = &app.server.settings.summary;
    let (state, role) = if app.server.settings.busy.is_some() {
        (t!("Working…", "처리 중…", "処理中…"), R::Accent)
    } else {
        server_health_label(summary.health)
    };
    let width = super::sync::pane_text_width(area);
    let mut lines = vec![state_heading(settings, summary.display_name(), state, role)];
    if let Some(failure) = app.server.settings.failure {
        // The problem and its recovery step on separate lines, each wrapped whole.
        lines.extend(wrapped(
            &format!("{}\n› {}", failure.label(), failure.recovery_label()),
            width,
            theme.style(R::Error),
            theme.style(R::SettingsValueFocused),
        ));
    } else if let Some(detail) = server_attention(summary) {
        lines.extend(wrapped(
            &detail,
            width,
            theme.style(R::Error),
            theme.style(R::SettingsValue),
        ));
    } else if !summary.configured {
        let muted = theme.style(R::TextMuted);
        lines.extend(wrapped(
            t!(
                "Connect one OpenSubsonic or Navidrome server.",
                "OpenSubsonic 또는 Navidrome 서버 하나를 연결하세요.",
                "OpenSubsonic または Navidrome サーバーを1台接続します。"
            ),
            width,
            muted,
            muted,
        ));
    }
    if summary.configured {
        let rows = [
            (
                t!("Sign-in", "로그인", "ログイン"),
                summary
                    .credential_kind
                    .map(MusicServerCredentialMode::label)
                    .unwrap_or("—")
                    .to_owned(),
            ),
            (
                t!("Connection", "연결", "接続"),
                if summary.lan_http {
                    t!("LAN HTTP allowed", "LAN HTTP 허용", "LAN HTTP 許可")
                } else {
                    "HTTPS"
                }
                .to_owned(),
            ),
            (
                t!("Certificate", "인증서", "証明書"),
                if summary.custom_ca {
                    t!("Custom CA file", "사용자 CA 파일", "カスタムCAファイル")
                } else {
                    t!("System trust", "시스템 신뢰", "システムの信頼")
                }
                .to_owned(),
            ),
            (
                t!("History", "이력", "履歴"),
                history_health_label(summary.history, summary.credential_kind).to_owned(),
            ),
        ];
        lines.push(Line::default());
        lines.extend(summary_rows(settings, &rows, width));
    }

    let labels: Vec<String> = if summary.configured {
        let mut labels = vec![
            t!("Test connection", "연결 테스트", "接続テスト").to_owned(),
            t!("Edit connection", "연결 정보 수정", "接続情報を編集").to_owned(),
            history_action_label(summary.history).to_owned(),
        ];
        if !summary.playlist_create_attention.is_empty() {
            labels.push(
                t!(
                    "Review pending create",
                    "보류 생성 확인",
                    "保留中の作成を確認"
                )
                .to_owned(),
            );
        }
        labels.push(t!("Remove server", "서버 제거", "サーバーを削除").to_owned());
        labels
    } else {
        vec![
            t!(
                "Set up music server",
                "음악 서버 설정",
                "音楽サーバーを設定"
            ),
            t!("Check again", "다시 확인", "再確認"),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    };
    let remove_index = summary.configured.then(|| labels.len() - 1);
    let actions = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let style = if Some(index) == remove_index {
                theme.style(R::Error)
            } else {
                theme.style(R::Accent)
            };
            ListItem::new(Line::from(Span::styled(format!("↵ {label}"), style)))
        })
        .collect();
    super::sync::render_pane(
        frame,
        app,
        settings,
        area,
        lines,
        actions,
        Some(app.server.settings.selected),
        MouseTarget::SettingsMusicServerRow,
    );
}

fn playback_report_attention_detail(count: usize) -> String {
    if count == 1 {
        t!(
            "1 report needs a decision.\nytt server scrobbles list",
            "재생 보고 1건 확인 필요\nytt server scrobbles list",
            "再生レポート1件・確認が必要\nytt server scrobbles list"
        )
        .to_owned()
    } else {
        t!(
            format!("{count} reports need a decision.\nytt server scrobbles list"),
            format!("재생 보고 {count}건 확인 필요\nytt server scrobbles list"),
            format!("再生レポート{count}件・確認が必要\nytt server scrobbles list")
        )
    }
}

fn playlist_create_attention_detail(count: usize, attention: &[PlaylistCreateAttention]) -> String {
    let summary = if count == 1 {
        t!(
            "Review 1 playlist creation",
            "플레이리스트 생성 1건 확인",
            "プレイリスト作成1件を確認"
        )
        .to_owned()
    } else {
        t!(
            format!("Review {count} playlist creations"),
            format!("플레이리스트 생성 {count}건 확인"),
            format!("プレイリスト作成{count}件を確認")
        )
    };
    attention.first().map_or_else(
        || format!("{summary}\nytt server playlists pending"),
        |pending| {
            format!(
                "{summary}\n{}: {}",
                t!("Local ID", "로컬 ID", "ローカルID"),
                pending.local_playlist_id.as_str()
            )
        },
    )
}

fn playlist_projection_attention_detail(count: usize) -> String {
    if count == 1 {
        t!(
            "1 playlist update needs a reconnect\nEdit connection to retry",
            "플레이리스트 업데이트 1건 재연결 필요\n연결 정보를 수정해 다시 시도",
            "プレイリスト更新1件・再接続が必要\n接続を編集して再試行"
        )
        .to_owned()
    } else {
        t!(
            format!("{count} playlist updates need a reconnect\nEdit connection to retry"),
            format!("플레이리스트 업데이트 {count}건 재연결 필요\n연결 정보를 수정해 다시 시도"),
            format!("プレイリスト更新{count}件・再接続が必要\n接続を編集して再試行")
        )
    }
}

fn playlist_content_attention_detail(count: usize) -> String {
    if count == 1 {
        t!(
            "Mixed tracks: 1 linked list\nReview in Server Library",
            "다른 출처 곡: 연결 목록 1개\n서버 보관함에서 확인",
            "別の曲あり：連携リスト1件\nサーバーライブラリで確認"
        )
        .to_owned()
    } else {
        t!(
            format!("Mixed tracks: {count} linked lists\nReview in Server Library"),
            format!("다른 출처 곡: 연결 목록 {count}개\n서버 보관함에서 확인"),
            format!("別の曲あり：連携リスト{count}件\nサーバーライブラリで確認")
        )
    }
}

fn playlist_link_attention_detail(count: usize) -> String {
    if count == 1 {
        t!(
            "1 server playlist is missing\nLibrary: choose what to keep",
            "서버 목록 1개가 사라짐\n보관함: 남길 항목 선택",
            "サーバー側で1件消失\nライブラリ：残すものを選択"
        )
        .to_owned()
    } else {
        t!(
            format!("{count} server playlists are missing\nLibrary: choose what to keep"),
            format!("서버 목록 {count}개가 사라짐\n보관함: 남길 항목 선택"),
            format!("サーバー側で{count}件消失\nライブラリ：残すものを選択")
        )
    }
}

fn history_health_label(
    health: MusicServerHistoryHealth,
    credential: Option<MusicServerCredentialMode>,
) -> &'static str {
    match health {
        MusicServerHistoryHealth::Off => t!("Play counts only", "재생 횟수만", "再生回数のみ"),
        MusicServerHistoryHealth::Probing => t!(
            "Checking detailed history · play counts available",
            "상세 이력 확인 중 · 재생 횟수 사용 가능",
            "詳細履歴を確認中・再生回数は利用可能"
        ),
        MusicServerHistoryHealth::Detailed => t!(
            "Detailed history available (experimental)",
            "상세 이력 사용 가능 (실험적)",
            "詳細履歴を利用可能（実験的）"
        ),
        MusicServerHistoryHealth::PlayCountsOnly => t!(
            "Detailed history unavailable · play counts only",
            "상세 이력 미지원 · 재생 횟수만",
            "詳細履歴は未対応・再生回数のみ"
        ),
        MusicServerHistoryHealth::UpdatePassword
            if credential == Some(MusicServerCredentialMode::Password) =>
        {
            t!(
                "Update via: ytt server setup",
                "다음 명령으로 업데이트: ytt server setup",
                "次のコマンドで更新: ytt server setup"
            )
        }
        MusicServerHistoryHealth::UpdatePassword => t!(
            "Update via: ytt server history enable --experimental",
            "다음 명령으로 업데이트: ytt server history enable --experimental",
            "次のコマンドで更新: ytt server history enable --experimental"
        ),
    }
}

fn history_action_label(health: MusicServerHistoryHealth) -> &'static str {
    match health {
        MusicServerHistoryHealth::Off => t!(
            "Enable detailed history in CLI",
            "CLI에서 상세 이력 켜기",
            "CLIで詳細履歴を有効化"
        ),
        MusicServerHistoryHealth::Probing
        | MusicServerHistoryHealth::Detailed
        | MusicServerHistoryHealth::PlayCountsOnly
        | MusicServerHistoryHealth::UpdatePassword => t!(
            "Turn off detailed history",
            "상세 이력 끄기",
            "詳細履歴をオフ"
        ),
    }
}

pub(crate) fn render_music_server_wizard(frame: &mut Frame, app: &App, area: Rect) {
    let Some(wizard) = app.server.settings.wizard.as_ref() else {
        return;
    };
    let popup = centered(
        area,
        64,
        match wizard {
            MusicServerWizard::Setup(_) => 17,
            MusicServerWizard::AbandonPlaylistCreateConfirm(_) => 13,
            MusicServerWizard::Waiting | MusicServerWizard::RemoveConfirm => 8,
        },
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(t!(" Music server ", " 음악 서버 ", " 音楽サーバー "))
        .borders(Borders::ALL)
        .border_style(app.theme.style(R::Accent))
        .style(app.theme.style(R::TextPrimary));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    match wizard {
        MusicServerWizard::Setup(form) => render_setup_form(frame, app, form, inner),
        MusicServerWizard::Waiting => {
            let (message, cancel_allowed) = match app.server.settings.busy {
                Some(MusicServerBusy::Testing) => (
                    t!(
                        "Testing the connection…",
                        "연결을 테스트하는 중…",
                        "接続をテストしています…"
                    ),
                    true,
                ),
                Some(MusicServerBusy::Saving) => {
                    (t!("Saving…", "저장하는 중…", "保存しています…"), false)
                }
                Some(MusicServerBusy::Removing) => {
                    (t!("Removing…", "제거하는 중…", "削除しています…"), false)
                }
                Some(MusicServerBusy::PlaylistRecovery) => (
                    t!(
                        "Forgetting the pending create…",
                        "보류 중인 생성을 잊는 중…",
                        "保留中の作成を破棄しています…"
                    ),
                    false,
                ),
                _ => (t!("Working…", "처리 중…", "処理中…"), true),
            };
            let detail = if cancel_allowed {
                t!(
                    "Esc or q cancels this screen; a late test result will be ignored.",
                    "Esc 또는 q로 취소할 수 있으며 늦게 도착한 테스트 결과는 무시돼요.",
                    "Esc または q でキャンセルできます。遅れて届いた結果は無視されます。"
                )
            } else {
                t!(
                    "This storage step cannot be cancelled safely.",
                    "이 저장 단계는 안전하게 취소할 수 없어요.",
                    "この保存処理は安全にキャンセルできません。"
                )
            };
            frame.render_widget(
                Paragraph::new(format!("{message}\n\n{detail}"))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
            if cancel_allowed {
                app.register_mouse_button(inner, MouseTarget::MusicServerWizardSecondary);
            }
        }
        MusicServerWizard::RemoveConfirm => {
            frame.render_widget(
                Paragraph::new(t!(
                    "Remove this connection?\nLocal music and personal data will be kept.\n\nEnter: Remove  ·  Esc: Cancel",
                    "이 연결을 제거할까요?\n로컬 음악과 개인 데이터는 그대로 유지돼요.\n\nEnter: 제거  ·  Esc: 취소",
                    "この接続を削除しますか？\nローカル音楽と個人データは保持されます。\n\nEnter: 削除  ·  Esc: キャンセル"
                ))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
                inner,
            );
            let half = inner.width / 2;
            app.register_mouse_button(
                Rect {
                    width: half,
                    ..inner
                },
                MouseTarget::MusicServerWizardPrimary,
            );
            app.register_mouse_button(
                Rect {
                    x: inner.x + half,
                    width: inner.width - half,
                    ..inner
                },
                MouseTarget::MusicServerWizardSecondary,
            );
        }
        MusicServerWizard::AbandonPlaylistCreateConfirm(attention) => {
            let state = playlist_create_recovery_state_label(attention.state);
            let rows = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(inner);
            frame.render_widget(
                Paragraph::new(t!(
                    "A server copy may already exist.",
                    "서버 복사본이 이미 있을 수 있어요.",
                    "サーバーにコピーが既に存在する場合があります。"
                ))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .style(app.theme.style(R::Warning).add_modifier(Modifier::BOLD)),
                rows[0],
            );
            frame.render_widget(
                Paragraph::new(playlist_create_local_id_line(attention, rows[1].width))
                    .alignment(Alignment::Center)
                    .style(app.theme.style(R::TextPrimary)),
                rows[1],
            );
            frame.render_widget(
                Paragraph::new(crate::ui::text::truncate_owned_to_width(
                    format!("{}: {state}", t!("State", "상태", "状態")),
                    usize::from(rows[2].width),
                ))
                .alignment(Alignment::Center)
                .style(app.theme.style(R::TextMuted)),
                rows[2],
            );
            frame.render_widget(
                Paragraph::new(t!(
                    "Forget only the retry guard. Neither copy is deleted.",
                    "재시도 보호만 잊으며 어느 복사본도 삭제하지 않아요.",
                    "再試行ガードだけを破棄し、どちらのコピーも削除しません。"
                ))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .style(app.theme.style(R::TextMuted)),
                rows[3],
            );

            let forget_full = t!(" Enter: Forget ", " Enter: 잊기 ", " Enter: 破棄 ");
            let back_full = t!(" Esc: Back ", " Esc: 뒤로 ", " Esc: 戻る ");
            let full_width = buttons::text_width(forget_full)
                .saturating_add(2)
                .saturating_add(buttons::text_width(back_full));
            let (forget, back) = if full_width <= rows[5].width {
                (forget_full, back_full)
            } else {
                (
                    t!(" Forget ", " 잊기 ", " 破棄 "),
                    t!(" Back ", " 뒤로 ", " 戻る "),
                )
            };
            buttons::render_segments(
                frame,
                app,
                rows[5],
                &[
                    buttons::Seg::button(MouseTarget::MusicServerWizardPrimary, forget),
                    buttons::Seg::label("  "),
                    buttons::Seg::button(MouseTarget::MusicServerWizardSecondary, back),
                ],
                app.theme.style(R::Warning).add_modifier(Modifier::BOLD),
                crate::ui::confirm_gap_style(app),
                Alignment::Center,
            );
        }
    }
}

fn playlist_create_local_id_line(attention: &PlaylistCreateAttention, width: u16) -> String {
    let label = t!("Local ID: ", "로컬 ID: ", "ローカルID: ");
    let available = usize::from(width).saturating_sub(usize::from(buttons::text_width(label)));
    let id = crate::ui::text::middle_to_width(attention.local_playlist_id.as_str(), available);
    crate::ui::text::truncate_owned_to_width(format!("{label}{id}"), usize::from(width))
}

fn playlist_create_recovery_state_label(state: PlaylistCreateRecoveryState) -> &'static str {
    match state {
        PlaylistCreateRecoveryState::ServerIdentityUnknown => {
            t!("server ID unknown", "서버 ID 미확인", "サーバーID不明")
        }
        PlaylistCreateRecoveryState::ReadbackNeeded => {
            t!("readback needed", "재조회 필요", "再取得が必要")
        }
    }
}

fn render_setup_form(
    frame: &mut Frame,
    app: &App,
    form: &crate::app::MusicServerSetupForm,
    area: Rect,
) {
    let fields = MusicServerSetupField::ALL;
    let inputs = fields.len() - 2;
    // Labels share one column when it leaves room for a readable value; otherwise each row
    // falls back to `label: value`.
    let label_width = fields[..inputs]
        .iter()
        .map(|field| usize::from(buttons::text_width(setup_field_label(*field))))
        .max()
        .unwrap_or(0)
        + 2;
    let label_width = (label_width + 2 + 12 <= usize::from(area.width)).then_some(label_width);
    // Inputs, a blank row, the Save/Cancel row, then the two-row key hint. On a short popup
    // the hint goes first, then the blank row; the inputs scroll so the focused one stays
    // visible, and the button row is always drawn.
    let hint_rows: u16 = if area.height >= inputs as u16 + 4 {
        2
    } else {
        0
    };
    let gap: u16 = u16::from(area.height >= inputs as u16 + 2);
    let input_rows = usize::from(area.height.saturating_sub(hint_rows + gap + 1));
    let offset = form
        .selected
        .min(inputs - 1)
        .saturating_add(1)
        .saturating_sub(input_rows);
    for (row, (index, field)) in fields[..inputs]
        .iter()
        .copied()
        .enumerate()
        .skip(offset)
        .take(input_rows)
        .enumerate()
    {
        let selected = form.selected == index;
        let label = setup_field_label(field);
        let rect = Rect {
            x: area.x,
            y: area.y + row as u16,
            width: area.width,
            height: 1,
        };
        let reveal_width =
            u16::from(field == MusicServerSetupField::Secret && rect.width >= 12).saturating_mul(8);
        let field_rect = Rect {
            width: rect.width.saturating_sub(reveal_width),
            ..rect
        };
        let (label_text, value_text, placeholder) = setup_field_text(
            form,
            field,
            label,
            label_width,
            selected,
            field_rect.width as usize,
        );
        let value_style = if selected {
            app.theme
                .style(R::SettingsValueFocused)
                .add_modifier(Modifier::BOLD)
        } else if placeholder {
            app.theme.style(R::TextMuted)
        } else {
            app.theme.style(R::SettingsValue)
        };
        let label_style = if selected {
            value_style
        } else {
            app.theme.style(R::SettingsLabel)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label_text.to_string(), label_style),
                Span::styled(value_text.to_string(), value_style),
            ])),
            field_rect,
        );
        app.register_mouse_button(field_rect, MouseTarget::MusicServerWizardField(index));
        if reveal_width > 0 {
            let reveal_rect = Rect {
                x: field_rect.right(),
                width: reveal_width,
                ..rect
            };
            frame.render_widget(
                Paragraph::new(if form.reveal_secret {
                    t!(" Hide ", " 숨김 ", " 隠す ")
                } else {
                    t!(" Show ", " 보기 ", " 表示 ")
                })
                .alignment(Alignment::Center)
                .style(app.theme.style(R::TextMuted)),
                reveal_rect,
            );
            app.register_mouse_button(reveal_rect, MouseTarget::MusicServerWizardReveal);
        }
    }

    // Save and Cancel sit together on their own row, the focused one highlighted.
    let button_y = area.y + (input_rows.min(inputs) as u16) + gap;
    if button_y < area.bottom() {
        let mut x = area.x + 2;
        for (index, field) in fields.iter().copied().enumerate().skip(inputs) {
            let label = format!(" {} ", setup_field_label(field));
            let width = buttons::text_width(&label).min(area.right().saturating_sub(x));
            if width == 0 {
                break;
            }
            let style = if form.selected == index {
                app.theme
                    .style(R::SettingsValueFocused)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else if field == MusicServerSetupField::SaveAndTest {
                app.theme.style(R::Accent).add_modifier(Modifier::BOLD)
            } else {
                app.theme.style(R::TextMuted)
            };
            let rect = Rect {
                x,
                y: button_y,
                width,
                height: 1,
            };
            frame.render_widget(Paragraph::new(label).style(style), rect);
            app.register_mouse_button(rect, MouseTarget::MusicServerWizardField(index));
            x = x.saturating_add(width).saturating_add(2);
        }
    }
    let hint = t!(
        "↑/↓ fields  ·  Enter reveal/action  ·  Esc cancel",
        "↑/↓ 필드  ·  Enter 표시/실행  ·  Esc 취소",
        "↑/↓ 項目  ·  Enter 表示/実行  ·  Esc キャンセル"
    );
    if hint_rows > 0 {
        frame.render_widget(
            Paragraph::new(hint)
                .style(app.theme.style(R::TextMuted))
                .wrap(Wrap { trim: true }),
            Rect {
                y: area.bottom().saturating_sub(hint_rows),
                height: hint_rows,
                ..area
            },
        );
    }
}

/// The label cell and the value cell of one setup input row, plus whether the value is a
/// placeholder. With a shared `label_width` column the label is padded to it; without one the
/// row reads `label: value`. The row being edited shows a caret window over the raw value.
fn setup_field_text(
    form: &crate::app::MusicServerSetupForm,
    field: MusicServerSetupField,
    label: &str,
    label_width: Option<usize>,
    selected: bool,
    width: usize,
) -> (String, Zeroizing<String>, bool) {
    let marker = if selected { "▶ " } else { "  " };
    let label_cell = |min_value: usize| {
        let cell = match label_width {
            Some(column) => format!("{marker}{}", crate::ui::text::pad_to_width(label, column)),
            None => format!("{marker}{label}: "),
        };
        if usize::from(buttons::text_width(&cell)).saturating_add(min_value) <= width {
            cell
        } else {
            marker.to_owned()
        }
    };
    if selected && let Some(raw) = form.text_value(field) {
        let prefix = label_cell(8);
        let prefix_width = usize::from(buttons::text_width(&prefix));
        let shown = Zeroizing::new(crate::ui::text::editable_value(
            raw,
            form.cursor.byte_index(raw),
            width.saturating_sub(prefix_width),
            '│',
            field == MusicServerSetupField::Secret && !form.reveal_secret,
        ));
        return (prefix, shown, false);
    }

    let value = Zeroizing::new(match field {
        MusicServerSetupField::DisplayName
        | MusicServerSetupField::Origin
        | MusicServerSetupField::Username
        | MusicServerSetupField::CustomCa => form.text_value(field).unwrap_or_default().to_owned(),
        MusicServerSetupField::Secret if form.reveal_secret => {
            form.text_value(field).unwrap_or_default().to_owned()
        }
        MusicServerSetupField::Secret => "•".repeat(
            form.text_value(field)
                .unwrap_or_default()
                .chars()
                .count()
                .min(24),
        ),
        MusicServerSetupField::CredentialMode => form.credential_mode.label().to_owned(),
        MusicServerSetupField::Identity => match form.identity_intent {
            Some(crate::app::MusicServerIdentityIntent::Create) => {
                t!("New connection", "새 연결", "新しい接続").to_owned()
            }
            Some(crate::app::MusicServerIdentityIntent::UpdateSameServerAndAccount) => t!(
                "Same server and account",
                "같은 서버와 계정",
                "同じサーバーとアカウント"
            )
            .to_owned(),
            Some(crate::app::MusicServerIdentityIntent::ReplaceServerOrAccount) => t!(
                "Different server or account",
                "다른 서버 또는 계정",
                "別のサーバーまたはアカウント"
            )
            .to_owned(),
            None => t!("Choose before saving", "저장 전에 선택", "保存前に選択").to_owned(),
        },
        MusicServerSetupField::AllowLanHttp => if form.allow_lan_http {
            t!("Yes", "예", "はい")
        } else {
            t!("No", "아니요", "いいえ")
        }
        .to_owned(),
        MusicServerSetupField::SaveAndTest | MusicServerSetupField::Cancel => String::new(),
    });
    let placeholder = value.is_empty();
    let value = if placeholder {
        Zeroizing::new("—".to_owned())
    } else {
        value
    };
    let prefix = label_cell(1);
    let available = width.saturating_sub(usize::from(buttons::text_width(&prefix)));
    let mut value = value;
    let clipped = Zeroizing::new(crate::ui::text::truncate_owned_to_width(
        std::mem::take(&mut *value),
        available,
    ));
    (prefix, clipped, placeholder)
}

fn setup_field_label(field: MusicServerSetupField) -> &'static str {
    match field {
        MusicServerSetupField::DisplayName => t!("Name", "이름", "名前"),
        MusicServerSetupField::Origin => t!("Server address", "서버 주소", "サーバーアドレス"),
        MusicServerSetupField::Identity => t!("Connection identity", "연결 식별", "接続の識別"),
        MusicServerSetupField::CredentialMode => {
            t!("Sign-in method", "로그인 방식", "ログイン方法")
        }
        MusicServerSetupField::Username => t!("Username", "사용자 이름", "ユーザー名"),
        MusicServerSetupField::Secret => t!(
            "Password / API key",
            "비밀번호 / API 키",
            "パスワード / APIキー"
        ),
        MusicServerSetupField::CustomCa => {
            t!("CA file (optional)", "CA 파일 (선택)", "CAファイル（任意）")
        }
        MusicServerSetupField::AllowLanHttp => t!(
            "Allow exact LAN HTTP",
            "정확한 LAN HTTP 허용",
            "指定LAN HTTPを許可"
        ),
        MusicServerSetupField::SaveAndTest => t!("Save & test", "저장 및 테스트", "保存してテスト"),
        MusicServerSetupField::Cancel => t!("Cancel", "취소", "キャンセル"),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
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

    fn draw_wizard(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_music_server_wizard(frame, app, frame.area()))
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
    fn compact_area_labels_cover_three_languages() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Korean,
            crate::i18n::Language::Japanese,
        ] {
            crate::i18n::set_language(language);
            assert!(
                SyncArea::ALL
                    .iter()
                    .all(|area| !compact_area_label(*area).is_empty())
            );
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn history_health_labels_cover_every_state_and_language() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Korean,
            crate::i18n::Language::Japanese,
        ] {
            crate::i18n::set_language(language);
            for health in [
                MusicServerHistoryHealth::Off,
                MusicServerHistoryHealth::Probing,
                MusicServerHistoryHealth::Detailed,
                MusicServerHistoryHealth::PlayCountsOnly,
                MusicServerHistoryHealth::UpdatePassword,
            ] {
                assert!(
                    !history_health_label(health, Some(MusicServerCredentialMode::ApiKey))
                        .is_empty()
                );
                assert!(!history_action_label(health).is_empty());
            }
            assert!(
                history_health_label(
                    MusicServerHistoryHealth::UpdatePassword,
                    Some(MusicServerCredentialMode::ApiKey),
                )
                .contains("ytt server history enable --experimental")
            );
            assert!(
                history_health_label(
                    MusicServerHistoryHealth::UpdatePassword,
                    Some(MusicServerCredentialMode::Password),
                )
                .contains("ytt server setup")
            );
            let expected = match language {
                crate::i18n::Language::English => ("Password", "API key"),
                crate::i18n::Language::Korean => ("비밀번호", "API 키"),
                crate::i18n::Language::Japanese => ("パスワード", "APIキー"),
            };
            assert_eq!(MusicServerCredentialMode::Password.label(), expected.0);
            assert_eq!(MusicServerCredentialMode::ApiKey.label(), expected.1);
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn playback_report_attention_copy_is_localized_and_points_to_cli_recovery() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, expected) in [
            (
                crate::i18n::Language::English,
                "2 reports need a decision.\nytt server scrobbles list",
            ),
            (
                crate::i18n::Language::Korean,
                "재생 보고 2건 확인 필요\nytt server scrobbles list",
            ),
            (
                crate::i18n::Language::Japanese,
                "再生レポート2件・確認が必要\nytt server scrobbles list",
            ),
        ] {
            crate::i18n::set_language(language);
            let detail = playback_report_attention_detail(2);
            assert_eq!(detail, expected);
            assert!(detail.lines().all(|line| buttons::text_width(line) <= 30));
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn playlist_create_attention_copy_is_localized_and_fits_narrow_layout() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, expected) in [
            (
                crate::i18n::Language::English,
                "Review 2 playlist creations\nLocal ID: local-a",
            ),
            (
                crate::i18n::Language::Korean,
                "플레이리스트 생성 2건 확인\n로컬 ID: local-a",
            ),
            (
                crate::i18n::Language::Japanese,
                "プレイリスト作成2件を確認\nローカルID: local-a",
            ),
        ] {
            crate::i18n::set_language(language);
            let attention = vec![PlaylistCreateAttention {
                local_playlist_id: crate::personal_state::PlaylistId::new("local-a").unwrap(),
                state: PlaylistCreateRecoveryState::ServerIdentityUnknown,
            }];
            let detail = playlist_create_attention_detail(2, &attention);
            assert_eq!(detail, expected);
            assert!(detail.lines().all(|line| buttons::text_width(line) <= 30));
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn missing_playlist_copy_is_localized_and_asks_what_to_keep() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, expected, reconnect_word) in [
            (
                crate::i18n::Language::English,
                "2 server playlists are missing\nLibrary: choose what to keep",
                "reconnect",
            ),
            (
                crate::i18n::Language::Korean,
                "서버 목록 2개가 사라짐\n보관함: 남길 항목 선택",
                "재연결",
            ),
            (
                crate::i18n::Language::Japanese,
                "サーバー側で2件消失\nライブラリ：残すものを選択",
                "再接続",
            ),
        ] {
            crate::i18n::set_language(language);
            let detail = playlist_link_attention_detail(2);
            assert_eq!(detail, expected);
            assert!(!detail.contains(reconnect_word));
            assert!(detail.lines().all(|line| buttons::text_width(line) <= 30));
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn playlist_create_abandon_confirmation_warns_and_exposes_the_local_id() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for (language, warning, action) in [
            (crate::i18n::Language::English, "mayalreadyexist", "Forget"),
            (crate::i18n::Language::Korean, "이미있을수", "잊기"),
            (crate::i18n::Language::Japanese, "既に存在する場合", "破棄"),
        ] {
            crate::i18n::set_language(language);
            let mut app = App::new(50);
            app.server.settings.wizard = Some(MusicServerWizard::AbandonPlaylistCreateConfirm(
                PlaylistCreateAttention {
                    local_playlist_id: crate::personal_state::PlaylistId::new("local-a").unwrap(),
                    state: PlaylistCreateRecoveryState::ServerIdentityUnknown,
                },
            ));
            let text = draw_wizard(&app, 30, 30);
            let comparable: String = text
                .chars()
                .filter(|ch| ch.is_alphanumeric() || *ch == '-')
                .collect();
            assert!(comparable.contains("local-a"), "{language:?}: {text:?}");
            assert!(comparable.contains(warning), "{language:?}: {text:?}");
            assert!(comparable.contains(action), "{language:?}: {text:?}");
            assert!(
                comparable.contains("Enter") && comparable.contains("Esc"),
                "{language:?}: {text:?}"
            );
            let forget = app
                .hits
                .rect_of_target(MouseTarget::MusicServerWizardPrimary)
                .expect("visible Forget button");
            let back = app
                .hits
                .rect_of_target(MouseTarget::MusicServerWizardSecondary)
                .expect("visible Back button");
            assert_eq!(forget.height, 1, "{language:?}: {forget:?}");
            assert_eq!(back.height, 1, "{language:?}: {back:?}");
            assert_eq!(forget.y, back.y, "{language:?}");
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn maximum_local_id_keeps_both_ends_without_hiding_narrow_recovery_controls() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        let local_id = format!("head-{}-tail", "x".repeat(502));
        assert_eq!(local_id.chars().count(), 512);

        for (language, warning, safety, action) in [
            (
                crate::i18n::Language::English,
                "mayalreadyexist",
                "Neithercopyisdeleted",
                "Forget",
            ),
            (
                crate::i18n::Language::Korean,
                "이미있을수",
                "어느복사본도삭제하지않아요",
                "잊기",
            ),
            (
                crate::i18n::Language::Japanese,
                "既に存在する場合",
                "どちらのコピーも削除しません",
                "破棄",
            ),
        ] {
            crate::i18n::set_language(language);
            let mut app = App::new(50);
            app.server.settings.wizard = Some(MusicServerWizard::AbandonPlaylistCreateConfirm(
                PlaylistCreateAttention {
                    local_playlist_id: crate::personal_state::PlaylistId::new(local_id.clone())
                        .unwrap(),
                    state: PlaylistCreateRecoveryState::ReadbackNeeded,
                },
            ));

            let text = draw_wizard(&app, 30, 30);
            let comparable: String = text
                .chars()
                .filter(|character| character.is_alphanumeric() || *character == '-')
                .collect();
            for expected in ["head-", "-tail", warning, safety, action, "Enter", "Esc"] {
                assert!(
                    comparable.contains(expected),
                    "{language:?}, missing {expected:?}: {text:?}"
                );
            }
            assert!(text.contains('…'), "{language:?}: {text:?}");

            let forget = app
                .hits
                .rect_of_target(MouseTarget::MusicServerWizardPrimary)
                .expect("visible Forget button");
            let back = app
                .hits
                .rect_of_target(MouseTarget::MusicServerWizardSecondary)
                .expect("visible Back button");
            assert_eq!(forget.height, 1, "{language:?}: {forget:?}");
            assert_eq!(back.height, 1, "{language:?}: {back:?}");
            assert_eq!(forget.y, back.y, "{language:?}");
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn compact_area_labels_fit_one_inner_row_in_every_language() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Korean,
            crate::i18n::Language::Japanese,
        ] {
            crate::i18n::set_language(language);
            let labels = SyncArea::ALL
                .iter()
                .map(|area| usize::from(buttons::text_width(compact_area_label(*area))))
                .sum::<usize>();
            assert!(labels + 3 * (SyncArea::ALL.len() - 1) <= 28);
        }
        crate::i18n::set_language(original);
    }

    #[test]
    fn removal_progress_does_not_render_a_cancel_action() {
        let mut app = App::new(50);
        app.server.settings.wizard = Some(MusicServerWizard::Waiting);
        app.server.settings.busy = Some(MusicServerBusy::Removing);

        let text = draw_wizard(&app, 80, 24);
        assert!(
            ["Removing", "제거하는 중", "削除しています"]
                .iter()
                .any(|message| text.contains(message))
        );
        assert!(
            !["Cancel", "취소", "キャンセル"]
                .iter()
                .any(|message| text.contains(message))
        );
    }

    #[test]
    fn thirty_column_editor_keeps_origin_and_ca_carets_visible() {
        let _guard = crate::i18n::lock_for_test();
        let original = crate::i18n::current();
        crate::i18n::set_language(crate::i18n::Language::English);

        let mut form = crate::app::MusicServerSetupForm::default();
        form.origin
            .push_str("https://music.example.test:4533/rest/endpoint");
        form.selected = MusicServerSetupField::Origin as usize;
        form.cursor = crate::util::text_edit::TextCursor::at_end(&form.origin);
        let mut app = App::new(50);
        app.server.settings.wizard = Some(MusicServerWizard::Setup(form));
        let origin = draw_wizard(&app, 30, 30);
        assert!(origin.contains("endpoint│"));

        let Some(MusicServerWizard::Setup(form)) = app.server.settings.wizard.as_mut() else {
            panic!("setup form");
        };
        form.custom_ca_path
            .push_str("/private/certificates/custom.pem");
        form.selected = MusicServerSetupField::CustomCa as usize;
        form.cursor = crate::util::text_edit::TextCursor::at_end(&form.custom_ca_path);
        let ca = draw_wizard(&app, 30, 30);
        assert!(ca.contains("custom.pem│"));
        crate::i18n::set_language(original);
    }
}
