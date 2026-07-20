//! Delivery policies shared by the standalone TUI and daemon owner loops.
//!
//! Only event domains with identical semantics belong here. AI streaming picks and lyrics
//! intentionally have owner-specific stale handling, so their policies remain beside each
//! owner's event taxonomy.

use crate::util::event_policy::{EventKey as Key, EventLane as Lane, EventPolicy};

pub(crate) fn remote_event_policy(event: &crate::remote::server::RemoteEvent) -> EventPolicy {
    match event {
        crate::remote::server::RemoteEvent::Command(_, _)
        | crate::remote::server::RemoteEvent::SessionCommand { .. }
        | crate::remote::server::RemoteEvent::SessionSubscribe { .. } => {
            EventPolicy::MustReplyOrBusy {
                lane: Lane::RemoteCommand,
            }
        }
    }
}

pub(crate) fn player_event_policy(event: &crate::player::PlayerEvent) -> EventPolicy {
    match event {
        crate::player::PlayerEvent::TimePos(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerTimePos,
        },
        crate::player::PlayerEvent::Duration(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerDuration,
        },
        crate::player::PlayerEvent::Paused(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerPaused,
        },
        crate::player::PlayerEvent::Volume(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerVolume,
        },
        crate::player::PlayerEvent::Metadata(_) => EventPolicy::CoalesceLatest {
            lane: Lane::WorkResult,
            key: Key::PlayerMetadata,
        },
        crate::player::PlayerEvent::CacheTime(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerCacheTime,
        },
        crate::player::PlayerEvent::AudioCodec(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerAudioCodec,
        },
        crate::player::PlayerEvent::FileFormat(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerFileFormat,
        },
        crate::player::PlayerEvent::AudioDeviceList(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerAudioDeviceList,
        },
        crate::player::PlayerEvent::AudioDeviceRefreshFailed(_)
        | crate::player::PlayerEvent::AudioDeviceSelectionResult { .. }
        | crate::player::PlayerEvent::Eof
        | crate::player::PlayerEvent::Error(_)
        | crate::player::PlayerEvent::TransportClosed(_)
        | crate::player::PlayerEvent::CacheEmergency { .. }
        | crate::player::PlayerEvent::CacheReplacementEmergency { .. } => {
            EventPolicy::MustDeliver {
                lane: Lane::Control,
            }
        }
        crate::player::PlayerEvent::AudioDeviceChanged(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerAudioDevice,
        },
        crate::player::PlayerEvent::CurrentAudioOutput(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerCurrentAudioOutput,
        },
        crate::player::PlayerEvent::FileScoped { .. } => {
            unreachable!("audio file event was unscoped before policy lookup")
        }
    }
}

pub(crate) fn api_event_policy(event: &crate::api::ApiEvent) -> EventPolicy {
    match event {
        crate::api::ApiEvent::ModeResolved { .. }
        | crate::api::ApiEvent::TrackResolved { .. }
        | crate::api::ApiEvent::PlaylistTracks { .. }
        | crate::api::ApiEvent::PlaylistTracksError { .. }
        | crate::api::ApiEvent::ArtistPage { .. }
        | crate::api::ApiEvent::ArtistPageError { .. }
        | crate::api::ApiEvent::GuiSearchCompleted { .. } => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
        crate::api::ApiEvent::SearchResults { .. } | crate::api::ApiEvent::SearchError { .. } => {
            EventPolicy::DropIfStale {
                stale_key: Key::SearchRequest,
            }
        }
        crate::api::ApiEvent::StreamingResults { .. }
        | crate::api::ApiEvent::StreamingPreflighted { .. }
        | crate::api::ApiEvent::StreamingError { .. } => EventPolicy::DropIfStale {
            stale_key: Key::StreamingSeed,
        },
    }
}

pub(crate) fn download_event_policy(event: &crate::download::DownloadEvent) -> EventPolicy {
    match event {
        crate::download::DownloadEvent::Progress { .. }
        | crate::download::DownloadEvent::ImportProgress { .. } => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::DownloadProgress,
        },
        crate::download::DownloadEvent::Done { .. }
        | crate::download::DownloadEvent::ImportDone { .. }
        | crate::download::DownloadEvent::Error { .. }
        | crate::download::DownloadEvent::ImportError { .. } => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
    }
}

pub(crate) fn scrobble_event_policy(event: &crate::scrobble::ScrobbleEvent) -> EventPolicy {
    match event {
        crate::scrobble::ScrobbleEvent::AuthUrl(_)
        | crate::scrobble::ScrobbleEvent::AuthDone { .. }
        | crate::scrobble::ScrobbleEvent::AuthFailed(_)
        | crate::scrobble::ScrobbleEvent::SessionInvalid(_)
        | crate::scrobble::ScrobbleEvent::QueueDropped { .. } => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
        crate::scrobble::ScrobbleEvent::QueueStalled { .. } => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::ScrobbleQueueStalled,
        },
    }
}

pub(crate) fn transfer_event_policy(event: &crate::transfer::actor::TransferEvent) -> EventPolicy {
    match event {
        crate::transfer::actor::TransferEvent::Progress(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::TransferJob,
        },
        crate::transfer::actor::TransferEvent::AuthUrl(_)
        | crate::transfer::actor::TransferEvent::AuthDone { .. }
        | crate::transfer::actor::TransferEvent::AuthError(_)
        | crate::transfer::actor::TransferEvent::Disconnected
        | crate::transfer::actor::TransferEvent::SpotifyPlaylists(_)
        | crate::transfer::actor::TransferEvent::LocalPlaylistRequest(_)
        | crate::transfer::actor::TransferEvent::JobDone(_)
        | crate::transfer::actor::TransferEvent::JobRejected { .. }
        | crate::transfer::actor::TransferEvent::JobFailed { .. } => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_leaf_policies_cover_each_lane_and_stale_class() {
        assert_eq!(
            player_event_policy(&crate::player::PlayerEvent::TimePos(1.0)),
            EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::PlayerTimePos,
            }
        );
        assert_eq!(
            player_event_policy(&crate::player::PlayerEvent::Eof),
            EventPolicy::MustDeliver {
                lane: Lane::Control,
            }
        );
        assert_eq!(
            api_event_policy(&crate::api::ApiEvent::SearchError {
                request_id: 1,
                source: crate::search_source::SearchSource::Youtube,
                error: "offline".to_owned(),
            }),
            EventPolicy::DropIfStale {
                stale_key: Key::SearchRequest,
            }
        );
        assert_eq!(
            download_event_policy(&crate::download::DownloadEvent::Done {
                video_id: "v".to_owned(),
                path: "v.m4a".to_owned(),
            }),
            EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            }
        );
        assert_eq!(
            scrobble_event_policy(&crate::scrobble::ScrobbleEvent::QueueStalled { pending: 2 }),
            EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::ScrobbleQueueStalled,
            }
        );
        assert_eq!(
            transfer_event_policy(&crate::transfer::actor::TransferEvent::Disconnected),
            EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            }
        );
        let (request, _reply) = crate::transfer::actor::LocalPlaylistRequest::for_test(
            1,
            crate::transfer::local_playlist::LocalPlaylistOwnerRequest::Snapshot,
        );
        assert_eq!(
            transfer_event_policy(
                &crate::transfer::actor::TransferEvent::LocalPlaylistRequest(request),
            ),
            EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            }
        );
    }

    #[test]
    fn remote_requests_keep_the_reserved_reply_lane() {
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let event = crate::remote::server::RemoteEvent::Command(
            crate::remote::proto::RemoteCommand::TogglePause,
            reply.into(),
        );
        assert_eq!(
            remote_event_policy(&event),
            EventPolicy::MustReplyOrBusy {
                lane: Lane::RemoteCommand,
            }
        );
    }
}
