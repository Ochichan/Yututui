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
    fn plan_transfer_playlist_commit(
        &mut self,
        request: crate::transfer::actor::LocalPlaylistRequest,
        patch: crate::transfer::local_playlist::LocalPlaylistPatch,
        restore_current_on_error: bool,
    ) -> Vec<Cmd> {
        let owner_base_revision = self.playlists.revision();
        match crate::transfer::local_playlist::plan_apply_local_playlist_patch(
            &self.playlists,
            &patch,
        ) {
            Ok(plan) => vec![Cmd::Persist(PersistCmd::TransferPlaylistCommit(Box::new(
                crate::app::TransferPlaylistCommit {
                    request,
                    owner_base_revision,
                    candidate: plan.candidate,
                    kind: crate::app::TransferPlaylistCommitKind::Apply {
                        patch,
                        outcome: plan.outcome,
                    },
                },
            )))],
            Err(error) => {
                if restore_current_on_error {
                    self.restore_transfer_playlist_then_fail(request, error, 0)
                } else {
                    request.respond(Err(error));
                    Vec::new()
                }
            }
        }
    }

    fn restore_transfer_playlist_then_fail(
        &self,
        request: crate::transfer::actor::LocalPlaylistRequest,
        error: crate::transfer::local_playlist::LocalPlaylistStoreError,
        retry_attempt: u8,
    ) -> Vec<Cmd> {
        vec![Cmd::Persist(PersistCmd::TransferPlaylistCommit(Box::new(
            crate::app::TransferPlaylistCommit {
                request,
                owner_base_revision: self.playlists.revision(),
                // Detach from the shared Arc: the commit protocol owns its candidate outright.
                candidate: (*self.playlists).clone(),
                kind: crate::app::TransferPlaylistCommitKind::RestoreThenFail {
                    error,
                    retry_attempt,
                },
            },
        )))]
    }

    pub(in crate::app) fn on_transfer_playlist_persisted(
        &mut self,
        commit: crate::app::TransferPlaylistCommit,
        persistence: crate::persist::TargetFlushOutcome,
    ) -> Vec<Cmd> {
        use crate::persist::TargetFlushOutcome;
        let crate::app::TransferPlaylistCommit {
            request,
            owner_base_revision,
            candidate,
            kind,
        } = commit;
        match kind {
            crate::app::TransferPlaylistCommitKind::Apply { patch, outcome } => {
                match persistence {
                    TargetFlushOutcome::Unconfirmed => self.restore_transfer_playlist_then_fail(
                        request,
                        crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(
                            "playlist owner could not confirm the targeted durable commit",
                        ),
                        0,
                    ),
                    TargetFlushOutcome::Superseded => {
                        // A newer same-store generation won, but it is not proof that the
                        // transfer patch is present. Rebase; if the destination can no longer be
                        // planned, reassert the live owner snapshot before releasing the waiter.
                        self.plan_transfer_playlist_commit(request, patch, true)
                    }
                    TargetFlushOutcome::CommittedExact
                        if self.playlists.revision() != owner_base_revision =>
                    {
                        // The exact candidate reached disk, but installing it now would clobber
                        // newer owner changes. Rebase and confirm a new exact generation.
                        self.plan_transfer_playlist_commit(request, patch, true)
                    }
                    TargetFlushOutcome::CommittedExact => {
                        self.playlists = std::sync::Arc::new(candidate);
                        self.reconcile_playlists_reload();
                        self.dirty = true;
                        request.respond(Ok(
                            crate::transfer::local_playlist::LocalPlaylistOwnerReply::Applied(
                                outcome,
                            ),
                        ));
                        Vec::new()
                    }
                }
            }
            crate::app::TransferPlaylistCommitKind::RestoreThenFail {
                error,
                retry_attempt,
            } => {
                if persistence == TargetFlushOutcome::CommittedExact
                    && self.playlists.revision() == owner_base_revision
                {
                    request.respond(Err(error));
                    Vec::new()
                } else {
                    // The old candidate or failed target may still become durable later. Keep a
                    // higher-order exact restore pending until the current live snapshot wins.
                    self.restore_transfer_playlist_then_fail(
                        request,
                        error,
                        retry_attempt.saturating_add(1),
                    )
                }
            }
        }
    }

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
            TransferEvent::LocalPlaylistRequest(request) => {
                return match request.request().clone() {
                    crate::transfer::local_playlist::LocalPlaylistOwnerRequest::Snapshot => {
                        request.respond(Ok(
                            crate::transfer::local_playlist::LocalPlaylistOwnerReply::Snapshot(
                                (*self.playlists).clone(),
                            ),
                        ));
                        Vec::new()
                    }
                    crate::transfer::local_playlist::LocalPlaylistOwnerRequest::Apply(patch) => {
                        self.plan_transfer_playlist_commit(request, patch, false)
                    }
                };
            }
            TransferEvent::JobDone(report) => {
                self.transfer_running = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::TargetFlushOutcome;
    use crate::playlists::AddResult;
    use crate::transfer::local_playlist::{
        LocalPlaylistOwnerReply, LocalPlaylistOwnerRequest, LocalPlaylistPatch,
        LocalPlaylistPatchRow,
    };
    use tokio::sync::oneshot::error::TryRecvError;

    fn patch(observed_revision: u64) -> LocalPlaylistPatch {
        LocalPlaylistPatch {
            observed_revision,
            destination_id: None,
            destination_name: "Transfer".to_owned(),
            rows: vec![LocalPlaylistPatchRow {
                checkpoint_index: 4,
                song: crate::api::Song::remote("transfer-row", "Transfer Row", "Artist", "3:00"),
            }],
        }
    }

    fn begin(
        app: &mut App,
        correlation_id: u64,
    ) -> (
        Box<crate::app::TransferPlaylistCommit>,
        tokio::sync::oneshot::Receiver<
            Result<
                crate::transfer::local_playlist::LocalPlaylistOwnerReply,
                crate::transfer::local_playlist::LocalPlaylistStoreError,
            >,
        >,
    ) {
        begin_patch(app, correlation_id, patch(app.playlists.revision()))
    }

    fn begin_patch(
        app: &mut App,
        correlation_id: u64,
        patch: LocalPlaylistPatch,
    ) -> (
        Box<crate::app::TransferPlaylistCommit>,
        tokio::sync::oneshot::Receiver<
            Result<
                crate::transfer::local_playlist::LocalPlaylistOwnerReply,
                crate::transfer::local_playlist::LocalPlaylistStoreError,
            >,
        >,
    ) {
        let (request, reply) = crate::transfer::actor::LocalPlaylistRequest::for_test(
            correlation_id,
            LocalPlaylistOwnerRequest::Apply(patch),
        );
        let commands = app.on_transfer_event(
            crate::transfer::actor::TransferEvent::LocalPlaylistRequest(request),
        );
        (only_commit(commands), reply)
    }

    fn only_commit(commands: Vec<Cmd>) -> Box<crate::app::TransferPlaylistCommit> {
        let mut commands = commands.into_iter();
        let commit = match commands.next().expect("one transfer commit") {
            Cmd::Persist(PersistCmd::TransferPlaylistCommit(commit)) => commit,
            _ => panic!("expected transfer playlist commit"),
        };
        assert!(commands.next().is_none());
        commit
    }

    fn assert_pending(
        reply: &mut tokio::sync::oneshot::Receiver<
            Result<
                crate::transfer::local_playlist::LocalPlaylistOwnerReply,
                crate::transfer::local_playlist::LocalPlaylistStoreError,
            >,
        >,
    ) {
        assert!(matches!(reply.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn apply_keeps_live_store_unchanged_until_exact_same_revision_then_replies() {
        let mut app = App::new(50);
        let (commit, mut reply) = begin(&mut app, 1);

        assert!(app.playlists.find("Transfer").is_none());
        assert_pending(&mut reply);

        let commands =
            app.on_transfer_playlist_persisted(*commit, TargetFlushOutcome::CommittedExact);
        assert!(commands.is_empty());
        let playlist = app.playlists.find("Transfer").expect("candidate installed");
        assert_eq!(playlist.songs.len(), 1);
        let response = reply.try_recv().expect("durable reply").expect("success");
        let LocalPlaylistOwnerReply::Applied(outcome) = response else {
            panic!("expected apply reply");
        };
        assert_eq!(outcome.rows[0].checkpoint_index, 4);
        assert_eq!(outcome.rows[0].result, AddResult::Added);
    }

    #[test]
    fn exact_revision_race_rebases_on_latest_live_store_before_installing() {
        let mut app = App::new(50);
        let (stale_commit, mut reply) = begin(&mut app, 2);
        let destination = app
            .playlists_mut()
            .create("Transfer")
            .expect("owner playlist");
        assert_eq!(
            app.playlists_mut().add(
                &destination,
                crate::api::Song::remote("owner-row", "Owner Row", "Artist", "3:00"),
            ),
            AddResult::Added
        );

        let rebased = only_commit(
            app.on_transfer_playlist_persisted(*stale_commit, TargetFlushOutcome::CommittedExact),
        );
        assert_pending(&mut reply);
        let live = app
            .playlists
            .find(&destination)
            .expect("live owner playlist");
        assert_eq!(live.songs.len(), 1, "rebased candidate is not live yet");

        assert!(
            app.on_transfer_playlist_persisted(*rebased, TargetFlushOutcome::CommittedExact)
                .is_empty()
        );
        let installed = app
            .playlists
            .find(&destination)
            .expect("rebased candidate installed");
        assert_eq!(installed.songs.len(), 2);
        assert!(
            installed
                .songs
                .iter()
                .any(|song| song.video_id == "owner-row")
        );
        assert!(
            installed
                .songs
                .iter()
                .any(|song| song.video_id == "transfer-row")
        );
        assert!(matches!(
            reply.try_recv().expect("durable reply"),
            Ok(LocalPlaylistOwnerReply::Applied(_))
        ));
    }

    #[test]
    fn superseded_target_replans_against_latest_live_store_without_replying() {
        let mut app = App::new(50);
        let (stale_commit, mut reply) = begin(&mut app, 3);
        let destination = app
            .playlists_mut()
            .create("Transfer")
            .expect("owner playlist");
        assert_eq!(
            app.playlists_mut().add(
                &destination,
                crate::api::Song::remote("owner-row", "Owner Row", "Artist", "3:00"),
            ),
            AddResult::Added
        );

        let replanned = only_commit(
            app.on_transfer_playlist_persisted(*stale_commit, TargetFlushOutcome::Superseded),
        );
        assert_pending(&mut reply);
        assert_eq!(app.playlists.find(&destination).unwrap().songs.len(), 1);

        assert!(
            app.on_transfer_playlist_persisted(*replanned, TargetFlushOutcome::CommittedExact)
                .is_empty()
        );
        assert_eq!(app.playlists.find(&destination).unwrap().songs.len(), 2);
        assert!(matches!(
            reply.try_recv().expect("durable reply"),
            Ok(LocalPlaylistOwnerReply::Applied(_))
        ));
    }

    #[test]
    fn unconfirmed_target_waits_for_an_exact_live_restore_before_error_reply() {
        let mut app = App::new(50);
        let (commit, mut reply) = begin(&mut app, 4);

        let restore = only_commit(
            app.on_transfer_playlist_persisted(*commit, TargetFlushOutcome::Unconfirmed),
        );
        assert!(matches!(
            &restore.kind,
            crate::app::TransferPlaylistCommitKind::RestoreThenFail {
                retry_attempt: 0,
                ..
            }
        ));
        assert!(app.playlists.find("Transfer").is_none());
        assert_pending(&mut reply);

        let retry_restore = only_commit(
            app.on_transfer_playlist_persisted(*restore, TargetFlushOutcome::Superseded),
        );
        assert!(matches!(
            &retry_restore.kind,
            crate::app::TransferPlaylistCommitKind::RestoreThenFail {
                retry_attempt: 1,
                ..
            }
        ));
        assert_pending(&mut reply);
        assert!(
            app.on_transfer_playlist_persisted(*retry_restore, TargetFlushOutcome::CommittedExact,)
                .is_empty()
        );
        let error = reply
            .try_recv()
            .expect("error only after exact restore")
            .expect_err("unconfirmed apply remains resumable");
        assert!(error.is_resumable());
        assert!(app.playlists.find("Transfer").is_none());
    }

    #[test]
    fn rebase_errors_restore_live_snapshot_before_replying() {
        for persistence in [
            TargetFlushOutcome::CommittedExact,
            TargetFlushOutcome::Superseded,
        ] {
            let mut app = App::new(50);
            let destination = app
                .playlists_mut()
                .create("Ephemeral Destination")
                .expect("owner destination");
            let owner_revision = app.playlists.revision();
            let (stale_commit, mut reply) = begin_patch(
                &mut app,
                5,
                LocalPlaylistPatch {
                    observed_revision: owner_revision,
                    destination_id: Some(destination.clone()),
                    destination_name: String::new(),
                    rows: vec![LocalPlaylistPatchRow {
                        checkpoint_index: 4,
                        song: crate::api::Song::remote(
                            "transfer-row",
                            "Transfer Row",
                            "Artist",
                            "3:00",
                        ),
                    }],
                },
            );
            app.playlists_mut()
                .delete(&destination)
                .expect("concurrent owner delete");

            let restore =
                only_commit(app.on_transfer_playlist_persisted(*stale_commit, persistence));
            assert!(matches!(
                restore.kind,
                crate::app::TransferPlaylistCommitKind::RestoreThenFail { .. }
            ));
            assert_pending(&mut reply);

            assert!(
                app.on_transfer_playlist_persisted(*restore, TargetFlushOutcome::CommittedExact)
                    .is_empty()
            );
            let error = reply
                .try_recv()
                .expect("reply after restore")
                .expect_err("deleted destination cannot be rebased");
            assert!(!error.is_resumable());
            assert!(app.playlists.find(&destination).is_none());
        }
    }
}
