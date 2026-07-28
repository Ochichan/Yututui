use crate::app::{AiMsg, App, Cmd, DataCmd, DownloadCmd, Msg};

pub(super) fn durable_mutation_component(cmd: &Cmd) -> Option<&'static str> {
    match cmd {
        Cmd::Recorder(_) => Some("recorder"),
        Cmd::UpdateSeen { .. } => Some("update state"),
        Cmd::Persist(crate::app::PersistCmd::TransferPlaylistCommit(commit))
            if matches!(
                &commit.kind,
                crate::app::TransferPlaylistCommitKind::RestoreThenFail { .. }
            ) =>
        {
            // A previously accepted candidate may already own disk. Let the restore coordinator
            // retain its waiter until persistence recovers or shutdown's final snapshot wins.
            None
        }
        Cmd::Persist(crate::app::PersistCmd::TransferPlaylistCommit(_)) => Some("transfer state"),
        Cmd::Persist(_) => Some("persistence"),
        Cmd::Local(crate::app::LocalCmd::LoadIndex { .. }) => None,
        Cmd::Local(_) => Some("local import/index"),
        Cmd::FetchArtwork { .. } => Some("artwork cache"),
        Cmd::Download(
            DownloadCmd::Start(_) | DownloadCmd::SetDir(_) | DownloadCmd::Delete { .. },
        ) => Some("downloads"),
        Cmd::YtdlpSelfHeal { .. } => Some("managed yt-dlp"),
        Cmd::SummarizeFeedback { .. }
        | Cmd::RomanizeTitles { .. }
        | Cmd::AskAi { .. }
        | Cmd::AiRerank { .. } => Some("AI usage"),
        Cmd::Scrobble(_) => Some("scrobble state"),
        Cmd::Transfer(_) => Some("transfer state"),
        Cmd::Data(DataCmd::PersonalSync { .. }) => Some("personal sync"),
        Cmd::Data(DataCmd::SyncUi(command)) if !command.is_read_only() => Some("personal sync"),
        Cmd::MusicServer(
            crate::app::MusicServerCommand::TestAndPrepare { .. }
            | crate::app::MusicServerCommand::Commit { .. }
            | crate::app::MusicServerCommand::DisableHistory { .. }
            | crate::app::MusicServerCommand::Remove { .. }
            | crate::app::MusicServerCommand::AbandonPlaylistCreate { .. }
            | crate::app::MusicServerCommand::PublishTrack { .. },
        ) => Some("music server settings"),
        Cmd::ServerLibrary(
            crate::app::ServerLibraryCommand::ApplyPlaylistPreview { .. }
            | crate::app::ServerLibraryCommand::CreateLinkedPlaylist { .. }
            | crate::app::ServerLibraryCommand::RecoverPlaylist { .. },
        ) => Some("music server playlist"),
        Cmd::PlayerControl(_)
        | Cmd::VideoConnect { .. }
        | Cmd::VideoLoad(_)
        | Cmd::VideoTogglePause
        | Cmd::VideoToggleFullscreen
        | Cmd::VideoToggleMute
        | Cmd::Search(_)
        | Cmd::MusicServer(crate::app::MusicServerCommand::Refresh { .. })
        | Cmd::ServerLibrary(
            crate::app::ServerLibraryCommand::LoadPage { .. }
            | crate::app::ServerLibraryCommand::LoadDetail { .. }
            | crate::app::ServerLibraryCommand::PreparePlaylist { .. },
        )
        | Cmd::Data(
            DataCmd::ScanDownloads(_) | DataCmd::PersonalDataExport(_) | DataCmd::SyncUi(_),
        )
        | Cmd::Download(DownloadCmd::Scan(_))
        | Cmd::FetchLyrics(_)
        | Cmd::Resolve { .. }
        | Cmd::ResolveForSelfHeal { .. }
        | Cmd::DesktopNotify { .. }
        | Cmd::ResolveTrack { .. }
        | Cmd::StreamingFallback { .. }
        | Cmd::StreamingPreflight { .. }
        | Cmd::SetAiModel(_)
        | Cmd::ReloadAi { .. } => None,
    }
}

pub(super) fn reject_mutation(app: &mut App, cmd: &Cmd, component: &str, reason: &str) -> Vec<Cmd> {
    tracing::warn!(component, %reason, "durable mutation rejected in read-only process");
    let follow_ups = match cmd {
        Cmd::FetchArtwork { .. } => {
            app.art.loading = false;
            Vec::new()
        }
        Cmd::Download(DownloadCmd::Start(song)) => {
            app.update(Msg::Download(crate::app::DownloadMsg::Rejected {
                tracking_key: crate::download::download_tracking_key(song),
                error: "read-only secondary cannot write downloads".to_owned(),
            }))
        }
        Cmd::RomanizeTitles {
            request_id, items, ..
        } => {
            let keys: Vec<_> = items.iter().map(|item| item.key.clone()).collect();
            app.update(Msg::Ai(AiMsg::RomanizedTitles {
                request_id: *request_id,
                keys,
                entries: Vec::new(),
            }))
        }
        Cmd::AskAi { .. } | Cmd::AiRerank { .. } => {
            app.ai.thinking = false;
            Vec::new()
        }
        Cmd::SummarizeFeedback { .. } => {
            app.streaming.feedback_in_flight = false;
            Vec::new()
        }
        Cmd::Transfer(crate::transfer::actor::TransferCmd::StartJob(_))
        | Cmd::Transfer(crate::transfer::actor::TransferCmd::WriteReviewedLocal { .. })
        | Cmd::Transfer(crate::transfer::actor::TransferCmd::CancelJob) => {
            app.transfer_running = false;
            Vec::new()
        }
        Cmd::Persist(crate::app::PersistCmd::TransferPlaylistCommit(commit)) => {
            commit.request.respond(Err(
                crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(format!(
                    "read-only owner rejected playlist commit: {reason}"
                )),
            ));
            Vec::new()
        }
        Cmd::Persist(crate::app::PersistCmd::PersonalSyncCommit(commit)) => {
            app.personal_state.sync.in_progress = false;
            app.personal_state.sync.pending_reply = None;
            commit
                .reply
                .respond(crate::remote::proto::RemoteResponse::err_with_message(
                    "read_only_secondary",
                    "the read-only secondary cannot change personal sync state".to_owned(),
                ));
            Vec::new()
        }
        Cmd::Persist(crate::app::PersistCmd::SyncActivationCommit(commit)) => {
            app.personal_state.sync.in_progress = false;
            if app.personal_state.sync_ui.is_current(commit.flow_id) {
                app.personal_state.sync_ui.busy = None;
                app.queue_sync_ui_refresh();
            }
            app.start_pending_sync_ui_refresh()
        }
        Cmd::Data(DataCmd::PersonalSync { reply, .. }) => {
            app.personal_state.sync.in_progress = false;
            app.personal_state.sync.pending_reply = None;
            reply.respond(crate::remote::proto::RemoteResponse::err_with_message(
                "read_only_secondary",
                "the read-only secondary cannot change personal sync state".to_owned(),
            ));
            Vec::new()
        }
        Cmd::Data(DataCmd::SyncUi(command)) => {
            if app.personal_state.sync_ui.is_current(command.flow_id()) {
                app.personal_state.sync_ui.busy = None;
                app.queue_sync_ui_refresh();
            }
            app.start_pending_sync_ui_refresh()
        }
        Cmd::MusicServer(_) => {
            app.server.settings.busy = None;
            app.server.settings.failure = Some(crate::app::MusicServerFailure::Storage);
            app.dirty = true;
            Vec::new()
        }
        Cmd::ServerLibrary(crate::app::ServerLibraryCommand::ApplyPlaylistPreview {
            generation,
            ..
        }) => app.update(Msg::Server(crate::app::ServerEvent::Library(
            crate::app::ServerLibraryEvent::PlaylistApplied {
                generation: *generation,
                result: Err(crate::app::ServerLibraryFailure::Unavailable),
            },
        ))),
        Cmd::ServerLibrary(crate::app::ServerLibraryCommand::CreateLinkedPlaylist {
            generation,
            snapshot,
        }) => app.update(Msg::Server(crate::app::ServerEvent::Library(
            crate::app::ServerLibraryEvent::PlaylistCreated {
                generation: *generation,
                local_playlist_id: snapshot.playlist_id.clone(),
                result: Err(crate::app::ServerLibraryFailure::Unavailable),
            },
        ))),
        Cmd::ServerLibrary(crate::app::ServerLibraryCommand::RecoverPlaylist {
            generation,
            action,
            ..
        }) => app.update(Msg::Server(crate::app::ServerEvent::Library(
            crate::app::ServerLibraryEvent::PlaylistRecovered {
                generation: *generation,
                action: *action,
                result: Err(crate::app::ServerLibraryFailure::Unavailable),
            },
        ))),
        _ => Vec::new(),
    };
    app.set_status_error(match crate::i18n::current() {
        crate::i18n::Language::Korean => {
            format!("읽기 전용 보조 인스턴스: {component} 변경 거부 — {reason}")
        }
        crate::i18n::Language::Japanese => {
            format!("読み取り専用セカンダリ: {component} の変更を拒否 — {reason}")
        }
        _ => format!("Read-only secondary: {component} change rejected — {reason}"),
    });
    follow_ups
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn read_only_secondary_settles_manual_sync_reply_without_worker_start() {
        let mut app = App::new(50);
        app.personal_state.sync.in_progress = true;
        let (reply, mut response) = oneshot::channel();
        let command = Cmd::Data(DataCmd::PersonalSync {
            action: crate::app::PersonalSyncAction::SyncNow,
            attempt: 1,
            personal_state: Box::new(crate::personal_state::PersonalStateV2::default()),
            revision_guard: crate::sync::OwnerRevisionGuard::default(),
            reply: crate::app::PersonalSyncReply::new(reply.into()),
        });

        assert_eq!(durable_mutation_component(&command), Some("personal sync"));
        assert!(reject_mutation(&mut app, &command, "personal sync", "test").is_empty());
        assert_eq!(
            response.try_recv().unwrap().reason.as_deref(),
            Some("read_only_secondary")
        );
        assert!(!app.personal_state.sync.in_progress);
    }

    #[test]
    fn publishing_a_track_is_a_durable_mutation_a_secondary_must_refuse() {
        // A read-only secondary writing into the music folder would do it behind the primary's
        // back, with no lease serialising the two.
        let command = Cmd::MusicServer(crate::app::MusicServerCommand::PublishTrack {
            generation: 1,
            video_id: "abcdefghijk".to_owned(),
        });

        assert_eq!(
            durable_mutation_component(&command),
            Some("music server settings")
        );
    }

    #[test]
    fn read_only_secondary_rejects_secret_bearing_server_setup_before_network_work() {
        let setup = Cmd::MusicServer(crate::app::MusicServerCommand::TestAndPrepare {
            generation: 1,
            input: crate::app::MusicServerSetupInput {
                display_name: zeroize::Zeroizing::new("Server".to_owned()),
                origin: zeroize::Zeroizing::new("https://music.example.test".to_owned()),
                username: zeroize::Zeroizing::new(String::new()),
                secret: zeroize::Zeroizing::new("secret".to_owned()),
                credential_mode: crate::app::MusicServerCredentialMode::ApiKey,
                custom_ca_path: zeroize::Zeroizing::new(String::new()),
                allow_lan_http: false,
                identity_intent: crate::app::MusicServerIdentityIntent::Create,
            },
        });
        let refresh = Cmd::MusicServer(crate::app::MusicServerCommand::Refresh { generation: 2 });

        assert_eq!(
            durable_mutation_component(&setup),
            Some("music server settings")
        );
        assert_eq!(durable_mutation_component(&refresh), None);
    }

    #[test]
    fn read_only_secondary_settles_linked_playlist_create_without_actor_work() {
        let snapshot = crate::personal_state::PersonalPlaylistSnapshot {
            playlist_id: crate::personal_state::PlaylistId::new("local").unwrap(),
            name: "Local".to_owned(),
            entries: Vec::new(),
        };
        let mut app = App::new(50);
        app.server.library.playlist_create = Some(crate::app::ServerPlaylistCreateModal {
            generation: 7,
            snapshot: snapshot.clone(),
            stage: crate::app::ServerPlaylistCreateStage::Applying,
        });
        let command = Cmd::ServerLibrary(crate::app::ServerLibraryCommand::CreateLinkedPlaylist {
            generation: 7,
            snapshot,
        });

        assert_eq!(
            durable_mutation_component(&command),
            Some("music server playlist")
        );
        assert!(
            reject_mutation(
                &mut app,
                &command,
                "music server playlist",
                "test writer lease"
            )
            .is_empty()
        );
        assert!(app.server.library.playlist_create.is_none());
        assert_eq!(app.status.kind, crate::app::StatusKind::Error);
    }
}
