use super::ytm_retrieval::video_search_error_is_soft;
use super::*;

fn input(title: &str, artists: &[&str], album: Option<&str>, dur: Option<u32>) -> TrackInput {
    TrackInput {
        title: title.to_owned(),
        artists: artists.iter().map(|s| (*s).to_owned()).collect(),
        album_artists: Vec::new(),
        album: album.map(str::to_owned),
        album_id: None,
        album_uri: None,
        album_release_date: None,
        album_release_date_precision: None,
        album_total_tracks: None,
        album_type: None,
        album_art_url: None,
        disc_number: None,
        track_number: None,
        duration_secs: dur,
        isrc: None,
        explicit: None,
        source_url: None,
        source_key: "src".to_owned(),
        known_video_id: None,
    }
}

fn cand(title: &str, artist: &str, album: Option<&str>, dur: Option<u32>) -> MatchCandidate {
    MatchCandidate {
        key: format!("key-{title}"),
        title: title.to_owned(),
        artist: artist.to_owned(),
        album: album.map(str::to_owned),
        duration_secs: dur,
        track_number: None,
        release_year: None,
        explicit: None,
        source_kind: CandidateSourceKind::YtmCatalogSong,
        channel: Some(artist.to_owned()),
        channel_id: None,
        channel_verified: None,
        availability: None,
        has_audio_format: None,
        max_audio_bitrate_kbps: None,
        isrc: None,
        metadata_title: None,
        preflighted: false,
        preflight_reject_reason: None,
        preflight_reason_codes: Vec::new(),
    }
}

#[test]
fn normalize_goldens() {
    assert_eq!(normalize("ＴＴ"), "tt"); // NFKC fullwidth fold
    assert_eq!(normalize("Don’t Stop"), "don t stop");
    assert_eq!(normalize("R&B Mix"), "r and b mix");
    assert_eq!(normalize("사건의 지평선"), "사건의 지평선"); // CJK untouched
    assert_eq!(
        normalize_stripped("Song Title (feat. Someone)"),
        "song title"
    );
    assert_eq!(normalize_stripped("Track - 2011 Remaster"), "track");
    assert_eq!(
        normalize_stripped("Album Cut [Deluxe Edition]"),
        "album cut"
    );
    // Identity-changing markers survive.
    assert_eq!(normalize_stripped("Song (Live)"), "song live");
    assert_eq!(
        normalize_stripped("Love Story (Taylor's Version)"),
        "love story taylor s version"
    );
}

#[test]
fn spotify_input_preserves_library_metadata() {
    let track = SpotifyTrack {
        id: Some("sp-track".to_owned()),
        uri: "spotify:track:sp-track".to_owned(),
        spotify_url: Some("https://open.spotify.com/track/sp-track".to_owned()),
        name: "Song".to_owned(),
        artists: vec!["Artist".to_owned()],
        artist_ids: vec!["sp-artist".to_owned()],
        album_artists: vec!["Album Artist".to_owned()],
        album_artist_ids: vec!["sp-album-artist".to_owned()],
        album: "Album".to_owned(),
        album_id: Some("sp-album".to_owned()),
        album_uri: Some("spotify:album:sp-album".to_owned()),
        album_url: Some("https://open.spotify.com/album/sp-album".to_owned()),
        album_type: Some("album".to_owned()),
        album_total_tracks: Some(10),
        album_release_date: Some("2026-07-01".to_owned()),
        album_release_date_precision: Some("day".to_owned()),
        album_images: vec![crate::spotify::models::SpotifyImage {
            url: "https://i.scdn.co/image/cover640".to_owned(),
            width: Some(640),
            height: Some(640),
        }],
        duration_ms: 123_456,
        disc_number: Some(1),
        track_number: Some(4),
        isrc: Some("ISRC1".to_owned()),
        explicit: true,
        added_at: Some("2026-07-02T00:00:00Z".to_owned()),
        is_playable: Some(true),
        restriction_reason: None,
    };

    let input = TrackInput::from_spotify(&track);

    assert_eq!(input.title, "Song");
    assert_eq!(input.artists, vec!["Artist".to_owned()]);
    assert_eq!(input.album_artists, vec!["Album Artist".to_owned()]);
    assert_eq!(input.album.as_deref(), Some("Album"));
    assert_eq!(input.album_id.as_deref(), Some("sp-album"));
    assert_eq!(input.album_uri.as_deref(), Some("spotify:album:sp-album"));
    assert_eq!(input.album_release_date.as_deref(), Some("2026-07-01"));
    assert_eq!(input.album_release_date_precision.as_deref(), Some("day"));
    assert_eq!(input.album_total_tracks, Some(10));
    assert_eq!(input.album_type.as_deref(), Some("album"));
    assert_eq!(
        input.album_art_url.as_deref(),
        Some("https://i.scdn.co/image/cover640")
    );
    assert_eq!(input.disc_number, Some(1));
    assert_eq!(input.track_number, Some(4));
    assert_eq!(input.duration_secs, Some(123));
    assert_eq!(input.isrc.as_deref(), Some("ISRC1"));
    assert_eq!(input.explicit, Some(true));
    assert_eq!(
        input.source_url.as_deref(),
        Some("https://open.spotify.com/track/sp-track")
    );
    assert_eq!(input.source_key, "spotify:track:sp-track");
}

#[test]
fn song_input_preserves_catalog_metadata() {
    let song = Song::from_search(
        "dQw4w9WgXcQ",
        "Song",
        "Artist",
        "3:03",
        Some("Album".to_owned()),
    )
    .with_catalog_metadata(
        Some("Album Artist".to_owned()),
        Some(1),
        Some(4),
        Some("ISRC123".to_owned()),
        Some("spotify:track:abc".to_owned()),
        Some("https://open.spotify.com/track/abc".to_owned()),
    )
    .with_import_metadata(crate::api::SongImportMetadata {
        artists: vec!["Artist".to_owned(), "Guest".to_owned()],
        album_artists: vec!["Album Artist".to_owned(), "Label Ensemble".to_owned()],
        album_art_url: Some("https://i.scdn.co/image/song-cover".to_owned()),
        ..Default::default()
    });

    let input = TrackInput::from_song(&song);

    assert_eq!(input.title, "Song");
    assert_eq!(input.artists, vec!["Artist".to_owned(), "Guest".to_owned()]);
    assert_eq!(
        input.album_artists,
        vec!["Album Artist".to_owned(), "Label Ensemble".to_owned()]
    );
    assert_eq!(input.album.as_deref(), Some("Album"));
    assert_eq!(
        input.album_art_url.as_deref(),
        Some("https://i.scdn.co/image/song-cover")
    );
    assert_eq!(input.disc_number, Some(1));
    assert_eq!(input.track_number, Some(4));
    assert_eq!(input.duration_secs, Some(183));
    assert_eq!(input.isrc.as_deref(), Some("ISRC123"));
    assert_eq!(input.source_key, "spotify:track:abc");
    assert_eq!(
        input.source_url.as_deref(),
        Some("https://open.spotify.com/track/abc")
    );
    assert_eq!(input.known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
}

#[test]
fn ytm_query_plan_adds_all_artists_album_and_title_fallbacks() {
    let mut input = input(
        "Song Title (feat. Guest)",
        &["Primary", "Featured"],
        Some("Album Name"),
        Some(180),
    );
    input.album_artists = vec!["Album Artist".to_owned()];
    input.album_release_date = Some("2024-05-01".to_owned());

    assert_eq!(
        ytm_query_plan(&input),
        vec![
            "Primary Song Title".to_owned(),
            "Primary Featured Song Title".to_owned(),
            "Primary Featured Song Title (feat. Guest)".to_owned(),
            "Album Artist Song Title".to_owned(),
            "Primary Song Title Album Name".to_owned(),
            "Song Title Album Name".to_owned(),
            "Primary Song Title 2024".to_owned(),
            "Primary Featured Song Title 2024".to_owned(),
            "Primary Song Title official audio".to_owned(),
            "Primary Song Title topic".to_owned(),
            "Song Title (feat. Guest)".to_owned(),
            "Song Title".to_owned(),
        ]
    );
}

#[test]
fn ytm_query_plan_dedupes_empty_and_repeated_variants() {
    let mut input = input("Song", &["Artist", "Artist"], Some("Song"), None);
    input.artists.push(" ".to_owned());

    assert_eq!(
        ytm_query_plan(&input),
        vec![
            "Artist Song".to_owned(),
            "Artist Artist Song".to_owned(),
            "Artist Song official audio".to_owned(),
            "Artist Song topic".to_owned(),
            "Song".to_owned(),
        ]
    );
}

#[test]
fn ytm_query_plan_handles_missing_artists() {
    let input = input("Song", &[], Some("Album"), None);

    assert_eq!(
        ytm_query_plan(&input),
        vec!["Song Album".to_owned(), "Song".to_owned()]
    );
}

#[test]
fn ytm_catalog_plan_uses_only_fast_primary_queries_before_fallbacks() {
    let mut input = input(
        "Song Title (feat. Guest)",
        &["Primary", "Featured"],
        Some("Album Name"),
        Some(180),
    );
    input.album_artists = vec!["Album Artist".to_owned()];
    input.album_release_date = Some("2024-05-01".to_owned());

    assert_eq!(
        ytm_catalog_query_plan(&input),
        vec![
            "Primary Song Title".to_owned(),
            "Primary Song Title (feat. Guest)".to_owned(),
            "Song Title Primary".to_owned(),
        ]
    );
    let fallback = ytm_fallback_query_plan(&input);
    assert_eq!(
        fallback,
        vec![
            "Primary Song Title".to_owned(),
            "Primary Song Title official audio".to_owned(),
            "Song Title official music video".to_owned(),
        ],
        "public search is a separate backend, so its primary query must not be suppressed by a catalog attempt"
    );
}

#[test]
fn video_search_provider_errors_remain_hard_at_matching_boundary() {
    let err = anyhow::anyhow!(
        "yt-dlp search exited with status exit status: 1 (ERROR: query \"Aimyon Marigold topic\" page 1: Unable to download API page: HTTP Error 403: Forbidden)"
    );
    assert!(!video_search_error_is_soft(&err));
}

#[test]
fn plain_track_does_not_auto_accept_instrumental_candidate() {
    let i = input("Song", &["Artist"], None, Some(180));
    let c = cand("Song (Instrumental)", "Artist", None, Some(180));
    let cfg = MatchConfig::default();

    let out = best_outcome(&i, &[c], &cfg);

    assert!(!matches!(out, MatchOutcome::Matched { .. }));
    let breakdown =
        score_candidate_breakdown(&i, &cand("Song (Instrumental)", "Artist", None, Some(180)));
    assert!(breakdown.accept_blocked);
    assert!(
        breakdown
            .reason_codes
            .contains(&"instrumental_mismatch".to_owned())
    );
}

#[test]
fn karaoke_candidate_is_hard_rejected_for_plain_track() {
    let i = input("Song", &["Artist"], None, Some(180));
    let c = cand("Song (Karaoke Version)", "Artist", None, Some(180));

    assert!(matches!(
        best_outcome(&i, &[c], &MatchConfig::default()),
        MatchOutcome::NotFound
    ));
}

#[test]
fn instrumental_source_can_match_instrumental_candidate() {
    let i = input("Song (Instrumental)", &["Artist"], None, Some(180));
    let c = cand("Song - Instrumental", "Artist", None, Some(180));

    assert!(matches!(
        best_outcome(&i, &[c], &MatchConfig::default()),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn duplicate_recording_representations_do_not_consume_cluster_margin() {
    let i = input("Song", &["Artist"], None, Some(180));
    let a = cand("Song", "Artist", None, Some(180));
    let b = cand("Song", "Artist", Some("Other Album"), Some(180));
    let cfg = MatchConfig::default();

    assert!(matches!(
        best_outcome(&i, &[a, b], &cfg),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn ytm_catalog_candidate_beats_plain_video_when_close() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut catalog = cand("Song", "Artist", Some("Album"), Some(180));
    catalog.key = "catalog".to_owned();
    catalog.source_kind = CandidateSourceKind::YtmCatalogSong;
    let mut video = cand("Song", "Artist", Some("Album"), Some(180));
    video.key = "video".to_owned();
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Random Uploads".to_owned());

    match best_outcome(&i, &[video, catalog], &MatchConfig::default()) {
        MatchOutcome::Matched { key, .. } => assert_eq!(key, "catalog"),
        other => panic!("expected matched catalog candidate, got {other:?}"),
    }
}

#[test]
fn blocked_top_candidate_does_not_hide_safe_official_match() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut blocked = cand("Song", "Artist", Some("Album"), Some(180));
    blocked.key = "blocked-generic".to_owned();
    blocked.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    blocked.channel = Some("Random Uploads".to_owned());

    let mut official = cand("Song", "Artist Official", None, Some(180));
    official.key = "safe-official".to_owned();
    official.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    official.channel = Some("Artist Official".to_owned());
    official.preflighted = true;
    official.has_audio_format = Some(true);

    let blocked_score = score_candidate_breakdown(&i, &blocked);
    let official_score = score_candidate_breakdown(&i, &official);
    assert!(blocked_score.accept_blocked);
    assert!(blocked_score.total > official_score.total);

    match best_outcome(&i, &[blocked, official], &MatchConfig::default()) {
        MatchOutcome::Matched { key, .. } => assert_eq!(key, "safe-official"),
        other => panic!("expected safe official match, got {other:?}"),
    }
}

#[test]
fn blocked_runner_up_does_not_consume_safe_acceptance_margin() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut catalog = cand("Song", "Artist", None, Some(180));
    catalog.key = "safe-catalog".to_owned();
    catalog.source_kind = CandidateSourceKind::YtmCatalogSong;

    let mut blocked = cand("Song", "Artist", Some("Album"), Some(180));
    blocked.key = "blocked-generic".to_owned();
    blocked.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    blocked.channel = Some("Random Uploads".to_owned());

    let catalog_score = score_candidate_breakdown(&i, &catalog);
    let blocked_score = score_candidate_breakdown(&i, &blocked);
    assert!(blocked_score.accept_blocked);
    assert!(
        (catalog_score.total - blocked_score.total).abs() < MatchConfig::default().accept_margin
    );

    match best_outcome(&i, &[blocked, catalog], &MatchConfig::default()) {
        MatchOutcome::Matched {
            key,
            score_breakdown: Some(score),
            ..
        } => {
            assert_eq!(key, "safe-catalog");
            assert!(score.reason_codes.contains(&"policy_safe_cache".to_owned()));
        }
        other => panic!("expected catalog match with blocked runner-up ignored, got {other:?}"),
    }
}

#[test]
fn generic_youtube_high_score_without_official_signal_needs_review() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut video = cand("Song", "Artist", Some("Album"), Some(180));
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Random Uploads".to_owned());

    match best_outcome(&i, &[video], &MatchConfig::default()) {
        MatchOutcome::Ambiguous { candidates } => {
            let score = candidates[0].score_breakdown.as_ref().unwrap();
            assert!(score.accept_blocked);
            assert!(
                score
                    .reason_codes
                    .contains(&"unverified_youtube_upload".to_owned())
            );
            assert_eq!(score.quality_tier, "unverified_upload");
        }
        other => panic!("expected ambiguous generic upload, got {other:?}"),
    }
}

#[test]
fn generic_youtube_can_match_when_user_videos_are_allowed() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut video = cand("Song", "Artist", Some("Album"), Some(180));
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Random Uploads".to_owned());
    let cfg = MatchConfig {
        allow_user_videos: true,
        ..MatchConfig::default()
    };

    match best_outcome(&i, &[video], &cfg) {
        MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } => {
            assert!(!score.accept_blocked);
            assert_eq!(score.confidence_tier, "review");
        }
        other => panic!("expected allowed generic upload match, got {other:?}"),
    }
}

#[test]
fn official_topic_youtube_candidate_can_auto_accept() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut video = cand("Song", "Artist", Some("Album"), Some(180));
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Artist - Topic".to_owned());

    match best_outcome(&i, &[video], &MatchConfig::default()) {
        MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } => {
            assert!(!score.accept_blocked);
            assert_eq!(score.quality_tier, "trusted_official");
        }
        other => panic!("expected matched Topic candidate, got {other:?}"),
    }
}

#[test]
fn public_official_title_requires_finalist_metadata_before_auto_accept() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut video = cand("Song (Official Audio)", "Artist", Some("Album"), Some(180));
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Artist".to_owned());

    let before = score_candidate_breakdown(&i, &video);
    assert!(before.accept_blocked);
    assert!(
        before
            .reason_codes
            .contains(&"unverified_youtube_upload".to_owned())
    );

    video.preflighted = true;
    video.has_audio_format = Some(true);
    match best_outcome(&i, &[video], &MatchConfig::default()) {
        MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } => {
            assert!(!score.accept_blocked);
            assert!(
                score
                    .reason_codes
                    .contains(&"playable_audio_format".to_owned())
            );
        }
        other => panic!("expected preflighted official audio match, got {other:?}"),
    }
}

#[test]
fn failed_metadata_preflight_never_auto_accepts_even_when_user_videos_are_allowed() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut video = cand(
        "Artist - Song [OFFICIAL MUSIC VIDEO]",
        "Artist",
        Some("Album"),
        Some(180),
    );
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Artist".to_owned());
    video
        .preflight_reason_codes
        .push("metadata_preflight_failed".to_owned());
    let cfg = MatchConfig {
        allow_user_videos: true,
        ..MatchConfig::default()
    };

    let breakdown = score_candidate_breakdown_with_config(&i, &video, &cfg);
    assert!(breakdown.accept_blocked);
    assert!(
        breakdown
            .reason_codes
            .contains(&"metadata_preflight_failed".to_owned())
    );
    assert!(!matches!(
        best_outcome(&i, &[video], &cfg),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn verified_official_title_credit_bridges_cross_script_channel_artist() {
    let i = input("Marigold", &["Aimyon"], None, Some(306));
    let mut video = cand(
        "Aimyon - Marigold [OFFICIAL MUSIC VIDEO]",
        "あいみょん",
        None,
        Some(322),
    );
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("あいみょん".to_owned());
    video.preflighted = true;
    video.channel_verified = Some(true);
    video.channel_id = Some("UCQVhrypJhw1HxuRV4gX6hoQ".to_owned());
    video.has_audio_format = Some(true);
    video
        .preflight_reason_codes
        .push("metadata_title_channel_corroborated".to_owned());

    let breakdown = score_candidate_breakdown(&i, &video);
    assert_eq!(breakdown.artist, 1.0);
    assert!(
        breakdown
            .reason_codes
            .contains(&"corroborated_title_artist_credit".to_owned())
    );
    assert!(!breakdown.accept_blocked);
    assert!(matches!(
        best_outcome(&i, &[video], &MatchConfig::default()),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn unverified_or_spoofed_title_credit_does_not_replace_channel_artist() {
    let i = input("Marigold", &["Aimyon"], None, Some(306));
    let user_video_config = MatchConfig {
        allow_user_videos: true,
        ..MatchConfig::default()
    };

    let mut unverified = cand(
        "Aimyon - Marigold [OFFICIAL MUSIC VIDEO]",
        "ファン投稿",
        None,
        Some(322),
    );
    unverified.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    unverified.channel = Some("ファン投稿".to_owned());
    unverified.preflighted = true;
    unverified.channel_verified = Some(false);
    unverified.has_audio_format = Some(true);

    let unverified_score =
        score_candidate_breakdown_with_config(&i, &unverified, &user_video_config);
    assert!(unverified_score.artist < 0.5);
    assert!(
        !unverified_score
            .reason_codes
            .contains(&"corroborated_title_artist_credit".to_owned())
    );
    assert!(!matches!(
        best_outcome(&i, &[unverified], &user_video_config),
        MatchOutcome::Matched { .. }
    ));

    let mut spoof = cand(
        "Aimyon Fan Channel - Marigold [OFFICIAL MUSIC VIDEO]",
        "ファン投稿",
        None,
        Some(322),
    );
    spoof.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    spoof.channel = Some("ファン投稿".to_owned());
    spoof.preflighted = true;
    spoof.channel_verified = Some(true);
    spoof.channel_id = Some("verified-fan-channel".to_owned());
    spoof.has_audio_format = Some(true);

    let spoof_score = score_candidate_breakdown_with_config(&i, &spoof, &user_video_config);
    assert!(spoof_score.artist < 0.5);
    assert!(
        !spoof_score
            .reason_codes
            .contains(&"corroborated_title_artist_credit".to_owned())
    );
    assert!(!matches!(
        best_outcome(&i, &[spoof], &user_video_config),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn official_title_credit_cannot_override_instrumental_identity_gate() {
    let i = input("Marigold", &["Aimyon"], None, Some(306));
    let mut video = cand(
        "Aimyon - Marigold (Instrumental) [Official Audio]",
        "あいみょん",
        None,
        Some(306),
    );
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("あいみょん".to_owned());
    video.preflighted = true;
    video.channel_verified = Some(true);
    video.has_audio_format = Some(true);
    video
        .preflight_reason_codes
        .push("metadata_title_channel_corroborated".to_owned());

    let breakdown = score_candidate_breakdown(&i, &video);
    assert_eq!(breakdown.artist, 1.0);
    assert!(breakdown.accept_blocked);
    assert!(
        breakdown
            .reason_codes
            .contains(&"instrumental_mismatch".to_owned())
    );
    assert!(!matches!(
        best_outcome(&i, &[video], &MatchConfig::default()),
        MatchOutcome::Matched { .. }
    ));
}

#[test]
fn low_audio_ceiling_stays_review_only_and_reason_codes_are_unique() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let mut video = cand("Song (Official Audio)", "Artist", Some("Album"), Some(180));
    video.source_kind = CandidateSourceKind::YoutubeVideoSearch;
    video.channel = Some("Artist".to_owned());
    video.preflighted = true;
    video.has_audio_format = Some(true);
    video.max_audio_bitrate_kbps = Some(32.0);
    video.preflight_reason_codes = vec![
        "metadata_preflight".to_owned(),
        "low_audio_ceiling".to_owned(),
        "low_audio_ceiling".to_owned(),
    ];

    match best_outcome(&i, &[video], &MatchConfig::default()) {
        MatchOutcome::Ambiguous { candidates } => {
            let score = candidates[0].score_breakdown.as_ref().unwrap();
            assert!(score.accept_blocked);
            assert_eq!(
                score
                    .reason_codes
                    .iter()
                    .filter(|reason| reason.as_str() == "low_audio_ceiling")
                    .count(),
                1
            );
        }
        other => panic!("expected low-quality official upload to need review, got {other:?}"),
    }
}

#[test]
fn adversarial_version_matrix_never_silently_auto_accepts_plain_source() {
    let i = input("Song", &["Artist"], Some("Album"), Some(180));
    let unsafe_versions = [
        "Song (Instrumental)",
        "Song (Karaoke)",
        "Song (Backing Track)",
        "Song (Vocal Removed)",
        "Song (Cover)",
        "Song (AI Cover)",
        "Song (Live)",
        "Song (Acoustic)",
        "Song (Demo)",
        "Song (Rehearsal)",
        "Song (Remix)",
        "Song (Radio Edit)",
        "Song (Extended Mix)",
        "Song (Sped Up)",
        "Song (Slowed)",
        "Song (Slowed + Reverb)",
        "Song (Nightcore)",
        "Song (8D Audio)",
        "Song (Bass Boosted)",
        "Song (Lofi)",
        "Song (2011 Remaster)",
        "Song (Taylor's Version)",
        "Song (Japanese Version)",
    ];

    for title in unsafe_versions {
        let outcome = best_outcome(
            &i,
            &[cand(title, "Artist", Some("Album"), Some(180))],
            &MatchConfig::default(),
        );
        assert!(
            !matches!(outcome, MatchOutcome::Matched { .. }),
            "plain source unexpectedly auto-accepted {title}: {outcome:?}"
        );
    }
}

#[test]
fn requested_version_matrix_requires_the_same_version_marker() {
    for source_title in [
        "Song (Instrumental)",
        "Song (Live)",
        "Song (Acoustic)",
        "Song (Remix)",
        "Song (Radio Edit)",
        "Song (2011 Remaster)",
        "Song (Taylor's Version)",
    ] {
        let source = input(source_title, &["Artist"], Some("Album"), Some(180));
        let plain = cand("Song", "Artist", Some("Album"), Some(180));
        assert!(
            !matches!(
                best_outcome(&source, &[plain], &MatchConfig::default()),
                MatchOutcome::Matched { .. }
            ),
            "requested version {source_title} unexpectedly accepted a plain candidate"
        );

        let same = cand(source_title, "Artist", Some("Album"), Some(180));
        assert!(
            matches!(
                best_outcome(&source, &[same], &MatchConfig::default()),
                MatchOutcome::Matched { .. }
            ),
            "requested version {source_title} did not accept the same marked version"
        );
    }
}

#[test]
fn duration_mismatch_blocks_auto_accept() {
    let i = input("Song", &["Artist"], None, Some(180));
    let c = cand("Song", "Artist", None, Some(195));

    match best_outcome(&i, &[c], &MatchConfig::default()) {
        MatchOutcome::Ambiguous { candidates } => {
            let score = candidates[0].score_breakdown.as_ref().unwrap();
            assert!(score.accept_blocked);
            assert_eq!(score.duration_delta_secs, Some(15));
            assert!(score.reason_codes.contains(&"duration_mismatch".to_owned()));
        }
        other => panic!("expected duration mismatch review, got {other:?}"),
    }
}

#[test]
fn spotify_query_plan_uses_isrc_first_then_fielded_fallbacks() {
    let mut i = input("Song (feat. Guest)", &["Artist"], Some("Album"), Some(180));
    i.isrc = Some("USRC17607839".to_owned());
    i.album_release_date = Some("2023-09-22".to_owned());

    assert_eq!(
        spotify_query_plan(&i),
        vec![
            "isrc:USRC17607839".to_owned(),
            "track:\"song\" artist:\"Artist\" album:\"Album\"".to_owned(),
            "track:\"song\" year:2023 artist:\"Artist\"".to_owned(),
            "Artist Song (feat. Guest)".to_owned(),
        ]
    );
}

#[test]
fn dual_script_title_matches_either_form() {
    // "TT (티티)" vs "TT": the feat-stripper doesn't touch it (not noise), but the
    // full-form comparison still lands via similarity of normalized strings.
    let a = input("TT", &["TWICE"], None, Some(212));
    let c = cand("TT (티티)", "TWICE", None, Some(212));
    assert!(
        score_candidate(&a, &c) >= 0.80,
        "{}",
        score_candidate(&a, &c)
    );
}

#[test]
fn exact_match_scores_high_and_wrong_artist_low() {
    let i = input("ETA", &["NewJeans"], Some("Get Up"), Some(151));
    let exact = cand("ETA", "NewJeans", Some("Get Up"), Some(151));
    assert!(score_candidate(&i, &exact) >= 0.95);

    let cover = cand("ETA", "Random Cover Band", None, Some(151));
    assert!(score_candidate(&i, &cover) < 0.80);
}

#[test]
fn score_breakdown_exposes_weighted_components() {
    let i = input("ETA", &["NewJeans"], Some("Get Up"), Some(151));
    let exact = cand("ETA", "NewJeans", Some("Get Up"), Some(151));

    let breakdown = score_candidate_breakdown(&i, &exact);

    assert_eq!(breakdown.title, 1.0);
    assert_eq!(breakdown.artist, 1.0);
    assert_eq!(breakdown.duration, 1.0);
    assert_eq!(breakdown.album_bonus, 0.08);
    assert_eq!(breakdown.total, score_candidate(&i, &exact));
}

#[test]
fn album_track_candidate_gets_track_number_bonus() {
    let mut i = input("Album Cut", &["Artist"], Some("Album"), Some(180));
    i.track_number = Some(7);
    let mut exact = cand("Album Cut", "Artist", Some("Album"), Some(180));
    exact.source_kind = CandidateSourceKind::YtmAlbumTrack;
    exact.track_number = Some(7);

    let breakdown = score_candidate_breakdown(&i, &exact);

    assert_eq!(breakdown.source_kind, "ytm_album_track");
    assert_eq!(breakdown.track_number_bonus, 0.02);
    assert_eq!(breakdown.confidence_tier, "exact");
    assert!(breakdown.reason_codes.contains(&"album_track".to_owned()));
}

#[test]
fn duration_delta_penalizes() {
    let i = input("Song", &["Artist"], None, Some(200));
    let close = cand("Song", "Artist", None, Some(202));
    let far = cand("Song", "Artist", None, Some(220));
    assert!(score_candidate(&i, &close) > score_candidate(&i, &far));
    assert!(score_candidate_breakdown(&i, &far).accept_blocked);
}

#[test]
fn album_bonus_breaks_remaster_tie() {
    let i = input("Track", &["Artist"], Some("Original Album"), Some(200));
    let original = cand("Track", "Artist", Some("Original Album"), Some(200));
    let remaster = cand("Track", "Artist", Some("Greatest Hits"), Some(200));
    assert!(score_candidate(&i, &original) > score_candidate(&i, &remaster));
}

#[test]
fn cjk_title_similarity_works() {
    let i = input("사건의 지평선", &["윤하"], None, Some(300));
    let exact = cand("사건의 지평선", "윤하 (YOUNHA)", None, Some(301));
    assert!(
        score_candidate(&i, &exact) >= 0.80,
        "containment on dual-script artist: {}",
        score_candidate(&i, &exact)
    );
}

#[test]
fn multi_artist_containment() {
    let i = input("Duet", &["IU", "Someone Else"], None, None);
    let c = cand("Duet", "IU & Someone Else", None, None);
    assert!(score_candidate(&i, &c) >= 0.75);
}

#[test]
fn classification_bands() {
    let cfg = MatchConfig::default();
    let i = input("ETA", &["NewJeans"], None, Some(151));
    // Accept.
    let out = best_outcome(&i, &[cand("ETA", "NewJeans", None, Some(151))], &cfg);
    match out {
        MatchOutcome::Matched {
            score,
            score_breakdown: Some(score_breakdown),
            ..
        } => assert_eq!(score_breakdown.total, score),
        other => panic!("got {other:?}"),
    }
    // Ambiguous band: same title, artist edit-distance-ish, duration off.
    let out = best_outcome(
        &i,
        &[cand("ETA", "NewJeanz Tribute", None, Some(170))],
        &cfg,
    );
    match out {
        MatchOutcome::Ambiguous { candidates } => {
            assert_eq!(
                candidates[0].score_breakdown.as_ref().unwrap().total,
                candidates[0].score
            );
        }
        other => panic!("got {other:?}"),
    }
    // Nothing close.
    let out = best_outcome(&i, &[cand("Different Song", "Other", None, Some(90))], &cfg);
    assert!(matches!(out, MatchOutcome::NotFound));
    // Empty candidate set.
    assert!(matches!(
        best_outcome(&i, &[], &cfg),
        MatchOutcome::NotFound
    ));
}

#[test]
fn memo_key_folds_case_and_annotations() {
    let a = input("Song (feat. X)", &["Artist"], None, None);
    let b = input("SONG (FEAT. X)", &["artist"], None, None);
    assert_eq!(memo_key(&a), memo_key(&b));
}

#[test]
fn memo_key_separates_recording_identity_and_source_metadata() {
    let base = input("Song", &["Artist", "Guest"], Some("Album"), Some(180));

    let mut different_source = base.clone();
    different_source.source_key = "spotify:track:other".to_owned();
    assert_ne!(memo_key(&base), memo_key(&different_source));

    let mut different_isrc = base.clone();
    different_isrc.isrc = Some("USRC00000001".to_owned());
    assert_ne!(memo_key(&base), memo_key(&different_isrc));

    let mut different_version = base.clone();
    different_version.title = "Song (Instrumental)".to_owned();
    assert_ne!(memo_key(&base), memo_key(&different_version));

    let mut different_album = base.clone();
    different_album.album = Some("Deluxe Album".to_owned());
    assert_ne!(memo_key(&base), memo_key(&different_album));

    let mut different_duration = base.clone();
    different_duration.duration_secs = Some(240);
    assert_ne!(memo_key(&base), memo_key(&different_duration));

    let mut different_artist_order = base.clone();
    different_artist_order.artists.reverse();
    assert_ne!(memo_key(&base), memo_key(&different_artist_order));
}

/// Degraded (yt-dlp video) results: MV-decorated titles, artist-in-title, and
/// channel names must still land above the accept threshold.
#[test]
fn video_result_shapes_still_match() {
    // "IU 'Celebrity' M/V" on the official channel, duration off by MV extras.
    let i = input("Celebrity", &["IU"], None, Some(195));
    let mv = cand(
        "IU 'Celebrity' M/V",
        "이지금 [IU Official]",
        None,
        Some(215),
    );
    assert!(
        score_candidate(&i, &mv) >= 0.80,
        "MV shape: {}",
        score_candidate(&i, &mv)
    );

    // Topic channel = catalog audio: artist is "<Artist> - Topic".
    let i = input("헤어진 후에", &["Y2K"], None, Some(272));
    let topic = cand("헤어진 후에", "Y2K - Topic", None, Some(273));
    assert!(
        score_candidate(&i, &topic) >= 0.90,
        "topic shape: {}",
        score_candidate(&i, &topic)
    );

    // Lyric-video decoration.
    let i = input("Way Back Home", &["SHAUN"], None, Some(217));
    let lyric = cand(
        "숀 (SHAUN) - 웨이백홈 (Way Back Home) [Lyric Video]",
        "Official dingo",
        None,
        Some(218),
    );
    assert!(
        score_candidate(&i, &lyric) >= 0.60,
        "lyric-video shape at least ambiguous: {}",
        score_candidate(&i, &lyric)
    );

    // Noise phrases never eat real titles: "Video Games" stays itself.
    assert_eq!(normalize_stripped("Video Games"), "video games");
    assert_eq!(normalize_stripped("Celebrity (Official MV)"), "celebrity");
    assert_eq!(normalize_stripped("Celebrity M/V"), "celebrity");
}
