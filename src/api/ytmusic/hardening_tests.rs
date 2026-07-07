use super::*;

#[test]
fn playlist_track_entries_reject_non_video_ids() {
    for id in ["UCfLdIEPs1tYj4ieEdJnyNyw", "PL123456789012345", "too-short"] {
        assert!(
            parse_ytdlp_playlist_track(&serde_json::json!({
                "id": id,
                "title": "A Song",
            }))
            .is_none(),
            "{id} should not be accepted as a playlist track video id"
        );
    }
}

#[test]
fn non_youtube_flat_search_rejects_invalid_playable_urls() {
    let invalid = serde_json::json!({
        "id": "track1",
        "title": "Local File Trap",
        "uploader": "Bad",
        "webpage_url": "file:///etc/passwd",
    });
    assert!(parse_ytdlp_entry(SearchSource::SoundCloud, &invalid).is_none());

    let valid = serde_json::json!({
        "id": "track2",
        "title": "Public Stream",
        "uploader": "OK",
        "webpage_url": "https://soundcloud.com/artist/track",
    });
    assert_eq!(
        parse_ytdlp_entry(SearchSource::SoundCloud, &valid)
            .expect("valid URL")
            .watch_url(),
        "https://soundcloud.com/artist/track"
    );
}

#[test]
fn direct_provider_parsers_reject_private_or_non_http_urls() {
    assert!(
        parse_jamendo_track(&serde_json::json!({
            "id": "j1",
            "name": "Jam",
            "audio": "http://127.0.0.1/audio.mp3",
        }))
        .is_none()
    );
    assert!(
        parse_radio_station(&serde_json::json!({
            "stationuuid": "r1",
            "name": "Local",
            "url_resolved": "smb://server/share",
        }))
        .is_none()
    );
    assert_eq!(
        parse_radio_station(&serde_json::json!({
            "stationuuid": "r2",
            "name": "Public",
            "url_resolved": "https://radio.example/stream",
        }))
        .expect("valid radio URL")
        .watch_url(),
        "https://radio.example/stream"
    );
}
