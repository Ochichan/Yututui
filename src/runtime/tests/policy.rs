use super::*;

fn assert_policy(event: RuntimeEvent, expected: EventPolicy) {
    assert_eq!(event.policy(), expected);
}

#[test]
fn runtime_event_policy_covers_representative_events() {
    assert_eq!(
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::Eof).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control
        }
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::TransportClosed(
            "EOF".to_owned()
        ))
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control
        }
    );
    assert_eq!(
        RuntimeEvent::App(Msg::Player(PlayerMsg::TransportClosed("EOF".to_owned()))).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control
        }
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(12.0)).policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerTimePos
        }
    );

    let (reply, _rx) = tokio::sync::oneshot::channel();
    assert_eq!(
        RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(
            RemoteCommand::TogglePause,
            reply.into(),
        ))
        .policy(),
        EventPolicy::MustReplyOrBusy {
            lane: EventLane::RemoteCommand
        }
    );

    assert_eq!(
        RuntimeEvent::App(Msg::Data(crate::app::DataMsg::PersonalDataExport(
            crate::app::PersonalDataExportMsg::Finished {
                result: Err("test export failure".to_owned()),
                reply: None,
            },
        )))
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );

    assert_eq!(
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id: "v".to_owned(),
            percent: 50.0,
        })
        .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::DownloadProgress
        }
    );
    assert_eq!(
        RuntimeEvent::Download(crate::download::DownloadEvent::Done {
            video_id: "v".to_owned(),
            path: "song.m4a".to_owned(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
    assert_eq!(
        RuntimeEvent::Api(crate::api::ApiEvent::StreamingError {
            seed_video_id: "seed".to_owned(),
            error: "nope".to_owned(),
        })
        .policy(),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed
        }
    );
    assert!(matches!(
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueStalled { pending: 1 })
            .policy(),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::ScrobbleQueueStalled,
        }
    ));
    assert_eq!(
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthDone {
            username: "listener".to_owned(),
            session_key: "secret".to_owned(),
        })
        .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 7 })
            .policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    for event in [
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthUrl(
            "https://example.invalid/auth".to_owned(),
        )),
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::AuthFailed(
            "denied".to_owned(),
        )),
        RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::SessionInvalid(
            crate::scrobble::service::ServiceKind::Lastfm,
        )),
    ] {
        assert_eq!(
            event.policy(),
            EventPolicy::MustDeliver {
                lane: EventLane::Control,
            }
        );
    }
}

#[test]
fn runtime_event_policy_covers_leaf_event_classes() {
    use crate::api::{ApiEvent, ApiMode, PlaylistIntent};
    use crate::search_source::SearchSource;

    assert_policy(
        RuntimeEvent::Ai(crate::ai::AiEvent::Thinking(true)),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::AiThinking,
        },
    );
    assert_policy(
        RuntimeEvent::Ai(crate::ai::AiEvent::StreamingPicks {
            seed_video_id: "seed".to_owned(),
            picks: Vec::new(),
            conf: None,
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed,
        },
    );
    assert_policy(
        RuntimeEvent::Ai(crate::ai::AiEvent::Chat("ok".to_owned())),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );

    assert_policy(
        RuntimeEvent::Api(ApiEvent::ModeResolved {
            mode: ApiMode::Anonymous,
            had_cookie: false,
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::Api(ApiEvent::SearchResults {
            request_id: 7,
            query: "q".to_owned(),
            source: SearchSource::Youtube,
            songs: Vec::new(),
            timed_out: false,
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::SearchRequest,
        },
    );
    assert_policy(
        RuntimeEvent::Api(ApiEvent::PlaylistTracks {
            title: "mix".to_owned(),
            intent: PlaylistIntent::Play,
            songs: Vec::new(),
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::Api(ApiEvent::GuiSearchCompleted {
            request_id: crate::api::GuiSearchRequestId::new(0, 9),
            groups: Vec::new(),
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );

    assert_policy(
        RuntimeEvent::Artwork(crate::artwork::ArtworkEvent::Result {
            video_id: "v".to_owned(),
            image: None,
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::ArtworkVideo,
        },
    );
    assert_policy(
        RuntimeEvent::Lyrics(crate::lyrics::LyricsEvent::Result {
            video_id: "v".to_owned(),
            lines: Vec::new().into(),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::LyricsVideo,
        },
    );
    assert_policy(
        RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("v"),
            purpose: crate::resolver::ResolvePurpose::SelfHeal,
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("v"),
            purpose: crate::resolver::ResolvePurpose::Prefetch,
        }),
        EventPolicy::CoalesceLatest {
            lane: EventLane::WorkResult,
            key: EventKey::ResolverVideo,
        },
    );
    assert_policy(
        RuntimeEvent::Video {
            generation: 1,
            event: crate::player::video::VideoEvent::Next,
        },
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        },
    );
    assert_policy(
        RuntimeEvent::Video {
            generation: 1,
            event: crate::player::video::VideoEvent::Paused(true),
        },
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::VideoOverlayPaused,
        },
    );
    assert_policy(
        RuntimeEvent::Video {
            generation: 1,
            event: crate::player::video::VideoEvent::Failed("ipc closed".to_owned()),
        },
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        },
    );
    assert_policy(
        RuntimeEvent::Tools(crate::tools::ToolsEvent::Progress {
            channel: crate::tools::YtdlpChannel::Nightly,
            percent: Some(20),
        }),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::ToolProgress,
        },
    );
    assert_policy(
        RuntimeEvent::Tools(crate::tools::ToolsEvent::Failed {
            error: "offline".to_owned(),
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::Update(crate::update::UpdateEvent::Checked(update_status())),
        EventPolicy::CoalesceLatest {
            lane: EventLane::WorkResult,
            key: EventKey::UpdateCheck,
        },
    );
    assert_policy(
        RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
            transfer_progress("job"),
        )),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::TransferJob,
        },
    );
    assert_policy(
        RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::JobFailed {
            job_id: "job".to_owned(),
            error: "failed".to_owned(),
            resumable: true,
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::JobRejected {
            job_id: "rejected".to_owned(),
            error: "busy".to_owned(),
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::TelemetryWake,
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        },
    );
}
