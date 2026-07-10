use super::*;

fn input() -> TrackInput {
    TrackInput {
        title: "Song Title (feat. Guest)".to_owned(),
        artists: vec!["Primary".to_owned(), "Featured".to_owned()],
        album_artists: Vec::new(),
        album: Some("Album Name".to_owned()),
        album_id: None,
        album_uri: None,
        album_release_date: Some("2024-05-01".to_owned()),
        album_release_date_precision: None,
        album_total_tracks: None,
        album_type: None,
        album_art_url: None,
        disc_number: None,
        track_number: None,
        duration_secs: Some(180),
        isrc: None,
        explicit: None,
        source_url: None,
        source_key: "spotify:track:test".to_owned(),
        known_video_id: None,
    }
}

#[test]
fn public_fallback_is_backend_scoped_and_quality_ordered() {
    let input = input();
    assert_eq!(
        ytm_fallback_query_plan(&input),
        vec![
            "Primary Song Title".to_owned(),
            "Primary Song Title official audio".to_owned(),
            "Song Title official music video".to_owned(),
        ]
    );
}

#[test]
fn public_fallback_uses_album_context_without_an_artist() {
    let mut input = input();
    input.artists.clear();

    assert_eq!(
        ytm_fallback_query_plan(&input),
        vec![
            "Song Title Album Name".to_owned(),
            "Song Title official audio".to_owned(),
            "Song Title official music video".to_owned(),
        ]
    );
}

#[test]
fn public_fallback_without_artist_or_album_still_uses_three_distinct_intents() {
    let mut input = input();
    input.artists.clear();
    input.album = None;

    assert_eq!(
        ytm_fallback_query_plan(&input),
        vec![
            "Song Title".to_owned(),
            "Song Title official audio".to_owned(),
            "Song Title official music video".to_owned(),
        ]
    );
}

#[tokio::test]
async fn anonymous_catalog_miss_skips_pacing_and_provider_work() {
    let state = SharedYtmMatchState::new(Pacing::new(Duration::ZERO), 1, 1, 1);
    let result = shared_catalog_songs(
        &YtMusicApi::Anonymous,
        "Primary Song Title",
        &SearchConfig::default(),
        &state,
    )
    .await
    .expect("anonymous catalog capability check");

    assert!(matches!(result, SharedCatalogSongs::Unavailable));
    assert_eq!(state.diagnostics().await.catalog_searches, 0);
}

#[tokio::test]
async fn metadata_cache_miss_releases_guard_before_single_flight_recheck() {
    let state = SharedYtmMatchState::new(Pacing::new(Duration::ZERO), 1, 1, 1);
    let key = "missing-video";

    assert!(cached_video_metadata(&state, key).await.is_none());
    let lock = video_lock(&state, key).await;
    let _single_flight = lock.lock().await;
    let second_lookup = tokio::time::timeout(
        Duration::from_millis(50),
        cached_video_metadata(&state, key),
    )
    .await
    .expect("cache miss must not retain video_memo across the miss arm");
    assert!(second_lookup.is_none());
}

#[test]
fn metadata_preflight_requires_identity_and_audio_evidence() {
    let mut meta = YtdlpVideoMeta {
        title: "Song Title".to_owned(),
        channel: "Artist".to_owned(),
        ..YtdlpVideoMeta::default()
    };
    assert!(!usable_transfer_preflight(&meta));

    meta.audio.available_audio_formats = Some(0);
    assert!(
        usable_transfer_preflight(&meta),
        "a known zero audio count is usable evidence and is rejected by the scoring gate"
    );
    meta.channel.clear();
    assert!(!usable_transfer_preflight(&meta));
}

#[test]
fn failed_metadata_preflight_is_not_retried_in_the_same_track() {
    let song = Song::remote("video", "Song Title", "Artist", "3:00");
    let mut candidate =
        MatchCandidate::from_song_with_kind(&song, CandidateSourceKind::YoutubeVideoSearch);
    candidate
        .preflight_reason_codes
        .push("metadata_preflight_failed".to_owned());

    assert!(!needs_transfer_preflight(
        &candidate,
        &MatchConfig::default()
    ));
}

#[test]
fn allow_user_videos_still_requires_finalist_safety_preflight() {
    let song = Song::remote("video", "Song Title", "Artist", "3:00");
    let candidate =
        MatchCandidate::from_song_with_kind(&song, CandidateSourceKind::YoutubeVideoSearch);
    assert!(!needs_transfer_preflight(
        &candidate,
        &MatchConfig::default()
    ));
    assert!(needs_transfer_preflight(
        &candidate,
        &MatchConfig {
            allow_user_videos: true,
            ..MatchConfig::default()
        }
    ));
}

#[test]
fn metadata_native_title_must_credit_the_verified_channel() {
    let mut meta = YtdlpVideoMeta {
        title: "あいみょん - マリーゴールド【OFFICIAL MUSIC VIDEO】".to_owned(),
        channel: "あいみょん".to_owned(),
        ..YtdlpVideoMeta::default()
    };
    assert!(metadata_title_credits_channel(&meta));

    meta.channel = "Verified Broadcaster".to_owned();
    assert!(!metadata_title_credits_channel(&meta));
}

#[test]
fn translated_search_title_and_native_metadata_title_both_contribute_identity() {
    let mut input = input();
    input.title = "満月の夜なら".to_owned();
    input.artists = vec!["Aimyon".to_owned()];
    input.duration_secs = Some(205);

    let song = Song::remote(
        "OVKKtwDReEA",
        "Aimyon - Only Under the Full Moon [OFFICIAL MUSIC VIDEO]",
        "あいみょん",
        "3:42",
    );
    let mut candidate =
        MatchCandidate::from_song_with_kind(&song, CandidateSourceKind::YoutubeVideoSearch);
    let cfg = MatchConfig {
        allow_user_videos: true,
        ..MatchConfig::default()
    };
    let flat_score = score_candidate_breakdown_with_config(&input, &candidate, &cfg).total;
    assert!(flat_score < cfg.ambiguous_floor);
    assert_eq!(
        transfer_preflight_rank(&input, &candidate, &cfg),
        Some(flat_score),
        "official-like localized rows must reach canonical metadata"
    );
    candidate.preflighted = true;
    let mut meta = YtdlpVideoMeta {
        title: "あいみょん - 満月の夜なら 【OFFICIAL MUSIC VIDEO】".to_owned(),
        channel: "あいみょん".to_owned(),
        channel_is_verified: Some(true),
        duration_secs: Some(222),
        availability: Some("public".to_owned()),
        live_status: Some("not_live".to_owned()),
        ..YtdlpVideoMeta::default()
    };
    meta.audio.available_audio_formats = Some(5);

    apply_transfer_preflight(&input, &mut candidate, &meta);
    let breakdown = score_candidate_breakdown_with_config(&input, &candidate, &cfg);

    assert_eq!(
        candidate.metadata_title.as_deref(),
        Some(meta.title.as_str())
    );
    assert!(breakdown.title >= 0.98, "native title: {breakdown:?}");
    assert!(
        breakdown.artist >= 0.95,
        "translated artist credit: {breakdown:?}"
    );
    assert!(
        !breakdown.accept_blocked,
        "verified official video: {breakdown:?}"
    );
    assert!(matches!(
        best_outcome(&input, &[candidate], &cfg),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn preflight_summary_distinguishes_partial_and_terminal_failure() {
    assert_eq!(PreflightSummary::default().status(), "success");
    assert_eq!(
        PreflightSummary {
            attempts: 2,
            successes: 1,
            failures: 1,
            first_error_kind: Some("http_403".to_owned()),
        }
        .status(),
        "partial"
    );
    assert_eq!(
        PreflightSummary {
            attempts: 1,
            failures: 1,
            first_error_kind: Some("timeout".to_owned()),
            ..PreflightSummary::default()
        }
        .status(),
        "error"
    );
}

#[test]
fn provider_failures_are_not_successful_empty_searches() {
    let error = anyhow::anyhow!("yt-dlp search exited with status 1 (HTTP Error 403: Forbidden)");
    assert!(!video_search_error_is_soft(&error));
}

#[test]
fn public_search_retries_only_transient_provider_rejections() {
    for (detail, kind) in [
        ("HTTP Error 403: Forbidden", "http_403"),
        ("HTTP Error 429: Too Many Requests", "http_429"),
        ("Unable to download API page: connection reset", "network"),
    ] {
        let error = anyhow::anyhow!(detail);
        assert_eq!(public_video_search_error_kind(&error), kind);
        assert!(public_video_search_is_retryable(&error));
    }

    for (detail, kind) in [
        ("yt-dlp search timed out", "timeout"),
        ("yt-dlp returned invalid JSON", "invalid_response"),
        ("yt-dlp executable not found", "tool_missing"),
    ] {
        let error = anyhow::anyhow!(detail);
        assert_eq!(public_video_search_error_kind(&error), kind);
        assert!(!public_video_search_is_retryable(&error));
    }
}
