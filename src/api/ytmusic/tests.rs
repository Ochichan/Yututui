//! Tests for the YouTube Music API integration.

use super::*;

#[cfg(unix)]
struct FakeYtdlpGuard {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl Drop for FakeYtdlpGuard {
    fn drop(&mut self) {
        *TEST_YTDLP_PROGRAM.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(unix)]
async fn with_fake_ytdlp() -> FakeYtdlpGuard {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let dir = std::env::temp_dir().join(format!("ytt-ytmusic-fake-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fake yt-dlp dir");
    let bin = dir.join("yt-dlp");
    std::fs::write(
            &bin,
            r#"#!/bin/sh
case " $* " in
  *" --version "*) echo '2026.07.07'; exit 0 ;;
esac
args="$*"
if printf '%s' "$args" | grep -q 'watch?v=aaa111bbb22'; then
  cat <<'JSON'
{"title":"Metadata Song","channel":"Meta Artist","duration":242,"live_status":"not_live","media_type":"video","description":"official audio"}
JSON
elif printf '%s' "$args" | grep -q 'playlist?list=PLfakeList'; then
  if printf '%s' "$args" | grep -q -- '--playlist-items 0'; then
    cat <<'JSON'
{"title":"Fake Playlist","channel":"Curator","playlist_count":3}
JSON
  else
    cat <<'JSON'
{"entries":[
  {"id":"aaa111bbb22","title":"Playlist Song","channel":"Playlist Artist","duration":181},
  {"id":"PLnot-a-video-playlist-id","title":"Playlist row"},
  {"id":"bbb222ccc33","title":"Second Playlist Song","uploader":"Uploader Artist","duration":0}
]}
JSON
  fi
elif printf '%s' "$args" | grep -q 'youtube.com/results'; then
  cat <<'JSON'
{"entries":[
  {"id":"PLfakeList","url":"https://www.youtube.com/playlist?list=PLfakeList","title":"Fake Playlist","uploader":"Curator","playlist_count":3},
  {"id":"aaa111bbb22","url":"https://www.youtube.com/watch?v=aaa111bbb22","title":"Plain Video"}
]}
JSON
else
  cat <<'JSON'
{"entries":[
  {"id":"aaa111bbb22","title":"Search Song","uploader":"Search Artist","duration":123},
  {"id":"aaa111bbb22","title":"Duplicate Song","uploader":"Search Artist","duration":123},
  {"id":"bbb222ccc33","title":"Second Song","channel":"Second Artist","duration":245},
  {"id":"UCnotavideoid","title":"Channel Row"}
]}
JSON
fi
"#,
        )
        .expect("write fake yt-dlp");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake yt-dlp");
    *TEST_YTDLP_PROGRAM.lock().unwrap_or_else(|e| e.into_inner()) = Some(bin);
    FakeYtdlpGuard { _guard: guard }
}

#[tokio::test]
async fn auth_search_breaker_requires_a_streak_resets_and_expires() {
    let _guard = AUTH_SEARCH_TEST_LOCK.lock().await;
    mark_auth_search_healthy();
    assert!(!auth_search_degraded());

    mark_auth_search_degraded();
    assert!(
        !auth_search_degraded(),
        "one transient failure stays closed"
    );
    mark_auth_search_healthy();
    assert!(!auth_search_degraded(), "success clears the failure streak");

    mark_auth_search_degraded();
    mark_auth_search_degraded();
    assert!(auth_search_degraded());

    AUTH_SEARCH_HEALTH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .degraded_until = Some(Instant::now() - Duration::from_millis(1));
    assert!(!auth_search_degraded());
    assert!(
        AUTH_SEARCH_HEALTH
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .degraded_until
            .is_none(),
        "expired degraded-search latch should clear itself"
    );
    mark_auth_search_healthy();
}

#[test]
fn anonymous_account_operations_share_cookie_error() {
    let Err(error) = YtMusicApi::Anonymous.browser() else {
        panic!("anonymous mode must reject account operations");
    };
    let error = error.to_string();
    assert!(error.contains("YouTube Music cookie"));
    assert!(error.contains("Settings"));
}

#[tokio::test]
async fn from_cookie_rejects_visitor_and_lookalike_cookies() {
    for cookie in [
        "PREF=tz=Asia.Seoul; YSC=visitor",
        "X-SAPISID=not-real; PREF=tz=Asia.Seoul",
        "__Secure-3PAPISID_LOOKALIKE=not-real",
    ] {
        let Err(error) = YtMusicApi::from_cookie(cookie).await else {
            panic!("visitor/lookalike cookie should not authenticate");
        };
        let error = error.to_string();
        assert!(error.contains("no login session"));
        assert!(error.contains("SAPISID"));
    }
}

#[test]
fn json_helpers_use_first_typed_match_only() {
    let json = serde_json::json!({
        "title": 42,
        "fallback_title": "Readable",
        "live": "false",
        "was_live": true
    });
    assert_eq!(
        json_string(&json, &["missing", "title", "fallback_title"]),
        Some("Readable".to_owned())
    );
    assert_eq!(json_bool(&json, &["live", "was_live"]), Some(true));
    assert_eq!(json_bool(&json, &["missing", "live"]), None);
}

#[test]
fn ytdlp_metadata_mapper_keeps_authority_and_bounded_audio_summary() {
    let meta = parse_ytdlp_video_meta(&serde_json::json!({
        "title": "Artist - Song (Official Video)",
        "channel": "ArtistVEVO",
        "channel_id": "UCchannel",
        "uploader_id": "artistvevo",
        "channel_is_verified": true,
        "availability": "public",
        "extractor": "youtube",
        "extractor_key": "Youtube",
        "duration": 201.4,
        "live_status": "not_live",
        "is_live": false,
        "was_live": false,
        "media_type": "video",
        "description": "Provided to YouTube by Label",
        "requested_formats": [
            {"format_id": "137", "ext": "mp4", "vcodec": "avc1", "acodec": "none"},
            {
                "format_id": "140",
                "format_note": "medium",
                "audio_ext": "m4a",
                "vcodec": "none",
                "acodec": "mp4a.40.2",
                "abr": 129.5,
                "asr": 44100
            }
        ],
        "formats": [
            {"format_id": "18", "vcodec": "avc1", "acodec": "mp4a.40.2", "abr": 96},
            {"format_id": "140", "vcodec": "none", "acodec": "mp4a.40.2", "abr": 129.5},
            {"format_id": "251", "vcodec": "none", "acodec": "opus", "abr": 160},
            {"format_id": "137", "vcodec": "avc1", "acodec": "none"}
        ]
    }));

    assert_eq!(meta.channel_id.as_deref(), Some("UCchannel"));
    assert_eq!(meta.uploader_id.as_deref(), Some("artistvevo"));
    assert_eq!(meta.channel_is_verified, Some(true));
    assert_eq!(meta.availability.as_deref(), Some("public"));
    assert_eq!(meta.extractor.as_deref(), Some("youtube"));
    assert_eq!(meta.audio.selected_format_id.as_deref(), Some("140"));
    assert_eq!(
        meta.audio.selected_audio_codec.as_deref(),
        Some("mp4a.40.2")
    );
    assert_eq!(meta.audio.selected_audio_bitrate_kbps, Some(129.5));
    assert_eq!(meta.audio.selected_sample_rate_hz, Some(44_100));
    assert_eq!(meta.audio.available_audio_formats, Some(3));
    assert_eq!(meta.audio.available_audio_only_formats, Some(2));
    assert_eq!(meta.audio.max_audio_bitrate_kbps, Some(160.0));
}

#[test]
fn ytdlp_metadata_added_fields_default_when_reading_legacy_json() {
    let meta: YtdlpVideoMeta = serde_json::from_value(serde_json::json!({
        "title": "Legacy",
        "channel": "Artist",
        "duration_secs": 180,
        "live_status": null,
        "is_live": false,
        "was_live": false,
        "media_type": "video",
        "description": null
    }))
    .expect("legacy metadata remains readable");

    assert_eq!(meta.channel_id, None);
    assert_eq!(meta.channel_is_verified, None);
    assert_eq!(meta.audio, YtdlpAudioSummary::default());
}

#[test]
fn split_seed_accepts_dash_variants_and_rejects_empty_sides() {
    assert_eq!(split_seed("  Track — Artist  "), Some(("Track", "Artist")));
    assert_eq!(split_seed("Track - Artist"), Some(("Track", "Artist")));
    assert_eq!(split_seed("Track -   "), None);
    assert_eq!(split_seed("No separator"), None);
}

#[test]
fn push_query_keeps_first_occurrence_order() {
    let mut queries = vec!["seed radio".to_owned()];
    push_query(&mut queries, "seed radio".to_owned());
    push_query(&mut queries, "seed songs".to_owned());
    push_query(&mut queries, "seed songs".to_owned());
    assert_eq!(queries, vec!["seed radio", "seed songs"]);
}

#[tokio::test]
async fn disabled_sources_and_non_track_recommendation_sources_fail_before_network() {
    let mut cfg = SearchConfig::default();
    cfg.set_enabled(SearchSource::SoundCloud, false);

    let err = YtMusicApi::Anonymous
        .search_one_source("artist", SearchSource::SoundCloud, &cfg)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("SoundCloud is disabled"));

    let excluded = HashSet::new();
    let err = related_tracks_from_source(
        "Song - Artist",
        SearchSource::SoundCloud,
        &cfg,
        5,
        &excluded,
        StreamingMode::Balanced,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("SoundCloud is disabled"));

    let err = related_tracks_from_source(
        "Song - Artist",
        SearchSource::RadioBrowser,
        &SearchConfig::default(),
        5,
        &excluded,
        StreamingMode::Balanced,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("Radio Browser streams are not used"));

    let err = related_tracks_from_source(
        "Song - Artist",
        SearchSource::All,
        &SearchConfig::default(),
        5,
        &excluded,
        StreamingMode::Balanced,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("nested ALL"));
}

#[tokio::test]
async fn provider_source_helpers_reject_unusable_config_without_provider_calls() {
    let cfg = SearchConfig::default();
    let err = search_external_source(SearchSource::RadioBrowser, "q", &cfg, 5)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a track recommendation source"));

    let err = search_external_source(SearchSource::All, "q", &cfg, 5)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a track recommendation source"));

    let err = search_external_source(SearchSource::Jamendo, "q", &cfg, 5)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Jamendo client_id is missing"));
}

#[tokio::test]
async fn all_source_search_with_no_enabled_sources_is_an_empty_complete_result() {
    let cfg = SearchConfig {
        youtube: false,
        soundcloud: false,
        audius: false,
        jamendo: false,
        internet_archive: false,
        radio_browser: false,
        ..SearchConfig::default()
    };

    let (songs, timed_out) = YtMusicApi::Anonymous
        .search_all_sources("anything", &cfg)
        .await
        .unwrap();

    assert!(songs.is_empty());
    assert!(!timed_out);
}

#[cfg(unix)]
#[tokio::test]
async fn anonymous_ytdlp_search_uses_selected_binary_and_filters_results() {
    let _guard = with_fake_ytdlp().await;

    let (songs, timed_out) = YtMusicApi::Anonymous
        .search_songs_reported(
            "Search Song",
            SearchSource::Youtube,
            &SearchConfig::default(),
        )
        .await
        .expect("fake yt-dlp search");

    assert!(!timed_out);
    assert_eq!(
        songs
            .iter()
            .map(|song| (
                song.video_id.as_str(),
                song.title.as_str(),
                song.duration.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("aaa111bbb22", "Search Song", "2:03"),
            ("aaa111bbb22", "Duplicate Song", "2:03"),
            ("bbb222ccc33", "Second Song", "4:05"),
        ]
    );
    assert!(songs.iter().all(|song| song.youtube_id().is_some()));
}

#[cfg(unix)]
#[tokio::test]
async fn pasted_youtube_urls_use_ytdlp_metadata_without_text_search() {
    let _guard = with_fake_ytdlp().await;

    let (songs, timed_out) = YtMusicApi::Anonymous
        .search_songs_reported(
            "https://youtu.be/aaa111bbb22",
            SearchSource::All,
            &SearchConfig::default(),
        )
        .await
        .expect("pasted video lookup");

    assert!(!timed_out);
    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0].video_id, "aaa111bbb22");
    assert_eq!(songs[0].title, "Metadata Song");
    assert_eq!(songs[0].artist, "Meta Artist");
    assert_eq!(songs[0].duration, "4:02");
}

#[cfg(unix)]
#[tokio::test]
async fn playlist_ytdlp_boundaries_return_rows_and_tracks() {
    let _guard = with_fake_ytdlp().await;

    let rows = YtMusicApi::Anonymous
        .search_playlists("lofi focus")
        .await
        .expect("playlist search");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].video_id, "ytpl:PLfakeList");
    assert_eq!(rows[0].title, "Fake Playlist");
    assert_eq!(rows[0].duration, "3 tracks");

    let (direct, timed_out) = YtMusicApi::Anonymous
        .search_songs_reported(
            "https://www.youtube.com/playlist?list=PLfakeList",
            SearchSource::Youtube,
            &SearchConfig::default(),
        )
        .await
        .expect("direct playlist row");
    assert!(!timed_out);
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].video_id, "ytpl:PLfakeList");
    assert_eq!(direct[0].artist, "Curator");

    let tracks = YtMusicApi::Anonymous
        .playlist_tracks("ytpl:PLfakeList")
        .await
        .expect("playlist tracks");
    assert_eq!(
        tracks
            .iter()
            .map(|song| (
                song.video_id.as_str(),
                song.title.as_str(),
                song.duration.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("aaa111bbb22", "Playlist Song", "3:01"),
            ("bbb222ccc33", "Second Playlist Song", ""),
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn related_tracks_dedupes_excluded_ids_from_ytdlp_results() {
    let _guard = with_fake_ytdlp().await;
    let excluded = HashSet::from(["aaa111bbb22".to_owned()]);

    let songs = related_tracks("Seed - Artist", 2, &excluded, StreamingMode::Balanced)
        .await
        .expect("related tracks");

    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0].video_id, "bbb222ccc33");
    assert_eq!(songs[0].title, "Second Song");
}

#[test]
fn playlist_search_entries_keep_playlists_and_drop_videos() {
    let json = serde_json::json!({
        "entries": [
            {
                "id": "PLabcdefgh1234567890abcdefgh12345",
                "url": "https://www.youtube.com/playlist?list=PLabcdefgh1234567890abcdefgh12345",
                "title": "Chill Mix",
                "uploader": "Some Curator",
                "playlist_count": 42
            },
            // An interleaved plain video (11-char id, no list=): dropped.
            { "id": "abc12345678", "url": "https://www.youtube.com/watch?v=abc12345678", "title": "A video" },
            // Untitled playlist entry: dropped.
            { "id": "PLzz", "url": "https://www.youtube.com/playlist?list=PLzz", "title": "" }
        ]
    });
    let rows = parse_ytdlp_playlist_search(&json);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].video_id, "ytpl:PLabcdefgh1234567890abcdefgh12345");
    assert_eq!(rows[0].title, "Chill Mix");
    assert_eq!(rows[0].artist, "Some Curator");
    assert_eq!(rows[0].duration, "42 tracks");
    assert_eq!(
        rows[0].youtube_playlist_id(),
        Some("PLabcdefgh1234567890abcdefgh12345")
    );
    // A playlist row must never read as a playable YouTube video.
    assert_eq!(rows[0].youtube_id(), None);
}

#[test]
fn playlist_search_entries_accept_playlist_shaped_ids_without_a_list_url() {
    let json = serde_json::json!({
        "entries": [
            {
                "id": "OLAK5uy_playlist_shaped_identifier",
                "title": "Album-shaped playlist",
                "channel": "Official Artist"
            }
        ]
    });

    let rows = parse_ytdlp_playlist_search(&json);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].video_id, "ytpl:OLAK5uy_playlist_shaped_identifier");
    assert_eq!(rows[0].artist, "Official Artist");
    assert_eq!(rows[0].duration, "");
}

#[test]
fn playlist_track_entries_skip_private_and_format_duration() {
    let track = parse_ytdlp_playlist_track(&serde_json::json!({
        "id": "abc12345678",
        "title": "A Song",
        "channel": "An Artist",
        "duration": 245.0
    }))
    .expect("a playable track");
    assert_eq!(track.video_id, "abc12345678");
    assert_eq!(track.duration, "4:05");
    assert_eq!(track.duration_secs, Some(245));
    for title in ["[Private video]", "[Deleted video]", ""] {
        assert!(
            parse_ytdlp_playlist_track(&serde_json::json!({
                "id": "abc12345678",
                "title": title,
            }))
            .is_none()
        );
    }
}

#[test]
fn playlist_track_entries_reject_non_video_ids_and_use_uploader_fallbacks() {
    assert!(
        parse_ytdlp_playlist_track(&serde_json::json!({
            "id": "PLnot-a-video-playlist-id",
            "title": "Playlist row",
        }))
        .is_none()
    );

    let track = parse_ytdlp_playlist_track(&serde_json::json!({
        "id": "abc12345678",
        "title": "No Duration Song",
        "uploader": "Uploader Artist",
        "duration": -1.0
    }))
    .expect("valid video id with title");
    assert_eq!(track.artist, "Uploader Artist");
    assert_eq!(track.duration, "");
    assert_eq!(track.duration_secs, None);
}

#[test]
fn youtube_flat_search_skips_non_video_entries() {
    let channel = serde_json::json!({
        "id": "UCfLdIEPs1tYj4ieEdJnyNyw",
        "title": "Lauv",
        "uploader": "Lauv"
    });
    assert!(parse_ytdlp_entry(SearchSource::Youtube, &channel).is_none());

    let video = serde_json::json!({
        "id": "TAfHyXrULiM",
        "title": "Paris in the Rain",
        "uploader": "Lauv",
        "duration": 198.0
    });
    let song = parse_ytdlp_entry(SearchSource::Youtube, &video).expect("video entry");
    assert_eq!(song.youtube_id(), Some("TAfHyXrULiM"));
    assert_eq!(song.duration, "3:18");
}

#[test]
fn ytdlp_flat_search_defaults_missing_youtube_metadata() {
    let video = serde_json::json!({
        "id": "TAfHyXrULiM"
    });

    let song = parse_ytdlp_entry(SearchSource::Youtube, &video).expect("video entry");

    assert_eq!(song.title, "Unknown");
    assert_eq!(song.artist, "");
    assert_eq!(song.duration, "");
}

#[test]
fn ytdlp_flat_search_maps_soundcloud_to_ytdlp_url() {
    let entry = serde_json::json!({
        "id": "tracks-123",
        "title": "Cloud Song",
        "channel": "Cloud Artist",
        "duration": 65.0,
        "webpage_url": "https://soundcloud.com/artist/cloud-song"
    });

    let song = parse_ytdlp_entry(SearchSource::SoundCloud, &entry).expect("soundcloud entry");
    assert_eq!(song.video_id, "sc:tracks-123");
    assert_eq!(song.source, SearchSource::SoundCloud);
    assert_eq!(song.title, "Cloud Song");
    assert_eq!(song.artist, "Cloud Artist");
    assert_eq!(song.duration, "1:05");
    assert_eq!(
        song.playable,
        Some(PlayableRef::YtdlpUrl {
            source: SearchSource::SoundCloud,
            url: "https://soundcloud.com/artist/cloud-song".to_owned(),
        })
    );
}

#[test]
fn ytdlp_flat_search_drops_external_entries_without_safe_url() {
    for entry in [
        serde_json::json!({
            "id": "tracks-123",
            "title": "Missing URL",
        }),
        serde_json::json!({
            "id": "tracks-123",
            "title": "Local URL",
            "url": "http://127.0.0.1/audio.mp3",
        }),
    ] {
        assert!(parse_ytdlp_entry(SearchSource::SoundCloud, &entry).is_none());
    }
}

#[test]
fn audius_track_preserves_track_id_and_app_name() {
    let entry = serde_json::json!({
        "id": "AUD123",
        "title": "Audius Song",
        "user": { "handle": "producer" },
        "duration": 121.0
    });

    let song = parse_audius_track(&entry, "yututui-test").expect("audius track");
    assert_eq!(song.video_id, "au:AUD123");
    assert_eq!(song.artist, "producer");
    assert_eq!(song.duration, "2:01");
    assert_eq!(
        song.playable,
        Some(PlayableRef::AudiusTrackId {
            id: "AUD123".to_owned(),
            app_name: "yututui-test".to_owned(),
        })
    );
}

#[test]
fn provider_parsers_apply_display_defaults_and_name_precedence() {
    let audius = parse_audius_track(
        &serde_json::json!({
            "id": "AUD999",
            "user": { "name": "Display Name", "handle": "handle-name" },
            "duration": null
        }),
        "app",
    )
    .expect("audius track with id");
    assert_eq!(audius.title, "Unknown");
    assert_eq!(audius.artist, "Display Name");
    assert_eq!(audius.duration, "");

    let radio = parse_radio_station(&serde_json::json!({
        "stationuuid": "station-min",
        "url": "https://stream.example.org/live",
    }))
    .expect("minimal radio station");
    assert_eq!(radio.title, "Unknown station");
    assert_eq!(radio.artist, "");
}

#[test]
fn provider_parsers_drop_missing_ids_and_unsafe_urls() {
    assert!(parse_audius_track(&serde_json::json!({"title": "No ID"}), "app").is_none());
    assert!(
        parse_jamendo_track(&serde_json::json!({
            "id": "jam-1",
            "audio": "file:///tmp/song.mp3"
        }))
        .is_none()
    );
    assert!(
        parse_radio_station(&serde_json::json!({
            "stationuuid": "station-1",
            "url": "http://localhost/radio"
        }))
        .is_none()
    );
}

#[test]
fn jamendo_track_maps_public_audio_url() {
    let entry = serde_json::json!({
        "id": "jam-42",
        "name": "Jam Song",
        "artist_name": "Jam Artist",
        "duration": 242.0,
        "audio": "https://cdn.jamendo.com/audio.mp3"
    });

    let song = parse_jamendo_track(&entry).expect("jamendo track");
    assert_eq!(song.video_id, "ja:jam-42");
    assert_eq!(song.title, "Jam Song");
    assert_eq!(song.duration, "4:02");
    assert_eq!(
        song.playable,
        Some(PlayableRef::JamendoTrackId {
            id: "jam-42".to_owned(),
            url: "https://cdn.jamendo.com/audio.mp3".to_owned(),
        })
    );
}

#[test]
fn radio_station_builds_artist_from_country_codec_and_bitrate() {
    let entry = serde_json::json!({
        "stationuuid": "station-42",
        "name": "Night Radio",
        "url_resolved": "https://stream.example.org/live",
        "codec": "MP3",
        "bitrate": 128,
        "country": "KR"
    });

    let song = parse_radio_station(&entry).expect("radio station");
    assert_eq!(song.video_id, "rad:station-42");
    assert_eq!(song.title, "Night Radio");
    assert_eq!(song.artist, "KR / MP3 / 128k");
    assert!(song.is_radio_station());
    assert_eq!(
        song.playable,
        Some(PlayableRef::RadioStream {
            url: "https://stream.example.org/live".to_owned(),
        })
    );
}

#[test]
fn archive_file_url_escapes_path_segments() {
    assert_eq!(
        archive_file_url("collection id", "Disc 1/song name.flac"),
        "https://archive.org/download/collection%20id/Disc%201%2Fsong%20name.flac"
    );
}

#[test]
fn streaming_queries_expand_title_artist_seed() {
    let queries = streaming_queries("Song — Artist", StreamingMode::Balanced);
    assert_eq!(
        queries,
        vec![
            "Song — Artist radio",
            "Artist radio",
            "Artist songs",
            "Artist similar songs",
            "Song Artist",
        ]
    );
    // No "mix" queries — they pull long compilations.
    assert!(!queries.iter().any(|q| q.contains("mix")));
}

#[test]
fn streaming_queries_handle_plain_seed() {
    let queries = streaming_queries("lo-fi beats", StreamingMode::Balanced);
    assert_eq!(
        queries,
        vec![
            "lo-fi beats radio",
            "lo-fi beats songs",
            "lo-fi beats similar songs",
        ]
    );
    assert!(!queries.iter().any(|q| q.contains("mix")));
}

#[test]
fn streaming_queries_are_mode_specific() {
    let focused = streaming_queries("Song — Artist", StreamingMode::Focused);
    assert_eq!(focused[0], "Song Artist official audio");
    assert!(focused.iter().any(|q| q.contains("official video")));

    let discovery = streaming_queries("Song — Artist", StreamingMode::Discovery);
    assert_eq!(discovery[0], "Artist similar songs");
    assert!(discovery.iter().any(|q| q.contains("deep cuts")));
    assert!(!discovery.iter().any(|q| q.contains(" mix")));
}

#[test]
fn streaming_queries_use_mode_specific_empty_seed_defaults() {
    assert_eq!(
        streaming_queries("   ", StreamingMode::Focused),
        vec![
            "popular songs official audio",
            "popular music official video"
        ]
    );
    assert_eq!(
        streaming_queries("", StreamingMode::Discovery),
        vec![
            "new music similar songs",
            "popular music radio",
            "deep cuts songs",
        ]
    );
}

#[test]
fn preflight_metadata_rejects_live_and_long_non_music() {
    let cfg = StreamingConfig::default();
    let mut meta = YtdlpVideoMeta {
        title: "Episode 12 interview".to_owned(),
        channel: "Music Podcast".to_owned(),
        duration_secs: Some(1_800),
        live_status: None,
        is_live: None,
        was_live: None,
        media_type: None,
        description: Some("conversation and commentary".to_owned()),
        ..YtdlpVideoMeta::default()
    };
    assert!(reject_enriched(&meta, StreamingMode::Balanced, &cfg));

    meta = YtdlpVideoMeta {
        title: "Artist - Song".to_owned(),
        channel: "Artist".to_owned(),
        duration_secs: Some(180),
        live_status: Some("is_live".to_owned()),
        is_live: Some(true),
        was_live: None,
        media_type: None,
        description: None,
        ..YtdlpVideoMeta::default()
    };
    assert!(reject_enriched(&meta, StreamingMode::Discovery, &cfg));
}

#[test]
fn preflight_metadata_keeps_trusted_music_track() {
    let cfg = StreamingConfig::default();
    let meta = YtdlpVideoMeta {
        title: "Artist - Song (Official Audio)".to_owned(),
        channel: "Artist - Topic".to_owned(),
        duration_secs: Some(210),
        live_status: None,
        is_live: None,
        was_live: None,
        media_type: None,
        description: None,
        ..YtdlpVideoMeta::default()
    };
    assert!(!reject_enriched(&meta, StreamingMode::Focused, &cfg));
}

#[test]
fn preflight_metadata_rejects_playlist_rows_and_duration_edges() {
    let mut cfg = StreamingConfig {
        min_duration_secs: 60,
        max_duration_secs: 900,
        ..StreamingConfig::default()
    };
    let mut meta = YtdlpVideoMeta {
        title: "Artist - Song (Official Audio)".to_owned(),
        channel: "Artist - Topic".to_owned(),
        duration_secs: Some(59),
        live_status: None,
        is_live: None,
        was_live: None,
        media_type: None,
        description: None,
        ..YtdlpVideoMeta::default()
    };
    assert!(reject_enriched(&meta, StreamingMode::Balanced, &cfg));

    meta.duration_secs = Some(901);
    assert!(reject_enriched(&meta, StreamingMode::Balanced, &cfg));

    meta.duration_secs = Some(240);
    meta.media_type = Some("playlist".to_owned());
    assert!(reject_enriched(&meta, StreamingMode::Discovery, &cfg));

    meta.media_type = None;
    cfg.max_duration_secs = 20 * 60;
    meta.duration_secs = Some(13 * 60);
    assert!(
        reject_enriched(&meta, StreamingMode::Balanced, &cfg),
        "balanced mode has a 12 minute mode cap even when config allows longer tracks"
    );
    assert!(!reject_enriched(&meta, StreamingMode::Discovery, &cfg));
}

#[test]
fn preflight_metadata_rejects_live_replay_in_balanced_but_not_discovery() {
    let cfg = StreamingConfig::default();
    let meta = YtdlpVideoMeta {
        title: "Artist - Song (Live at Seoul)".to_owned(),
        channel: "Artist".to_owned(),
        duration_secs: Some(260),
        live_status: None,
        is_live: None,
        was_live: Some(true),
        media_type: None,
        description: Some("official live performance".to_owned()),
        ..YtdlpVideoMeta::default()
    };
    assert!(reject_enriched(&meta, StreamingMode::Balanced, &cfg));
    assert!(!reject_enriched(&meta, StreamingMode::Discovery, &cfg));
}

#[tokio::test]
async fn streaming_preflight_dedupes_and_tops_up_from_fallback_without_metadata_lookup() {
    let a = Song::from_search(
        "TAfHyXrULiM",
        "Artist - Song (Official Audio)",
        "Artist - Topic",
        "3:18",
        None,
    );
    let b = Song::from_search(
        "dQw4w9WgXcQ",
        "Second Artist - Single (Official Audio)",
        "Second Artist - Topic",
        "3:33",
        None,
    );

    let out = preflight_streaming_picks(
        vec![a.clone(), a],
        vec![b.clone()],
        StreamingMode::Focused,
        &StreamingConfig::default(),
    )
    .await;

    assert_eq!(
        out.iter()
            .map(|song| song.video_id.as_str())
            .collect::<Vec<_>>(),
        vec!["TAfHyXrULiM", "dQw4w9WgXcQ"]
    );
}

#[test]
fn artist_row_parts_builds_a_ytar_row() {
    let song = artist_row_parts(
        "Some Artist".to_owned(),
        Some("1M subscribers".to_owned()),
        "UCabc123",
    );
    assert_eq!(song.video_id, "ytar:UCabc123");
    assert_eq!(song.youtube_artist_id(), Some("UCabc123"));
    assert_eq!(song.youtube_playlist_id(), None);
    assert_eq!(song.title, "Some Artist");
    assert!(song.artist.is_empty());
    // The subscriber count rides in the duration slot (rows render it in parentheses).
    assert_eq!(song.duration, "1M subscribers");

    let bare = artist_row_parts("X".to_owned(), None, "UCx");
    assert!(bare.duration.is_empty());
}
