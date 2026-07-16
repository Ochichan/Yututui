//! Scrobble actor event handling.

use super::*;

impl App {
    /// Auth-flow progress and service-health notices from the scrobble actor.
    pub(in crate::app) fn on_scrobble_event(
        &mut self,
        event: crate::scrobble::ScrobbleEvent,
    ) -> Vec<Cmd> {
        use crate::scrobble::ScrobbleEvent;
        self.dirty = true;
        match event {
            ScrobbleEvent::AuthUrl(url) => {
                let opened = crate::util::browser::open_in_browser_checked(&url);
                // Also copy the URL: xdg-open can fail silently (e.g. a Flatpak
                // browser the cleared env can't resolve), and this is the only
                // path that would otherwise leave the user no way to reach the
                // approval page.
                copy_to_clipboard(&url);
                self.status.text = if opened.launched() {
                    t!(
                        "Approve YuTuTui! in the browser (link copied as fallback)",
                        "브라우저에서 YuTuTui!를 승인해 주세요 (링크는 예비용으로 복사했어요)",
                        "ブラウザでYuTuTui!を承認してください (リンクは予備としてコピー済み)"
                    )
                    .to_owned()
                } else {
                    t!(
                        "Could not open browser; link copied. Paste it manually or run `ytt doctor --verbose`.",
                        "브라우저를 열 수 없어요. 링크를 복사했으니 직접 붙여넣거나 `ytt doctor --verbose`를 실행해 주세요.",
                        "ブラウザを開けません。コピーしたリンクを手動で貼り付けるか `ytt doctor --verbose` を実行してください。"
                    )
                    .to_owned()
                };
                self.status.kind = StatusKind::Info;
                Vec::new()
            }
            ScrobbleEvent::AuthDone {
                username,
                session_key,
            } => {
                self.config.scrobble.lastfm.session_key = Some(session_key.clone());
                self.config.scrobble.lastfm.username = Some(username.clone());
                // Mirror into the open draft too, or closing settings would clobber the
                // fresh session with the stale pre-connect values.
                if let Some(st) = self.settings.as_mut() {
                    st.draft.lastfm_session_key = session_key;
                    st.draft.lastfm_username = username.clone();
                }
                self.status.text = match crate::i18n::current() {
                    crate::i18n::Language::Korean => format!("Last.fm 연결됨: {username}"),
                    crate::i18n::Language::Japanese => format!("Last.fm 接続済み: {username}"),
                    _ => format!("Last.fm connected as {username}"),
                };
                self.status.kind = StatusKind::Info;
                vec![
                    Cmd::Persist(PersistCmd::Config(Box::new(self.config.clone()))),
                    Cmd::Scrobble(ScrobbleCmd::Reconfigure(Box::new(
                        self.config.scrobble_settings(),
                    ))),
                ]
            }
            ScrobbleEvent::AuthFailed(error) => {
                let error = crate::util::sanitize::sanitize_error_text(error);
                self.status.text = format!(
                    "{}: {error}",
                    t!(
                        "Last.fm authorization failed",
                        "Last.fm 인증 실패",
                        "Last.fm 認証に失敗"
                    )
                );
                self.status.kind = StatusKind::Error;
                Vec::new()
            }
            ScrobbleEvent::SessionInvalid(kind) => {
                self.status.text = match crate::i18n::current() {
                    crate::i18n::Language::Korean => format!(
                        "{} 세션이 만료되었어요 — 설정 › 계정에서 다시 연결해 주세요",
                        kind.label()
                    ),
                    crate::i18n::Language::Japanese => format!(
                        "{} セッションの期限が切れました — 設定 › アカウントから再接続してください",
                        kind.label()
                    ),
                    _ => format!(
                        "{} session expired — reconnect in Settings › Accounts",
                        kind.label()
                    ),
                };
                self.status.kind = StatusKind::Error;
                Vec::new()
            }
            ScrobbleEvent::QueueStalled { pending } => {
                self.status.text = if pending == 0 {
                    match crate::i18n::current() {
                        crate::i18n::Language::Korean => {
                            "스크로블 저장소가 복구되어 대기 중인 항목을 저장했어요".to_owned()
                        }
                        crate::i18n::Language::Japanese => {
                            "スクロブルの保存領域が復旧し、保留分を保存しました".to_owned()
                        }
                        _ => "Scrobble storage recovered; retained listens were saved".to_owned(),
                    }
                } else {
                    match crate::i18n::current() {
                        crate::i18n::Language::Korean => {
                            format!("스크로블 {pending}건이 전송 대기 중이에요")
                        }
                        crate::i18n::Language::Japanese => {
                            format!("スクロブル{pending}件が送信待ちです")
                        }
                        _ => format!("{pending} scrobbles waiting to be delivered"),
                    }
                };
                self.status.kind = StatusKind::Info;
                Vec::new()
            }
            ScrobbleEvent::QueueDropped { dropped } => {
                self.status.text = match crate::i18n::current() {
                    crate::i18n::Language::Korean => {
                        format!("오프라인 스크로블 큐가 가득 차 {dropped}건을 삭제했어요")
                    }
                    crate::i18n::Language::Japanese => {
                        format!(
                            "オフラインのスクロブルキューが満杯のため古い{dropped}件を削除しました"
                        )
                    }
                    _ => {
                        format!(
                            "Offline scrobble queue was full; dropped {dropped} oldest scrobbles"
                        )
                    }
                };
                self.status.kind = StatusKind::Error;
                Vec::new()
            }
        }
    }
}
