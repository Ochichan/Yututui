//! Spotify transfer actor event reduction for the settings surface.

use super::super::*;

fn transfer_done_status(report: &crate::transfer::checkpoint::TransferReport) -> String {
    if crate::i18n::is_korean() {
        format!(
            "가져오기 완료: {} · Library > Playlists에 저장됨 · 검토: Local Deck > Import Sessions 또는 ytt transfer session {}",
            report.render_text(),
            report.job_id
        )
    } else {
        format!(
            "Import finished: {} · saved in Library > Playlists · review: Local Deck > Import Sessions or ytt transfer session {}",
            report.render_text(),
            report.job_id
        )
    }
}

impl App {
    /// Auth/listing/job events from the transfer actor.
    pub(in crate::app) fn on_transfer_event(
        &mut self,
        event: crate::transfer::actor::TransferEvent,
    ) -> Vec<Cmd> {
        use crate::transfer::actor::TransferEvent;
        self.dirty = true;
        match event {
            TransferEvent::AuthUrl(url) => {
                let saved_url_path = crate::spotify::auth::save_pending_auth_url(&url)
                    .ok()
                    .flatten();
                let opened = crate::util::browser::open_in_browser_checked(&url);
                // Also copy the URL: xdg-open can fail silently (e.g. a Flatpak
                // browser the cleared env can't resolve), and this is the only
                // path that would otherwise leave the user no way to reach the
                // approval page.
                let copied = copy_to_clipboard(&url);
                self.status.text =
                    spotify_auth_url_status(opened.launched(), copied, saved_url_path.as_deref());
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::AuthDone { display_name } => {
                let _ = crate::spotify::auth::clear_pending_auth_url();
                let mut used_client_id = None;
                if let Some(st) = self.settings.as_mut() {
                    st.draft.spotify_connected = true;
                    st.draft.spotify_stale = false;
                    st.draft.spotify_username = display_name.clone();
                    let cid = st.draft.spotify_client_id.trim().to_owned();
                    if !cid.is_empty() {
                        used_client_id = Some(cid);
                    }
                }
                self.status.text = if crate::i18n::is_korean() {
                    format!("Spotify 연결됨: {display_name}")
                } else {
                    format!("Spotify connected as {display_name}")
                };
                self.status.kind = StatusKind::Info;
                // Repair config if it had lost or mismatched the Client ID (recovered
                // from the token for this reconnect), so the orphaned "needs reconnect"
                // state doesn't come back on the next launch.
                if let Some(cid) = used_client_id
                    && self.config.spotify.client_id.as_deref() != Some(cid.as_str())
                {
                    self.config.spotify.client_id = Some(cid);
                    return vec![Cmd::Persist(PersistCmd::Config(Box::new(
                        self.config.clone(),
                    )))];
                }
            }
            TransferEvent::AuthError(error) => {
                let _ = crate::spotify::auth::clear_pending_auth_url();
                self.status.text = format!(
                    "{}: {}",
                    t!("Spotify authorization failed", "Spotify 인증 실패"),
                    crate::util::sanitize::sanitize_error_text(error)
                );
                self.status.kind = StatusKind::Error;
            }
            TransferEvent::Disconnected => {
                if let Some(st) = self.settings.as_mut() {
                    st.draft.spotify_connected = false;
                    st.draft.spotify_stale = false;
                    st.draft.spotify_username.clear();
                }
                self.status.text =
                    t!("Spotify disconnected", "Spotify 연결을 해제했어요").to_owned();
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::SpotifyPlaylists(Ok(items)) => {
                if items.is_empty() {
                    self.status.text =
                        t!("No Spotify playlists", "Spotify 플레이리스트 없음").to_owned();
                    self.status.kind = StatusKind::Info;
                } else {
                    self.status.text.clear();
                    self.overlays.spotify_picker =
                        Some(crate::app::state::SpotifyPicker { items, selected: 0 });
                }
            }
            TransferEvent::SpotifyPlaylists(Err(error)) => {
                self.status.text = format!(
                    "{}: {}",
                    t!(
                        "Could not list Spotify playlists",
                        "Spotify 플레이리스트 조회 실패"
                    ),
                    crate::util::sanitize::sanitize_error_text(error)
                );
                self.status.kind = StatusKind::Error;
            }
            TransferEvent::Progress(p) => {
                self.transfer_running = true;
                self.status.text = if crate::i18n::is_korean() {
                    format!(
                        "Spotify 가져오기: {} {}/{} · 맞춤 {} · 자동 {} · 검토 {} · 누락 {} · 작성 {} · {}",
                        p.stage.label(),
                        p.done,
                        p.total,
                        p.matched,
                        p.auto_accepted,
                        p.ambiguous,
                        p.not_found,
                        p.written,
                        p.current
                    )
                } else {
                    format!(
                        "Spotify import: {} {}/{} · matched {} · auto {} · review {} · missing {} · written {} · {}",
                        p.stage.label(),
                        p.done,
                        p.total,
                        p.matched,
                        p.auto_accepted,
                        p.ambiguous,
                        p.not_found,
                        p.written,
                        p.current
                    )
                };
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::JobDone(report) => {
                self.transfer_running = false;
                // A local-dest job wrote playlists.json from the actor; reload so the
                // Library shows it now and a later in-app save can't clobber it. (The
                // app persists its own mutations immediately, so disk is the union — which
                // also means a just-deleted playlist reappears if the job re-created it.)
                self.playlists
                    .replace_reloaded(crate::playlists::Playlists::load());
                // The store changed under the Playlists tab: drop a drill-down or pending
                // delete whose playlist vanished and re-clamp the cursor into the new rows.
                self.reconcile_playlists_reload();
                self.status.text = transfer_done_status(&report);
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::JobRejected { error, .. } => {
                // The actor still owns a different active job, so its running guard remains set
                // until that job emits JobDone/JobFailed.
                let error = crate::util::sanitize::sanitize_error_text(error);
                self.status.text = format!(
                    "{}: {error}",
                    t!("Import request rejected", "가져오기 요청 거부")
                );
                self.status.kind = StatusKind::Error;
            }
            TransferEvent::JobFailed {
                job_id,
                error,
                resumable,
            } => {
                self.transfer_running = false;
                let error = crate::util::sanitize::sanitize_error_text(error);
                self.status.text = if resumable && !job_id.is_empty() {
                    format!(
                        "{}: {error} · ytt transfer resume {job_id}",
                        t!("Import interrupted", "가져오기 중단")
                    )
                } else {
                    format!("{}: {error}", t!("Import failed", "가져오기 실패"))
                };
                self.status.kind = StatusKind::Error;
            }
        }
        Vec::new()
    }
}
