//! Plain-language Sync area selector and music-server settings/wizard rendering.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
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
    let mut x = area.x;
    let mut spans = Vec::new();
    for (index, sync_area) in SyncArea::ALL.iter().copied().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", theme.style(R::TextMuted)));
            x = x.saturating_add(3);
        }
        let label = compact_area_label(sync_area);
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

pub(crate) fn render_status(frame: &mut Frame, app: &App, settings: &SettingsState, area: Rect) {
    let theme = &settings.draft.theme;
    let personal = app.sync_settings_model();
    let server = &app.server.settings.summary;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                t!("Personal state", "개인 상태", "個人データ"),
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", theme.style(R::TextMuted)),
            Span::styled(health_label(personal.health), theme.style(R::SettingsValue)),
        ])),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(t!(
            "Encrypted changes stay local when the network is unavailable.",
            "네트워크를 사용할 수 없어도 암호화된 변경 사항은 로컬에 보관돼요.",
            "ネットワークが使えない間も暗号化された変更はローカルに保持されます。"
        ))
        .style(theme.style(R::TextMuted))
        .wrap(Wrap { trim: true }),
        rows[1],
    );
    let server_health = match server.health {
        MusicServerHealth::Off => t!("Off", "꺼짐", "オフ"),
        MusicServerHealth::UpToDate => t!("Up to date", "최신 상태", "最新"),
        MusicServerHealth::NeedsAttention => {
            t!("Needs attention", "확인 필요", "要確認")
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                t!("Music server", "음악 서버", "音楽サーバー"),
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", theme.style(R::TextMuted)),
            Span::styled(server_health, theme.style(R::SettingsValue)),
        ])),
        rows[2],
    );
    let server_detail = if server.playlist_creates_needing_decision > 0 {
        playlist_create_attention_detail(
            server.playlist_creates_needing_decision,
            &server.playlist_create_attention,
        )
    } else if server.playlist_links_needing_decision > 0 {
        playlist_link_attention_detail(server.playlist_links_needing_decision)
    } else if server.playlist_contents_needing_decision > 0 {
        playlist_content_attention_detail(server.playlist_contents_needing_decision)
    } else if server.playlist_projections_needing_decision > 0 {
        playlist_projection_attention_detail(server.playlist_projections_needing_decision)
    } else if server.playback_reports_needing_decision > 0 {
        playback_report_attention_detail(server.playback_reports_needing_decision)
    } else if server.configured {
        t!(
            "Server browsing is optional; local search and playback stay independent.",
            "서버 탐색은 선택 사항이며 로컬 검색과 재생은 독립적으로 동작해요.",
            "サーバー閲覧は任意で、ローカル検索と再生は独立して動作します。"
        )
        .to_owned()
    } else {
        t!(
            "No music server is connected.",
            "연결된 음악 서버가 없어요.",
            "音楽サーバーは接続されていません。"
        )
        .to_owned()
    };
    frame.render_widget(
        Paragraph::new(server_detail)
            .style(theme.style(
                if server.playback_reports_needing_decision > 0
                    || server.playlist_creates_needing_decision > 0
                    || server.playlist_links_needing_decision > 0
                    || server.playlist_contents_needing_decision > 0
                    || server.playlist_projections_needing_decision > 0
                {
                    R::Error
                } else {
                    R::TextMuted
                },
            ))
            .wrap(Wrap { trim: true }),
        rows[3],
    );
}

pub(crate) fn render_music_server(
    frame: &mut Frame,
    app: &App,
    settings: &SettingsState,
    area: Rect,
) {
    let theme = &settings.draft.theme;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(area);
    let summary = &app.server.settings.summary;
    let state = if app.server.settings.busy.is_some() {
        t!("Working…", "처리 중…", "処理中…")
    } else {
        match summary.health {
            MusicServerHealth::Off => t!("Off", "꺼짐", "オフ"),
            MusicServerHealth::UpToDate => t!("Up to date", "최신 상태", "最新"),
            MusicServerHealth::NeedsAttention => {
                t!("Needs attention", "확인 필요", "要確認")
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                summary.display_name(),
                theme.style(R::SettingsGroup).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", theme.style(R::TextMuted)),
            Span::styled(state, theme.style(R::SettingsValue)),
        ])),
        rows[0],
    );
    let detail = app.server.settings.failure.map_or_else(
        || {
            if summary.playlist_creates_needing_decision > 0 {
                playlist_create_attention_detail(
                    summary.playlist_creates_needing_decision,
                    &summary.playlist_create_attention,
                )
            } else if summary.playlist_links_needing_decision > 0 {
                playlist_link_attention_detail(summary.playlist_links_needing_decision)
            } else if summary.playlist_contents_needing_decision > 0 {
                playlist_content_attention_detail(summary.playlist_contents_needing_decision)
            } else if summary.playlist_projections_needing_decision > 0 {
                playlist_projection_attention_detail(summary.playlist_projections_needing_decision)
            } else if summary.playback_reports_needing_decision > 0 {
                playback_report_attention_detail(summary.playback_reports_needing_decision)
            } else if summary.configured {
                let auth = summary
                    .credential_kind
                    .map(MusicServerCredentialMode::label)
                    .unwrap_or("—");
                format!(
                    "{}  ·  {}  ·  {}",
                    auth,
                    if summary.lan_http {
                        t!("LAN HTTP allowed", "LAN HTTP 허용", "LAN HTTP 許可")
                    } else {
                        "HTTPS"
                    },
                    history_health_label(summary.history, summary.credential_kind),
                )
            } else {
                t!(
                    "Connect one OpenSubsonic or Navidrome server.",
                    "OpenSubsonic 또는 Navidrome 서버 하나를 연결하세요.",
                    "OpenSubsonic または Navidrome サーバーを1台接続します。"
                )
                .to_owned()
            }
        },
        |failure| format!("{}  ·  {}", failure.label(), failure.recovery_label()),
    );
    let detail_is_error = app.server.settings.failure.is_some()
        || summary.playback_reports_needing_decision > 0
        || summary.playlist_creates_needing_decision > 0
        || summary.playlist_links_needing_decision > 0
        || summary.playlist_contents_needing_decision > 0
        || summary.playlist_projections_needing_decision > 0;
    frame.render_widget(
        Paragraph::new(detail)
            .style(theme.style(if detail_is_error {
                R::Error
            } else {
                R::TextMuted
            }))
            .wrap(Wrap { trim: true }),
        rows[1],
    );

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
    for (index, label) in labels.iter().enumerate().take(rows[2].height as usize) {
        let selected = index == app.server.settings.selected;
        let rect = Rect {
            x: rows[2].x,
            y: rows[2].y + index as u16,
            width: rows[2].width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(format!("{}↵ {label}", if selected { "▶ " } else { "  " })).style(
                if selected {
                    theme
                        .style(R::SettingsValueFocused)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme.style(R::SettingsValue)
                },
            ),
            rect,
        );
        app.register_mouse_button(rect, MouseTarget::SettingsMusicServerRow(index));
    }
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
            MusicServerWizard::Setup(_) => 16,
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
    for (index, field) in fields.iter().copied().enumerate() {
        if index >= area.height.saturating_sub(2) as usize {
            break;
        }
        let selected = form.selected == index;
        let label = setup_field_label(field);
        let rect = Rect {
            x: area.x,
            y: area.y + index as u16,
            width: area.width,
            height: 1,
        };
        let reveal_width =
            u16::from(field == MusicServerSetupField::Secret && rect.width >= 12).saturating_mul(8);
        let field_rect = Rect {
            width: rect.width.saturating_sub(reveal_width),
            ..rect
        };
        let text = setup_field_text(form, field, label, selected, field_rect.width as usize);
        frame.render_widget(
            Paragraph::new(text.as_str()).style(if selected {
                app.theme
                    .style(R::SettingsValueFocused)
                    .add_modifier(Modifier::BOLD)
            } else {
                app.theme.style(R::TextPrimary)
            }),
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
    let hint = t!(
        "↑/↓ fields  ·  Enter reveal/action  ·  Esc cancel",
        "↑/↓ 필드  ·  Enter 표시/실행  ·  Esc 취소",
        "↑/↓ 項目  ·  Enter 表示/実行  ·  Esc キャンセル"
    );
    frame.render_widget(
        Paragraph::new(hint)
            .style(app.theme.style(R::TextMuted))
            .wrap(Wrap { trim: true }),
        Rect {
            y: area.bottom().saturating_sub(2),
            height: 2,
            ..area
        },
    );
}

fn setup_field_text(
    form: &crate::app::MusicServerSetupForm,
    field: MusicServerSetupField,
    label: &str,
    selected: bool,
    width: usize,
) -> Zeroizing<String> {
    if selected && let Some(raw) = form.text_value(field) {
        let full_prefix = format!("▶ {label}: ");
        let full_prefix_width = usize::from(buttons::text_width(&full_prefix));
        let prefix = if full_prefix_width.saturating_add(8) <= width {
            full_prefix
        } else {
            "▶ ".to_owned()
        };
        let prefix_width = usize::from(buttons::text_width(&prefix));
        let shown = Zeroizing::new(crate::ui::text::editable_value(
            raw,
            form.cursor.byte_index(raw),
            width.saturating_sub(prefix_width),
            '│',
            field == MusicServerSetupField::Secret && !form.reveal_secret,
        ));
        let mut text = Zeroizing::new(format!("{prefix}{}", shown.as_str()));
        return Zeroizing::new(crate::ui::text::truncate_owned_to_width(
            std::mem::take(&mut *text),
            width,
        ));
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
    let mut text = Zeroizing::new(if value.is_empty() {
        format!("{}{}", if selected { "▶ " } else { "  " }, label)
    } else {
        format!(
            "{}{}: {}",
            if selected { "▶ " } else { "  " },
            label,
            value.as_str()
        )
    });
    Zeroizing::new(crate::ui::text::truncate_owned_to_width(
        std::mem::take(&mut *text),
        width,
    ))
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
