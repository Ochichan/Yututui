use crate::app::{AiMsg, App, Cmd, DataCmd, DownloadCmd, Msg};

pub(super) fn durable_mutation_component(cmd: &Cmd) -> Option<&'static str> {
    match cmd {
        Cmd::Recorder(_) => Some("recorder"),
        Cmd::UpdateSeen { .. } => Some("update state"),
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
        | Cmd::FetchPlaylistTracks { .. }
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
        _ => Vec::new(),
    };
    app.set_status_error(if crate::i18n::is_korean() {
        format!("읽기 전용 보조 인스턴스: {component} 변경 거부 — {reason}")
    } else {
        format!("Read-only secondary: {component} change rejected — {reason}")
    });
    follow_ups
}
