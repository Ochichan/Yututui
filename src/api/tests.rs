use super::*;

#[test]
fn old_persisted_song_json_still_deserializes() {
    // library.json/playlists.json entries written before `album`/`duration_secs` existed.
    let json = r#"{"video_id":"dQw4w9WgXcQ","title":"T","artist":"A","duration":"3:45"}"#;
    let song: Song = serde_json::from_str(json).expect("old JSON must load");
    assert_eq!(song.album, None);
    assert_eq!(song.duration_secs, None);
    // And untouched tracks keep serializing without the new fields (diff-clean stores).
    let out = serde_json::to_string(&song).expect("serialize");
    assert!(!out.contains("album"));
    assert!(!out.contains("duration_secs"));
}

#[test]
fn from_search_enriches_album_and_seconds() {
    let song = Song::from_search("id", "T", "A", "3:45", Some("Album".to_owned()));
    assert_eq!(song.album.as_deref(), Some("Album"));
    assert_eq!(song.duration_secs, Some(225));
    // Blank album strings from the parser are treated as absent.
    let song = Song::from_search("id", "T", "A", "", Some("  ".to_owned()));
    assert_eq!(song.album, None);
    assert_eq!(song.duration_secs, None);
}

#[test]
fn youtube_video_id_shape_is_strict_for_external_inputs() {
    assert!(is_youtube_video_id("dQw4w9WgXcQ"));
    assert!(is_youtube_video_id("TAfHyXrULiM"));
    assert!(!is_youtube_video_id("UCfLdIEPs1tYj4ieEdJnyNyw"));
    assert!(!is_youtube_video_id("PL123456789012345"));
    assert!(!is_youtube_video_id("too-short"));
    assert!(!is_youtube_video_id("bad/id/value"));
}

#[test]
fn non_video_youtube_refs_do_not_become_playback_targets() {
    let song = Song::remote("UCfLdIEPs1tYj4ieEdJnyNyw", "Lauv", "Lauv", "");
    assert_eq!(song.youtube_id(), None);
    assert!(song.prefetch_target().is_none());
    assert!(song.unplayable_youtube_ref_reason().is_some());
}

#[test]
fn playable_refs_cover_watch_prefetch_and_share_edges() {
    let youtube = Song {
        playable: Some(PlayableRef::YoutubeVideo {
            id: "dQw4w9WgXcQ".to_owned(),
        }),
        ..Song::remote("dQw4w9WgXcQ", "Video", "Artist", "3:00")
    };
    assert_eq!(youtube.youtube_id(), Some("dQw4w9WgXcQ"));
    assert_eq!(
        youtube.watch_url_checked().unwrap(),
        "https://music.youtube.com/watch?v=dQw4w9WgXcQ"
    );
    assert_eq!(
        youtube.prefetch_target().as_deref(),
        Some("https://music.youtube.com/watch?v=dQw4w9WgXcQ")
    );
    assert_eq!(
        youtube.share_url().as_deref(),
        Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );

    let channel = Song {
        playable: Some(PlayableRef::YoutubeVideo {
            id: "UCfLdIEPs1tYj4ieEdJnyNyw".to_owned(),
        }),
        ..Song::remote("UCfLdIEPs1tYj4ieEdJnyNyw", "Channel", "Owner", "")
    };
    assert_eq!(
        channel.watch_url_checked().unwrap(),
        "https://music.youtube.com/channel/UCfLdIEPs1tYj4ieEdJnyNyw"
    );
    assert!(channel.prefetch_target().is_none());
    assert!(channel.share_url().is_none());

    let jamendo = Song::from_source(
        SearchSource::Jamendo,
        "jam-42",
        "Jam",
        "Artist",
        "2:00",
        PlayableRef::JamendoTrackId {
            id: "jam-42".to_owned(),
            url: "https://usercontent.jamendo.com/track.mp3".to_owned(),
        },
    );
    assert_eq!(jamendo.video_id, "ja:jam-42");
    assert_eq!(
        jamendo.playback_target_checked().unwrap(),
        "https://usercontent.jamendo.com/track.mp3"
    );
    assert!(jamendo.prefetch_target().is_none());

    let archive = Song::from_source(
        SearchSource::InternetArchive,
        "archive-1",
        "Archive",
        "Curator",
        "4:00",
        PlayableRef::ArchiveFile {
            identifier: "collection".to_owned(),
            file: "track.mp3".to_owned(),
            url: "https://archive.org/download/collection/track.mp3".to_owned(),
        },
    );
    assert_eq!(
        archive.watch_url_checked().unwrap(),
        "https://archive.org/download/collection/track.mp3"
    );
    assert!(archive.prefetch_target().is_none());

    let radio = Song::from_source(
        SearchSource::RadioBrowser,
        "station-1",
        "Station",
        "Country",
        "LIVE",
        PlayableRef::RadioStream {
            url: "https://stream.example.net/live".to_owned(),
        },
    );
    assert_eq!(
        radio.watch_url_checked().unwrap(),
        "https://stream.example.net/live"
    );
    assert!(radio.prefetch_target().is_none());

    let invalid_direct = Song::from_source(
        SearchSource::Jamendo,
        "bad",
        "Bad",
        "Artist",
        "1:00",
        PlayableRef::DirectUrl {
            source: SearchSource::Jamendo,
            url: "http://127.0.0.1/private.mp3".to_owned(),
        },
    );
    assert!(matches!(
        invalid_direct.watch_url_checked(),
        Err(PlayableUrlError::BlockedIp(ip)) if ip == "127.0.0.1"
    ));
    assert_eq!(invalid_direct.watch_url(), "");
    assert_eq!(invalid_direct.playback_target(), "");
}

#[test]
fn song_constructors_sanitize_display_metadata() {
    let bidi = '\u{202e}';
    let song = Song::from_search(
        "id\nbad",
        format!("{}{}", "t".repeat(MAX_TITLE_CHARS + 20), bidi),
        "artist\nname",
        format!("{}{}", "1".repeat(MAX_DURATION_CHARS + 20), bidi),
        Some(format!("{}{}", "a".repeat(MAX_ALBUM_CHARS + 20), bidi)),
    );

    assert_eq!(song.video_id, "idbad");
    assert_eq!(song.title.chars().count(), MAX_TITLE_CHARS);
    assert_eq!(song.artist, "artistname");
    assert_eq!(song.duration.chars().count(), MAX_DURATION_CHARS);
    assert_eq!(
        song.album.as_ref().unwrap().chars().count(),
        MAX_ALBUM_CHARS
    );
    assert!(!song.title.contains(bidi));
    assert!(!song.album.as_ref().unwrap().contains(bidi));
}

#[test]
fn local_file_extracts_embedded_youtube_id_without_false_positives() {
    let tagged = Song::local_file(PathBuf::from("/music/Artist - Title [dQw4w9WgXcQ].m4a"));
    assert!(tagged.is_local());
    assert_eq!(tagged.title, "Artist - Title");
    assert_eq!(tagged.youtube_id(), Some("dQw4w9WgXcQ"));
    assert_eq!(
        tagged.share_url().as_deref(),
        Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );
    assert!(tagged.playback_target().ends_with("[dQw4w9WgXcQ].m4a"));

    assert_eq!(
        Song::parse_embedded_id("Mix [Vol. 3]"),
        None,
        "ordinary bracketed titles must not become YouTube ids"
    );
    assert_eq!(Song::parse_embedded_id("[dQw4w9WgXcQ]"), None);
}

#[test]
fn with_local_path_preserves_catalog_identity_for_share_and_prefetch_rules() {
    let catalog = Song::from_search(
        "dQw4w9WgXcQ",
        "Title",
        "Artist",
        "3:45",
        Some("Album".to_owned()),
    );
    let local = catalog.with_local_path(PathBuf::from("/tmp/cache/title.m4a"));

    assert!(local.is_local());
    assert!(local.video_id.starts_with("local:"));
    assert_eq!(local.yt_video_id.as_deref(), Some("dQw4w9WgXcQ"));
    assert_eq!(local.youtube_id(), Some("dQw4w9WgXcQ"));
    assert!(local.prefetch_target().is_none());
    assert_eq!(local.album.as_deref(), Some("Album"));
    assert_eq!(local.duration_secs, Some(225));
}

#[test]
fn playback_target_branches_match_playable_ref_kind() {
    let ytdlp = Song::from_source(
        SearchSource::SoundCloud,
        "track-1",
        "Cloud",
        "Artist",
        "2:00",
        PlayableRef::YtdlpUrl {
            source: SearchSource::SoundCloud,
            url: "https://soundcloud.com/a/t".to_owned(),
        },
    );
    assert_eq!(ytdlp.video_id, "sc:track-1");
    assert_eq!(ytdlp.watch_url(), "https://soundcloud.com/a/t");
    assert_eq!(
        ytdlp.prefetch_target().as_deref(),
        Some("https://soundcloud.com/a/t")
    );

    let direct = Song::from_source(
        SearchSource::Jamendo,
        "jam-1",
        "Jam",
        "Artist",
        "2:00",
        PlayableRef::DirectUrl {
            source: SearchSource::Jamendo,
            url: "https://cdn.example.org/audio.mp3".to_owned(),
        },
    );
    assert_eq!(direct.watch_url(), "https://cdn.example.org/audio.mp3");
    assert!(direct.prefetch_target().is_none());

    let audius = Song::from_source(
        SearchSource::Audius,
        "au-1",
        "Audius",
        "Artist",
        "2:00",
        PlayableRef::AudiusTrackId {
            id: "au-1".to_owned(),
            app_name: "ytm tui".to_owned(),
        },
    );
    let target = audius.watch_url();
    assert!(target.starts_with("https://discoveryprovider.audius.co/v1/tracks/au-1/stream?"));
    assert!(target.contains("app_name=ytm+tui"));
    assert_eq!(audius.prefetch_target().as_deref(), Some(target.as_str()));
}

#[test]
fn youtube_playlist_rows_keep_playlist_identity_out_of_track_playback() {
    let song = Song::remote(
        format!("{PLAYLIST_ID_PREFIX}PL1234567890"),
        "List",
        "Owner",
        "",
    );
    assert_eq!(song.youtube_playlist_id(), Some("PL1234567890"));
    assert_eq!(song.youtube_id(), None);
    assert!(song.unplayable_youtube_ref_reason().is_some());
    assert!(song.prefetch_target().is_none());

    let playable_channel = Song {
        playable: Some(PlayableRef::YoutubeVideo {
            id: "UCfLdIEPs1tYj4ieEdJnyNyw".to_owned(),
        }),
        ..Song::remote("fallbackid1", "Channel", "Owner", "")
    };
    assert!(playable_channel.watch_url_checked().is_ok());
    assert!(playable_channel.prefetch_target().is_none());
}

fn test_api_handle(
    interactive_cap: usize,
    bulk_cap: usize,
) -> (ApiHandle, Receiver<ApiCmd>, Receiver<ApiCmd>) {
    let (interactive_tx, interactive_rx) = mpsc::channel(interactive_cap);
    let (bulk_tx, bulk_rx) = mpsc::channel(bulk_cap);
    (
        ApiHandle {
            interactive_tx,
            bulk_tx,
        },
        interactive_rx,
        bulk_rx,
    )
}

#[test]
fn api_handle_reports_full_queue() {
    let (handle, _interactive_rx, _bulk_rx) = test_api_handle(1, 1);

    handle
        .search(1, "first", SearchSource::Youtube, SearchConfig::default())
        .expect("first command should fill the small channel");
    let err = handle
        .search(2, "second", SearchSource::Youtube, SearchConfig::default())
        .expect_err("second command should report a full channel");

    assert_eq!(
        err,
        ApiEnqueueError::Full {
            kind: ApiCommandKind::Search
        }
    );
}

#[test]
fn api_handle_reports_closed_queue() {
    let (handle, interactive_rx, _bulk_rx) = test_api_handle(1, 1);
    drop(interactive_rx);

    let err = handle
        .search(1, "lost", SearchSource::Youtube, SearchConfig::default())
        .expect_err("closed channel should be reported");

    assert_eq!(
        err,
        ApiEnqueueError::Closed {
            kind: ApiCommandKind::Search
        }
    );
}

#[test]
fn api_handle_enqueues_all_command_kinds_with_payloads() {
    let (handle, mut interactive_rx, mut bulk_rx) = test_api_handle(8, 8);

    handle
        .gui_search(
            GuiSearchRequestId::new(3, 7),
            "gui",
            SearchSource::All,
            SearchConfig::default(),
        )
        .unwrap();
    handle
        .streaming(
            66,
            "seed",
            "seed-id",
            vec!["old".to_owned()],
            12,
            StreamingMode::Discovery,
            SearchConfig::default(),
        )
        .unwrap();
    handle
        .streaming_preflight(
            77,
            "seed-id",
            vec![Song::remote("a", "A", "x", "1:00")],
            vec![Song::remote("b", "B", "x", "1:00")],
            StreamingMode::Focused,
            StreamingConfig::default(),
        )
        .unwrap();
    handle
        .resolve_track(9, "Artist Track", SearchConfig::default())
        .unwrap();
    handle.search_playlists(10, "mix").unwrap();
    handle
        .playlist_tracks("PL123", "Roadtrip", PlaylistIntent::Import)
        .unwrap();

    let ApiCmd::GuiSearch {
        request_id, source, ..
    } = interactive_rx.try_recv().unwrap()
    else {
        panic!("GUI search should use the interactive lane");
    };
    assert_eq!(request_id.parts(), (3, 7));
    assert_eq!(source, SearchSource::All);
    assert!(matches!(
        interactive_rx.try_recv().unwrap(),
        ApiCmd::ResolveTrack { seq: 9, .. }
    ));
    assert!(matches!(
        interactive_rx.try_recv().unwrap(),
        ApiCmd::SearchPlaylists { request_id: 10, .. }
    ));
    assert!(matches!(
        bulk_rx.try_recv().unwrap(),
        ApiCmd::Streaming {
            request_id: 66,
            limit: 12,
            mode: StreamingMode::Discovery,
            ..
        }
    ));
    assert!(matches!(
        bulk_rx.try_recv().unwrap(),
        ApiCmd::StreamingPreflight {
            request_id: 77,
            mode: StreamingMode::Focused,
            ..
        }
    ));
    assert!(matches!(
        bulk_rx.try_recv().unwrap(),
        ApiCmd::PlaylistTracks {
            intent: PlaylistIntent::Import,
            ..
        }
    ));
}

#[test]
fn api_command_kinds_route_to_expected_lanes() {
    assert_eq!(ApiCommandKind::Search.lane(), ApiLane::Interactive);
    assert_eq!(ApiCommandKind::GuiSearch.lane(), ApiLane::Interactive);
    assert_eq!(ApiCommandKind::ResolveTrack.lane(), ApiLane::Interactive);
    assert_eq!(ApiCommandKind::SearchPlaylists.lane(), ApiLane::Interactive);
    assert_eq!(ApiCommandKind::Streaming.lane(), ApiLane::Bulk);
    assert_eq!(ApiCommandKind::StreamingPreflight.lane(), ApiLane::Bulk);
    assert_eq!(ApiCommandKind::PlaylistTracks.lane(), ApiLane::Bulk);
}

#[test]
fn api_enqueue_error_display_names_the_failed_lane() {
    let err = ApiEnqueueError::Full {
        kind: ApiCommandKind::StreamingPreflight,
    };
    assert_eq!(err.kind(), ApiCommandKind::StreamingPreflight);
    assert_eq!(
        err.to_string(),
        "API streaming preflight queue is full; try again in a moment."
    );

    let err = ApiEnqueueError::Closed {
        kind: ApiCommandKind::PlaylistTracks,
    };
    assert_eq!(
        err.to_string(),
        "API playlist tracks worker is not running."
    );
}

#[test]
fn streaming_cache_cap_evicts_oldest_entries() {
    let mut cache = HashMap::new();
    let now = Instant::now();
    let oldest_key = (
        "oldest".to_owned(),
        StreamingMode::Balanced,
        SearchSource::Youtube,
    );
    cache.insert(
        oldest_key.clone(),
        (now - Duration::from_secs(60), Vec::new()),
    );
    for i in 0..STREAMING_YTDLP_CACHE_MAX {
        cache.insert(
            (
                format!("seed-{i}"),
                StreamingMode::Balanced,
                SearchSource::Youtube,
            ),
            (now + Duration::from_secs(i as u64), Vec::new()),
        );
    }

    enforce_streaming_cache_cap(&mut cache);

    assert_eq!(cache.len(), STREAMING_YTDLP_CACHE_MAX);
    assert!(!cache.contains_key(&oldest_key));
}
