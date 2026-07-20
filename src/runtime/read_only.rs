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
        Cmd::PlayerControl(_)
        | Cmd::VideoConnect { .. }
        | Cmd::VideoLoad(_)
        | Cmd::VideoTogglePause
        | Cmd::VideoToggleFullscreen
        | Cmd::VideoToggleMute
        | Cmd::Search { .. }
        | Cmd::SearchPlaylists { .. }
        | Cmd::SearchArtists { .. }
        | Cmd::FetchPlaylistTracks { .. }
        | Cmd::FetchArtist { .. }
        | Cmd::Data(DataCmd::ScanDownloads(_) | DataCmd::PersonalDataExport(_))
        | Cmd::Download(DownloadCmd::Scan(_))
        | Cmd::FetchLyrics { .. }
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
