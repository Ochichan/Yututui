use crate::app::{Msg, PlayerMsg, StreamingMsg};
use crate::util::event_policy::{EventKey as Key, EventLane as Lane, EventPolicy};

pub(super) fn app_msg_policy(msg: &Msg) -> EventPolicy {
    match msg {
        Msg::Quit | Msg::Media(_) => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
        Msg::Remote(_, _) => EventPolicy::MustReplyOrBusy {
            lane: Lane::RemoteCommand,
        },
        Msg::Data(_) => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
        Msg::Player(player) => player_msg_policy(player),
        Msg::ArtworkResized(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::ArtResize,
        },
        Msg::Download(
            crate::app::DownloadMsg::Progress { .. }
            | crate::app::DownloadMsg::ImportProgress { .. },
        ) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::DownloadProgress,
        },
        Msg::MediaArtworkReady(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::MediaArtVideo,
        },
        Msg::Local(crate::app::LocalMsg::ScanProgress(_)) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::LocalScanProgress,
        },
        Msg::Tools(crate::tools::ToolsEvent::Progress { .. }) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::ToolProgress,
        },
        Msg::UpdateChecked(_) => EventPolicy::CoalesceLatest {
            lane: Lane::WorkResult,
            key: Key::UpdateCheck,
        },
        Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(_)) => {
            EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::TransferJob,
            }
        }
        Msg::SearchResults { .. } | Msg::SearchError { .. } => EventPolicy::DropIfStale {
            stale_key: Key::SearchRequest,
        },
        Msg::ArtworkResult { .. } => EventPolicy::DropIfStale {
            stale_key: Key::ArtworkVideo,
        },
        Msg::LyricsResult { .. } => EventPolicy::DropIfStale {
            stale_key: Key::LyricsVideo,
        },
        Msg::Streaming(StreamingMsg::Resolved {
            self_heal: true, ..
        })
        | Msg::ResolveFailed { .. } => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
        Msg::Streaming(_) => EventPolicy::DropIfStale {
            stale_key: Key::StreamingSeed,
        },
        Msg::TrackResolved { .. } => EventPolicy::DropIfStale {
            stale_key: Key::ResolverVideo,
        },
        Msg::Noop | Msg::StatusTick | Msg::AnimTick | Msg::RecordingTick => {
            EventPolicy::BestEffort {
                reason: "loop-owned ticks and inert messages are redraw/status hints",
            }
        }
        Msg::Key(_)
        | Msg::MouseClick { .. }
        | Msg::MouseDoubleClick { .. }
        | Msg::MouseRightClick { .. }
        | Msg::MouseRightDoubleClick { .. }
        | Msg::MouseDrag { .. }
        | Msg::MouseLeftUp
        | Msg::MouseScroll { .. }
        | Msg::Resize
        | Msg::Focus(_)
        | Msg::Autoplay
        | Msg::ApiModeResolved { .. }
        | Msg::Recorder(_)
        | Msg::PlaylistTracks { .. }
        | Msg::PlaylistTracksError { .. }
        | Msg::DownloadsDeleted { .. }
        | Msg::Local(_)
        | Msg::Download(
            crate::app::DownloadMsg::Done { .. }
            | crate::app::DownloadMsg::ImportDone { .. }
            | crate::app::DownloadMsg::Error { .. }
            | crate::app::DownloadMsg::ImportError { .. }
            | crate::app::DownloadMsg::Rejected { .. }
            | crate::app::DownloadMsg::DirError { .. },
        )
        | Msg::PersistFailed { .. }
        | Msg::Ai(_)
        | Msg::Scrobble(_)
        | Msg::Tools(
            crate::tools::ToolsEvent::Installed { .. } | crate::tools::ToolsEvent::Failed { .. },
        )
        | Msg::YtdlpHealResult { .. }
        | Msg::Transfer(
            crate::transfer::actor::TransferEvent::AuthUrl(_)
            | crate::transfer::actor::TransferEvent::AuthDone { .. }
            | crate::transfer::actor::TransferEvent::AuthError(_)
            | crate::transfer::actor::TransferEvent::Disconnected
            | crate::transfer::actor::TransferEvent::SpotifyPlaylists(_)
            | crate::transfer::actor::TransferEvent::JobDone(_)
            | crate::transfer::actor::TransferEvent::JobRejected { .. }
            | crate::transfer::actor::TransferEvent::JobFailed { .. },
        ) => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
    }
}

pub(super) fn player_msg_policy(msg: &PlayerMsg) -> EventPolicy {
    match msg {
        PlayerMsg::TimePos(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerTimePos,
        },
        PlayerMsg::Duration(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerDuration,
        },
        PlayerMsg::Paused(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerPaused,
        },
        PlayerMsg::Volume(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerVolume,
        },
        PlayerMsg::Metadata(_) => EventPolicy::CoalesceLatest {
            lane: Lane::WorkResult,
            key: Key::PlayerMetadata,
        },
        PlayerMsg::CacheTime(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerCacheTime,
        },
        PlayerMsg::AudioCodec(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerAudioCodec,
        },
        PlayerMsg::FileFormat(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerFileFormat,
        },
        PlayerMsg::Eof
        | PlayerMsg::Error(_)
        | PlayerMsg::TransportClosed(_)
        | PlayerMsg::CacheEmergency { .. }
        | PlayerMsg::CacheReplacementEmergency { .. }
        | PlayerMsg::IntentAdmitted(_) => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
        PlayerMsg::VideoOverlay { event, .. } => video_event_policy(event),
    }
}

pub(super) fn video_event_policy(event: &crate::player::video::VideoEvent) -> EventPolicy {
    match event {
        crate::player::video::VideoEvent::Paused(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::VideoOverlayPaused,
        },
        // EOF and connection terminal events independently drive ownership cleanup.
        crate::player::video::VideoEvent::Failed(_)
        | crate::player::video::VideoEvent::Eof
        | crate::player::video::VideoEvent::Quit
        | crate::player::video::VideoEvent::Closed
        | crate::player::video::VideoEvent::Next
        | crate::player::video::VideoEvent::Prev
        | crate::player::video::VideoEvent::TogglePause
        | crate::player::video::VideoEvent::Close
        | crate::player::video::VideoEvent::ToggleFullscreen
        | crate::player::video::VideoEvent::ToggleMute => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
    }
}
