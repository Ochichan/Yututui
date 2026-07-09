use super::*;
use crate::remote::proto::RemoteCommand;
use crate::util::event_policy::{EventKey, EventLane, EventPolicy};

fn song(video_id: &str) -> crate::api::Song {
    crate::api::Song::from_search(
        video_id,
        format!("Title {video_id}"),
        "Artist",
        "3:21",
        Some("Album".to_owned()),
    )
}

fn transfer_progress(job_id: &str) -> crate::transfer::TransferProgress {
    crate::transfer::TransferProgress {
        job_id: job_id.to_owned(),
        stage: crate::transfer::Stage::Matching,
        done: 1,
        total: 2,
        matched: 1,
        ambiguous: 0,
        not_found: 0,
        current: "Artist - Title".to_owned(),
    }
}

fn update_status() -> crate::update::UpdateStatus {
    crate::update::UpdateStatus {
        current: "1.0.0".to_owned(),
        latest: "v1.0.1".to_owned(),
        available: true,
        first_seen: true,
        method: crate::update::InstallMethod::Cargo,
    }
}

#[test]
fn pending_player_cmds_coalesce_latest_setters_and_load() {
    let mut pending = PendingPlayerCmds::default();
    pending.push(PlayerCmd::SetVolume(10));
    pending.push(PlayerCmd::SetProperty {
        name: "speed".to_owned(),
        value: serde_json::json!(1.2),
    });
    pending.push(PlayerCmd::Load("https://example.invalid/old".to_owned()));
    pending.push(PlayerCmd::CyclePause);
    pending.push(PlayerCmd::SetVolume(30));
    pending.push(PlayerCmd::SetProperty {
        name: "speed".to_owned(),
        value: serde_json::json!(1.5),
    });
    pending.push(PlayerCmd::Load("https://example.invalid/new".to_owned()));

    let drained = pending.drain();

    assert_eq!(drained.len(), 4);
    assert!(
        drained
            .iter()
            .any(|cmd| matches!(cmd, PlayerCmd::CyclePause))
    );
    assert!(
        drained
            .iter()
            .any(|cmd| matches!(cmd, PlayerCmd::SetVolume(30)))
    );
    assert!(drained.iter().any(|cmd| {
        matches!(cmd, PlayerCmd::SetProperty { name, value } if name == "speed" && value == &serde_json::json!(1.5))
    }));
    assert!(drained.iter().any(|cmd| {
        matches!(cmd, PlayerCmd::Load(url) if url == "https://example.invalid/new")
    }));
}

#[test]
fn pending_player_cmds_cap_keeps_latest_load() {
    let mut pending = PendingPlayerCmds::default();
    pending.push(PlayerCmd::Load(
        "https://example.invalid/current".to_owned(),
    ));
    for i in 0..80 {
        pending.push(PlayerCmd::SeekRelative(i as f64));
    }

    assert_eq!(pending.len(), PENDING_PLAYER_CMDS_MAX);
    let drained = pending.drain();
    assert!(drained.iter().any(|cmd| {
        matches!(cmd, PlayerCmd::Load(url) if url == "https://example.invalid/current")
    }));
}

fn assert_policy(event: RuntimeEvent, expected: EventPolicy) {
    assert_eq!(event.policy(), expected);
}

#[test]
fn runtime_event_policy_covers_representative_events() {
    assert_eq!(
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).policy(),
        EventPolicy::MustDeliver {
            lane: EventLane::Control
        }
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::Eof).policy(),
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
            reply,
        ))
        .policy(),
        EventPolicy::MustReplyOrBusy {
            lane: EventLane::RemoteCommand
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
            lane: EventLane::WorkResult
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
        EventPolicy::BestEffort { .. }
    ));
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
        EventPolicy::MustReplyOrBusy {
            lane: EventLane::WorkResult,
        },
    );
    assert_policy(
        RuntimeEvent::Api(ApiEvent::GuiSearchCompleted {
            ticket: 9,
            query: "q".to_owned(),
            source: SearchSource::All,
            groups: Vec::new(),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::GuiSearchTicket,
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
            lines: Vec::new(),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::LyricsVideo,
        },
    );
    assert_policy(
        RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
            video_id: crate::ids::VideoId::from("v"),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::ResolverVideo,
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
        RuntimeEvent::TelemetryWake,
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        },
    );
}

#[test]
fn runtime_event_kind_and_telemetry_slots_are_stable() {
    assert_eq!(
        RuntimeEvent::Ai(crate::ai::AiEvent::Chat("hi".to_owned())).kind(),
        "ai"
    );
    assert_eq!(
        RuntimeEvent::Api(crate::api::ApiEvent::StreamingError {
            seed_video_id: "seed".to_owned(),
            error: "e".to_owned(),
        })
        .kind(),
        "api"
    );
    assert_eq!(
        RuntimeEvent::Download(crate::download::DownloadEvent::Done {
            video_id: "v".to_owned(),
            path: "v.m4a".to_owned(),
        })
        .kind(),
        "download"
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).kind(),
        "player"
    );
    assert_eq!(
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).kind(),
        "signal"
    );
    assert_eq!(RuntimeEvent::TelemetryWake.kind(), "telemetry_wake");
    assert!(RuntimeEvent::TelemetryWake.is_telemetry_wake());
    assert!(!RuntimeEvent::Player(crate::player::PlayerEvent::Eof).is_telemetry_wake());

    assert_eq!(
        RuntimeEvent::App(Msg::DownloadProgress {
            video_id: "a".to_owned(),
            percent: 1.0,
        })
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::DownloadProgress("a".to_owned()))
    );
    assert_eq!(
        RuntimeEvent::App(Msg::MediaArtworkReady(
            crate::media::artwork::MediaArtworkReady {
                key: "cover-key".to_owned(),
                path: "cover.jpg".into(),
            },
        ))
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::MediaArt("cover-key".to_owned()))
    );
    assert_eq!(
        RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
            transfer_progress("import-1"),
        ))
        .telemetry_slot(),
        Some(RuntimeTelemetrySlot::TransferProgress(
            "import-1".to_owned()
        ))
    );
    assert_eq!(
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).telemetry_slot(),
        Some(RuntimeTelemetrySlot::Static(EventKey::PlayerTimePos))
    );
    assert_eq!(
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).telemetry_slot(),
        None
    );
}

#[test]
fn app_message_policy_covers_backpressure_lanes() {
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    assert_eq!(
        app_msg_policy(&Msg::Remote(RemoteCommand::TogglePause, reply)),
        EventPolicy::MustReplyOrBusy {
            lane: EventLane::RemoteCommand,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::DownloadProgress {
            video_id: "v".to_owned(),
            percent: 12.0,
        }),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::DownloadProgress,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::MediaArtworkReady(
            crate::media::artwork::MediaArtworkReady {
                key: "v".to_owned(),
                path: "cover.jpg".into(),
            },
        )),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::MediaArtVideo,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Local(crate::app::LocalMsg::ScanProgress(
            crate::local::LocalScanProgress::default(),
        ))),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::LocalScanProgress,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::SearchError {
            request_id: 1,
            source: crate::search_source::SearchSource::Youtube,
            error: "offline".to_owned(),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::SearchRequest,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::TrackResolved {
            seq: 1,
            result: Ok(Vec::new()),
        }),
        EventPolicy::DropIfStale {
            stale_key: EventKey::ResolverVideo,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Streaming(StreamingMsg::Error {
            seed_video_id: "seed".to_owned(),
            error: "empty".to_owned(),
        })),
        EventPolicy::DropIfStale {
            stale_key: EventKey::StreamingSeed,
        }
    );
    assert!(matches!(
        app_msg_policy(&Msg::Noop),
        EventPolicy::BestEffort { .. }
    ));
    assert_eq!(
        app_msg_policy(&Msg::Media(crate::media::MediaCommand::Play)),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        app_msg_policy(&Msg::Transfer(
            crate::transfer::actor::TransferEvent::Disconnected,
        )),
        EventPolicy::MustDeliver {
            lane: EventLane::WorkResult,
        }
    );
}

#[test]
fn player_message_policy_covers_each_property_lane() {
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::TimePos(1.0)),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerTimePos,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::Metadata(serde_json::json!({"title":"Song"}))),
        EventPolicy::CoalesceLatest {
            lane: EventLane::WorkResult,
            key: EventKey::PlayerMetadata,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::AudioCodec(Some("aac".to_owned()))),
        EventPolicy::CoalesceLatest {
            lane: EventLane::Telemetry,
            key: EventKey::PlayerAudioCodec,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::VideoOverlay {
            generation: 2,
            event: crate::player::video::VideoEvent::Closed,
        }),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
    assert_eq!(
        app_player_msg_policy(&PlayerMsg::Error("boom".to_owned())),
        EventPolicy::MustDeliver {
            lane: EventLane::Control,
        }
    );
}

#[test]
fn runtime_event_to_msg_preserves_ai_api_and_transport_payloads() {
    let msg = Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::SetStationProfile {
        query: "city pop".to_owned(),
        explore: Some("wide".to_owned()),
        avoid_artists: vec!["skip".to_owned()],
    }));
    assert!(matches!(
        msg,
        Msg::Ai(AiMsg::SetStationProfile {
            query,
            explore: Some(explore),
            avoid_artists,
        }) if query == "city pop" && explore == "wide" && avoid_artists == ["skip"]
    ));

    let msg = Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::StreamingPicks {
        seed_video_id: "seed".to_owned(),
        picks: vec![crate::app::AiPick {
            cid: "c1".to_owned(),
            role: Some("bridge".to_owned()),
            reasons: vec!["tr".to_owned()],
        }],
        conf: Some(0.75),
    }));
    assert!(matches!(
        msg,
        Msg::Streaming(StreamingMsg::AiPicks {
            seed_video_id,
            picks,
            conf: Some(conf),
        }) if seed_video_id == "seed" && picks[0].cid == "c1" && (conf - 0.75).abs() < f32::EPSILON
    ));

    let msg = Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::SearchResults {
        request_id: 3,
        query: "query".to_owned(),
        source: crate::search_source::SearchSource::SoundCloud,
        songs: vec![song("v1")],
        timed_out: true,
    }));
    assert!(matches!(
        msg,
        Msg::SearchResults {
            request_id: 3,
            query,
            source: crate::search_source::SearchSource::SoundCloud,
            songs,
            timed_out: true,
        } if query == "query" && songs[0].video_id == "v1"
    ));

    let msg = Msg::from(RuntimeEvent::Api(
        crate::api::ApiEvent::PlaylistTracksError {
            title: "mix".to_owned(),
            error: "denied".to_owned(),
        },
    ));
    assert!(matches!(
        msg,
        Msg::PlaylistTracksError { title, error } if title == "mix" && error == "denied"
    ));

    let msg = Msg::from(RuntimeEvent::Download(
        crate::download::DownloadEvent::Error {
            video_id: "v2".to_owned(),
            error: "disk".to_owned(),
        },
    ));
    assert!(matches!(
        msg,
        Msg::DownloadError { video_id, error } if video_id == "v2" && error == "disk"
    ));

    let msg = Msg::from(RuntimeEvent::Player(
        crate::player::PlayerEvent::FileFormat(Some("mp4".to_owned())),
    ));
    assert!(matches!(
        msg,
        Msg::Player(PlayerMsg::FileFormat(Some(format))) if format == "mp4"
    ));

    let (reply, _reply_rx) = tokio::sync::oneshot::channel();
    let msg = Msg::from(RuntimeEvent::Remote(
        crate::remote::server::RemoteEvent::Command(RemoteCommand::Next, reply),
    ));
    assert!(matches!(msg, Msg::Remote(RemoteCommand::Next, _)));

    let msg = Msg::from(RuntimeEvent::Video {
        generation: 42,
        event: crate::player::video::VideoEvent::Failed("403".to_owned()),
    });
    assert!(matches!(
        msg,
        Msg::Player(PlayerMsg::VideoOverlay {
            generation: 42,
            event: crate::player::video::VideoEvent::Failed(error),
        }) if error == "403"
    ));
}

#[test]
fn runtime_event_to_msg_validates_resolver_urls_and_side_channels() {
    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Resolved {
            video_id: crate::ids::VideoId::from("v1"),
            stream_url: crate::ids::StreamUrl::from("https://rr1---sn.test/video.m4a"),
        },
    ));
    assert!(matches!(
        msg,
        Msg::Streaming(StreamingMsg::Resolved {
            video_id,
            stream_url,
        }) if video_id == "v1" && stream_url.starts_with("https://")
    ));

    let msg = Msg::from(RuntimeEvent::Resolver(
        crate::resolver::ResolverEvent::Resolved {
            video_id: crate::ids::VideoId::from("v2"),
            stream_url: crate::ids::StreamUrl::from("file:///etc/passwd"),
        },
    ));
    assert!(matches!(msg, Msg::ResolveFailed { video_id } if video_id == "v2"));

    let msg = Msg::from(RuntimeEvent::Api(
        crate::api::ApiEvent::GuiSearchCompleted {
            ticket: 1,
            query: "ignored".to_owned(),
            source: crate::search_source::SearchSource::All,
            groups: Vec::new(),
        },
    ));
    assert!(matches!(msg, Msg::Noop));

    let msg = Msg::from(RuntimeEvent::Signal(
        crate::player::lifetime::SignalEvent::Quit,
    ));
    assert!(matches!(msg, Msg::Quit));

    let msg = Msg::from(RuntimeEvent::Update(crate::update::UpdateEvent::Checked(
        update_status(),
    )));
    assert!(matches!(
        msg,
        Msg::UpdateChecked(status) if status.latest == "v1.0.1" && status.available
    ));

    let msg = Msg::from(RuntimeEvent::Transfer(
        crate::transfer::actor::TransferEvent::Progress(transfer_progress("job-2")),
    ));
    assert!(matches!(
        msg,
        Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(progress))
            if progress.job_id == "job-2"
    ));
}

#[test]
fn runtime_event_to_msg_preserves_ai_payload_variants() {
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Thinking(true))),
        Msg::Ai(AiMsg::Thinking(true))
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Error(
            "no key".to_owned()
        ))),
        Msg::Ai(AiMsg::Error(error)) if error == "no key"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::PlayTracks(vec![song(
            "play1234567"
        )]))),
        Msg::Ai(AiMsg::PlayTracks(songs)) if songs[0].video_id == "play1234567"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Enqueue(vec![song(
            "queue123456"
        )]))),
        Msg::Ai(AiMsg::Enqueue(songs)) if songs[0].video_id == "queue123456"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Suggestions(vec![
            song("suggest1234")
        ]))),
        Msg::Ai(AiMsg::Suggestions(songs)) if songs[0].video_id == "suggest1234"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::SetAutoplay(false))),
        Msg::Ai(AiMsg::SetAutoplay(false))
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::CreatePlaylist(
            "Road".to_owned()
        ))),
        Msg::Ai(AiMsg::CreatePlaylist(name)) if name == "Road"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::AddToPlaylist {
            playlist: "Road".to_owned(),
            songs: vec![song("add12345678")],
        })),
        Msg::Ai(AiMsg::AddToPlaylist { playlist, songs })
            if playlist == "Road" && songs[0].video_id == "add12345678"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::PlayPlaylist(
            "Favorites".to_owned()
        ))),
        Msg::Ai(AiMsg::PlayPlaylist(key)) if key == "Favorites"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::StationPatch {
            down_artists: vec!["Skip".to_owned()],
            boost_artists: vec!["Boost".to_owned()],
        })),
        Msg::Ai(AiMsg::StationPatch {
            down_artists,
            boost_artists,
        }) if down_artists == ["Skip"] && boost_artists == ["Boost"]
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::RomanizedTitles {
            request_id: 77,
            keys: vec!["k1".to_owned()],
            entries: vec![crate::romanize::RomanizedResult {
                key: "k1".to_owned(),
                title: "Title".to_owned(),
                artist: "Artist".to_owned(),
                confidence: Some(0.9),
            }],
        })),
        Msg::Ai(AiMsg::RomanizedTitles {
            request_id: 77,
            keys,
            entries,
        }) if keys == ["k1"] && entries[0].title == "Title"
    ));
}

#[test]
fn runtime_event_to_msg_preserves_api_player_and_service_payloads() {
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::ModeResolved {
            mode: crate::api::ApiMode::Authenticated,
            had_cookie: true,
        })),
        Msg::ApiModeResolved {
            mode: crate::api::ApiMode::Authenticated,
            had_cookie: true,
        }
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::TrackResolved {
            seq: 12,
            result: Err("not found".to_owned()),
        })),
        Msg::TrackResolved {
            seq: 12,
            result: Err(error),
        } if error == "not found"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::PlaylistTracks {
            title: "Mix".to_owned(),
            intent: crate::api::PlaylistIntent::Enqueue,
            songs: vec![song("plist123456")],
        })),
        Msg::PlaylistTracks {
            title,
            intent: crate::api::PlaylistIntent::Enqueue,
            songs,
        } if title == "Mix" && songs[0].video_id == "plist123456"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::StreamingResults {
            seed_video_id: "seed".to_owned(),
            candidates: vec![(song("cand1234567"), crate::streaming::CandidateSource::YtdlpStreaming)],
        })),
        Msg::Streaming(StreamingMsg::Results {
            seed_video_id,
            candidates,
        }) if seed_video_id == "seed"
            && candidates[0].0.video_id == "cand1234567"
            && candidates[0].1 == crate::streaming::CandidateSource::YtdlpStreaming
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::StreamingPreflighted {
            seed_video_id: "seed".to_owned(),
            songs: vec![song("pre12345678")],
        })),
        Msg::Streaming(StreamingMsg::Preflighted { seed_video_id, songs })
            if seed_video_id == "seed" && songs[0].video_id == "pre12345678"
    ));

    for (event, assert_msg) in [
        (crate::player::PlayerEvent::Duration(Some(88.0)), "duration"),
        (crate::player::PlayerEvent::Paused(true), "paused"),
        (crate::player::PlayerEvent::Volume(42.0), "volume"),
        (crate::player::PlayerEvent::CacheTime(Some(91.0)), "cache"),
        (
            crate::player::PlayerEvent::AudioCodec(Some("aac".to_owned())),
            "codec",
        ),
        (crate::player::PlayerEvent::Eof, "eof"),
        (
            crate::player::PlayerEvent::Error("decode".to_owned()),
            "error",
        ),
    ] {
        let msg = Msg::from(RuntimeEvent::Player(event));
        match assert_msg {
            "duration" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::Duration(Some(d))) if (d - 88.0).abs() < f64::EPSILON
            )),
            "paused" => assert!(matches!(msg, Msg::Player(PlayerMsg::Paused(true)))),
            "volume" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::Volume(v)) if (v - 42.0).abs() < f64::EPSILON
            )),
            "cache" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::CacheTime(Some(t))) if (t - 91.0).abs() < f64::EPSILON
            )),
            "codec" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::AudioCodec(Some(codec))) if codec == "aac"
            )),
            "eof" => assert!(matches!(msg, Msg::Player(PlayerMsg::Eof))),
            "error" => assert!(matches!(
                msg,
                Msg::Player(PlayerMsg::Error(error)) if error == "decode"
            )),
            _ => unreachable!(),
        }
    }

    assert!(matches!(
        Msg::from(RuntimeEvent::Scrobble(
            crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 4 },
        )),
        Msg::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 4 })
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Tools(crate::tools::ToolsEvent::Installed {
            version: "2026.07.07".to_owned(),
        })),
        Msg::Tools(crate::tools::ToolsEvent::Installed { version })
            if version == "2026.07.07"
    ));
    assert!(matches!(
        Msg::from(RuntimeEvent::Transfer(
            crate::transfer::actor::TransferEvent::AuthDone {
                display_name: "Tester".to_owned(),
            },
        )),
        Msg::Transfer(crate::transfer::actor::TransferEvent::AuthDone { display_name })
            if display_name == "Tester"
    ));
}

#[tokio::test]
async fn must_deliver_runtime_event_waits_when_owner_lane_is_full() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );

    assert!(emit(
        &tx,
        RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit)
    ));
    assert!(matches!(
        rx.recv().await,
        Some(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(_)))
    ));
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await,
        Ok(Some(RuntimeEvent::Signal(
            crate::player::lifetime::SignalEvent::Quit
        )))
    ));
}

#[test]
fn remote_runtime_event_reports_full_to_callers() {
    let (raw_tx, _rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx.clone());
    assert!(
        raw_tx
            .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                1.0
            )))
            .is_ok()
    );
    let (reply, _reply_rx) = tokio::sync::oneshot::channel();

    assert!(!emit(
        &tx,
        RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(
            RemoteCommand::TogglePause,
            reply,
        ))
    ));
}

#[test]
fn runtime_telemetry_coalesces_time_pos_to_one_wake() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
    let tx = RuntimeSender::new(raw_tx);

    for tick in 0..10_000 {
        assert!(emit(
            &tx,
            RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(tick as f64))
        ));
    }

    assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
    assert!(rx.try_recv().is_err());
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 1);
    assert!(matches!(
        &drained[0],
        RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(t)) if (*t - 9999.0).abs() < f64::EPSILON
    ));
}

#[test]
fn runtime_download_progress_coalesces_without_displacing_final_event() {
    let (raw_tx, mut rx) = tokio::sync::mpsc::channel(4);
    let tx = RuntimeSender::new(raw_tx);

    assert!(emit(
        &tx,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id: "a".to_owned(),
            percent: 10.0,
        })
    ));
    assert!(emit(
        &tx,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id: "a".to_owned(),
            percent: 70.0,
        })
    ));
    assert!(emit(
        &tx,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id: "b".to_owned(),
            percent: 40.0,
        })
    ));
    assert!(emit(
        &tx,
        RuntimeEvent::Download(crate::download::DownloadEvent::Done {
            video_id: "a".to_owned(),
            path: "a.m4a".to_owned(),
        })
    ));

    assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
    assert!(matches!(
        rx.try_recv(),
        Ok(RuntimeEvent::Download(crate::download::DownloadEvent::Done {
            video_id,
            ..
        })) if video_id == "a"
    ));
    let drained = tx.drain_coalesced();
    assert_eq!(drained.len(), 2);
    assert!(drained.iter().any(|event| matches!(
        event,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id,
            percent,
        }) if video_id == "a" && (*percent - 70.0).abs() < f64::EPSILON
    )));
    assert!(drained.iter().any(|event| matches!(
        event,
        RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
            video_id,
            percent,
        }) if video_id == "b" && (*percent - 40.0).abs() < f64::EPSILON
    )));
}
