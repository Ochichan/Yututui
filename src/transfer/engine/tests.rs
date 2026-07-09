use super::*;
use crate::transfer::checkpoint::ReviewDecision;
use crate::transfer::matching::{AmbiguousCandidate, MatchScoreBreakdown};

fn spec(dest: TransferDest) -> JobSpec {
    JobSpec {
        source: TransferSource::SpotifyLiked,
        dest,
        dry_run: false,
        min_score: 0.80,
        take_best: false,
        rematch: false,
    }
}

fn input(title: &str, artists: &[&str]) -> TrackInput {
    TrackInput {
        title: title.to_owned(),
        artists: artists.iter().map(|s| (*s).to_owned()).collect(),
        album_artists: Vec::new(),
        album: Some("Input Album".to_owned()),
        album_id: None,
        album_uri: None,
        album_release_date: None,
        album_release_date_precision: None,
        album_total_tracks: None,
        album_type: None,
        album_art_url: None,
        disc_number: None,
        track_number: None,
        duration_secs: Some(62),
        isrc: None,
        explicit: None,
        source_url: None,
        source_key: format!("src:{title}"),
        known_video_id: None,
    }
}

fn entry(input: TrackInput, outcome: Option<MatchOutcome>) -> TrackEntry {
    TrackEntry {
        input,
        outcome,
        review_decision: None,
        written: false,
    }
}

fn matched(key: &str) -> MatchOutcome {
    MatchOutcome::Matched {
        key: key.to_owned(),
        score: 0.91,
        display: format!("Matched {key}"),
        title: None,
        artist: None,
        album: None,
        duration_secs: None,
        score_breakdown: None,
    }
}

fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("random temp suffix");
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    std::env::temp_dir().join(format!(
        "yututui-engine-{name}-{}-{suffix}.{ext}",
        std::process::id()
    ))
}

#[test]
fn default_dest_name_uses_explicit_names_and_source_fallbacks() {
    assert_eq!(
        default_dest_name(
            &TransferDest::YtmNewPlaylist {
                name: Some("Road Trip".to_owned()),
            },
            "Source"
        ),
        "Road Trip"
    );
    assert_eq!(
        default_dest_name(&TransferDest::YtmNewPlaylist { name: None }, "Source"),
        "Source"
    );
    assert_eq!(
        default_dest_name(
            &TransferDest::LocalPlaylist {
                name: Some("   ".to_owned()),
            },
            "Local Source"
        ),
        "Local Source"
    );
    assert_eq!(
        default_dest_name(
            &TransferDest::YtmExistingPlaylist {
                name: "Existing".to_owned(),
            },
            "Ignored"
        ),
        "Existing"
    );
    assert_eq!(
        default_dest_name(&TransferDest::YtmLikes, "Ignored"),
        "Liked Music"
    );
    assert_eq!(
        default_dest_name(
            &TransferDest::File {
                path: std::path::PathBuf::from("backup.json"),
                format: FileFormat::Json,
            },
            "Ignored"
        ),
        "backup.json"
    );
}

#[test]
fn read_file_inputs_rejects_unsupported_extensions_before_reading() {
    let err = read_file_inputs(std::path::Path::new("playlist.TXT")).unwrap_err();

    assert!(
        err.to_string().contains("unsupported file type `txt`"),
        "{err}"
    );

    let err = read_file_inputs(std::path::Path::new("playlist")).unwrap_err();
    assert!(
        err.to_string().contains("unsupported file type ``"),
        "{err}"
    );
}

#[test]
fn read_file_inputs_loads_json_playlist_envelope() {
    let path = temp_path("playlist", "json");
    let mut song = Song::from_search(
        "dQw4w9WgXcQ",
        "Catalog Song",
        "Catalog Artist",
        "3:32",
        Some("Catalog Album".to_owned()),
    );
    song.duration_secs = Some(212);
    let file = crate::transfer::json::PlaylistFile::new(
        "Roadtrip".to_owned(),
        "ytm:PL123".to_owned(),
        vec![song],
    );
    crate::transfer::json::write_playlist(&path, &file).expect("write playlist json");

    let (name, inputs) = read_file_inputs(&path).expect("read playlist json");
    let _ = std::fs::remove_file(&path);

    assert_eq!(name, "Roadtrip");
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].title, "Catalog Song");
    assert_eq!(inputs[0].artists, vec!["Catalog Artist"]);
    assert_eq!(inputs[0].album.as_deref(), Some("Catalog Album"));
    assert_eq!(inputs[0].duration_secs, Some(212));
    assert_eq!(inputs[0].known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
}

#[test]
fn read_file_inputs_loads_csv_with_file_stem_name() {
    let path = temp_path("csv-source", "csv");
    std::fs::write(
            &path,
            "\
\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\",\"YouTube ID\"
spotify:track:1,\"CSV Song\",\"Artist A, Artist B\",\"CSV Album\",90000,ISRC1,dQw4w9WgXcQ
",
        )
        .expect("write csv");

    let expected_name = path.file_stem().unwrap().to_string_lossy().to_string();
    let (name, inputs) = read_file_inputs(&path).expect("read csv");
    let _ = std::fs::remove_file(&path);

    assert_eq!(name, expected_name);
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].title, "CSV Song");
    assert_eq!(inputs[0].artists, vec!["Artist A", "Artist B"]);
    assert_eq!(inputs[0].album.as_deref(), Some("CSV Album"));
    assert_eq!(inputs[0].duration_secs, Some(90));
    assert_eq!(inputs[0].isrc.as_deref(), Some("ISRC1"));
    assert_eq!(inputs[0].source_key, "spotify:track:1");
    assert_eq!(inputs[0].known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
}

#[test]
fn outcome_counts_ignore_unresolved_and_skipped_entries() {
    let tracks = vec![
        entry(input("Matched", &["A"]), Some(matched("vid-a"))),
        entry(
            input("Ambiguous", &["B"]),
            Some(MatchOutcome::Ambiguous {
                candidates: vec![AmbiguousCandidate {
                    key: "vid-b".to_owned(),
                    score: 0.70,
                    display: "B - Ambiguous".to_owned(),
                    score_breakdown: None,
                }],
            }),
        ),
        entry(input("Missing", &["C"]), Some(MatchOutcome::NotFound)),
        entry(input("Skipped", &["D"]), Some(MatchOutcome::SkippedLocal)),
        entry(input("Pending", &["E"]), None),
    ];

    assert_eq!(outcome_counts(&tracks), (1, 1, 1));
}

#[tokio::test]
async fn fetch_source_reads_file_inputs_without_service_clients() {
    let path = temp_path("fetch-file", "csv");
    std::fs::write(
            &path,
            "\
\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\",\"YouTube ID\"
spotify:track:1,\"CSV Song\",\"Artist A\",\"CSV Album\",90000,ISRC1,dQw4w9WgXcQ
",
        )
        .expect("write csv");
    let spec = JobSpec {
        source: TransferSource::File { path: path.clone() },
        dest: TransferDest::YtmLikes,
        dry_run: false,
        min_score: 0.80,
        take_best: false,
        rematch: false,
    };
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };

    let (name, entries, skipped_local) = fetch_source("job-fetch-file", &spec, &mut ctx)
        .await
        .unwrap_or_else(|err| panic!("fetch source failed: {}", err.error));
    let _ = std::fs::remove_file(&path);

    assert_eq!(name, path.file_stem().unwrap().to_string_lossy());
    assert_eq!(skipped_local, 0);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].input.title, "CSV Song");
    assert_eq!(
        entries[0].input.known_video_id.as_deref(),
        Some("dQw4w9WgXcQ")
    );
    assert!(entries[0].outcome.is_none());
    assert!(!entries[0].written);
}

#[tokio::test]
async fn fetch_source_caps_large_file_inputs_before_checkpointing() {
    let path = temp_path("fetch-cap", "csv");
    let mut csv = String::from(
        "\"Track URI\",\"Track Name\",\"Artist Name(s)\",\"Album Name\",\"Duration (ms)\",\"ISRC\",\"YouTube ID\"\n",
    );
    for i in 0..(TRACK_CAP + 2) {
        csv.push_str(&format!(
            "spotify:track:{i},\"Song {i}\",\"Artist\",\"Album\",60000,,\n"
        ));
    }
    std::fs::write(&path, csv).expect("write csv");
    let spec = JobSpec {
        source: TransferSource::File { path: path.clone() },
        dest: TransferDest::YtmLikes,
        dry_run: false,
        min_score: 0.80,
        take_best: false,
        rematch: false,
    };
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };

    let (_name, entries, skipped_local) = fetch_source("job-fetch-cap", &spec, &mut ctx)
        .await
        .unwrap_or_else(|err| panic!("fetch source failed: {}", err.error));
    let _ = std::fs::remove_file(&path);

    assert_eq!(entries.len(), TRACK_CAP);
    assert_eq!(entries.last().unwrap().input.title, "Song 9999");
    assert_eq!(skipped_local, 0);
}

#[test]
fn missing_clients_return_setup_errors() {
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };

    let Err(error) = ctx.ytm() else {
        panic!("missing ytm client should fail");
    };
    assert!(!error.resumable);
    assert!(error.error.to_string().contains("YouTube Music cookie"));

    let Err(error) = ctx.spotify() else {
        panic!("missing spotify client should fail");
    };
    assert!(!error.resumable);
    assert!(
        error
            .error
            .to_string()
            .contains("connected Spotify account")
    );
}

#[test]
fn provider_errors_are_classified_for_resume_safety() {
    for error in [
        SpotifyError::RateLimited,
        SpotifyError::Network("timeout".to_owned()),
        SpotifyError::Auth("expired".to_owned()),
    ] {
        assert!(
            spotify_job_error(error).resumable,
            "transient/auth Spotify failures should checkpoint and resume"
        );
    }

    for error in [
        SpotifyError::NotAllowlisted,
        SpotifyError::Api {
            status: 400,
            message: "bad request".to_owned(),
        },
        SpotifyError::Decode("bad json".to_owned()),
    ] {
        assert!(
            !spotify_job_error(error).resumable,
            "permanent Spotify failures should be fatal"
        );
    }

    let ytm_error = ytm_job_error(anyhow!("expired cookie"));
    assert!(ytm_error.resumable);
    assert!(
        ytm_error
            .error
            .to_string()
            .contains("YouTube Music request failed")
    );
}

#[test]
fn progress_beat_accepts_fetch_progress_without_surface_state() {
    let mut beat = progress_beat("job-fetch", Stage::Fetching);
    beat(1, 3, "Page 1".to_owned());
    beat(3, 3, "Done".to_owned());
}

#[test]
fn report_counts_matched_and_preserves_ambiguous_and_not_found_rows() {
    let breakdown = MatchScoreBreakdown {
        total: 0.79,
        raw_total: 0.79,
        title: 0.90,
        artist: 0.80,
        duration: 0.50,
        album_bonus: 0.0,
        quality_bonus: 0.0,
        identity_penalty: 0.0,
        non_music_penalty: 0.0,
        accept_blocked: false,
        reject_reason: None,
        reason_codes: Vec::new(),
        ..MatchScoreBreakdown::default()
    };
    let mut maybe = input("Maybe Song", &["Artist B"]);
    maybe.album_artists = vec!["Album Artist B".to_owned()];
    maybe.album_id = Some("spotify:album-id:b".to_owned());
    maybe.album_uri = Some("spotify:album:b".to_owned());
    maybe.album_release_date = Some("2026-07-01".to_owned());
    maybe.disc_number = Some(1);
    maybe.track_number = Some(2);
    maybe.isrc = Some("ISRC-B".to_owned());
    maybe.explicit = Some(false);
    maybe.source_url = Some("https://open.spotify.com/track/b".to_owned());
    let cp = Checkpoint::new(
        "job-report".to_owned(),
        spec(TransferDest::YtmLikes),
        vec![
            entry(input("Matched Song", &["Artist A"]), Some(matched("vid-a"))),
            entry(
                maybe,
                Some(MatchOutcome::Ambiguous {
                    candidates: vec![
                        AmbiguousCandidate {
                            key: "vid-b".to_owned(),
                            score: 0.79,
                            display: "Artist B - Candidate 1".to_owned(),
                            score_breakdown: Some(breakdown.clone()),
                        },
                        AmbiguousCandidate {
                            key: "vid-c".to_owned(),
                            score: 0.64,
                            display: "Artist B - Candidate 2".to_owned(),
                            score_breakdown: None,
                        },
                    ],
                }),
            ),
            entry(
                input("Missing Song", &["Artist C", "Feat D"]),
                Some(MatchOutcome::NotFound),
            ),
            entry(
                input("Skipped Local", &["Artist E"]),
                Some(MatchOutcome::SkippedLocal),
            ),
        ],
    );

    let report = build_report(&cp, 1);

    assert_eq!(report.job_id, "job-report");
    assert_eq!(report.schema_version, 3);
    assert_eq!(report.total, 4);
    assert_eq!(report.matched, 1);
    assert_eq!(report.skipped_local, 1);
    assert_eq!(report.ambiguous.len(), 1);
    assert_eq!(report.ambiguous[0].title, "Maybe Song");
    assert_eq!(report.ambiguous[0].artists, "Artist B");
    assert_eq!(
        report.ambiguous[0].note,
        "Artist B - Candidate 1 (0.79) | Artist B - Candidate 2 (0.64)"
    );
    assert_eq!(report.ambiguous[0].source_order, Some(2));
    assert_eq!(
        report.ambiguous[0].source_key.as_deref(),
        Some("src:Maybe Song")
    );
    assert_eq!(
        report.ambiguous[0].source_url.as_deref(),
        Some("https://open.spotify.com/track/b")
    );
    assert_eq!(report.ambiguous[0].album.as_deref(), Some("Input Album"));
    assert_eq!(
        report.ambiguous[0].album_artists,
        vec!["Album Artist B".to_owned()]
    );
    assert_eq!(
        report.ambiguous[0].album_id.as_deref(),
        Some("spotify:album-id:b")
    );
    assert_eq!(
        report.ambiguous[0].album_uri.as_deref(),
        Some("spotify:album:b")
    );
    assert_eq!(
        report.ambiguous[0].album_release_date.as_deref(),
        Some("2026-07-01")
    );
    assert_eq!(report.ambiguous[0].disc_number, Some(1));
    assert_eq!(report.ambiguous[0].track_number, Some(2));
    assert_eq!(report.ambiguous[0].duration_secs, Some(62));
    assert_eq!(report.ambiguous[0].isrc.as_deref(), Some("ISRC-B"));
    assert_eq!(report.ambiguous[0].explicit, Some(false));
    assert_eq!(report.ambiguous[0].selected_key.as_deref(), Some("vid-b"));
    assert_eq!(report.ambiguous[0].selected_score, Some(0.79));
    assert_eq!(
        report.ambiguous[0].search_queries,
        vec![
            "Artist B Maybe Song".to_owned(),
            "Album Artist B Maybe Song".to_owned(),
            "Artist B Maybe Song Input Album".to_owned(),
            "Maybe Song Input Album".to_owned(),
            "Artist B Maybe Song 2026".to_owned(),
            "Artist B Maybe Song official audio".to_owned(),
            "Artist B Maybe Song topic".to_owned(),
            "Maybe Song".to_owned(),
        ]
    );
    assert_eq!(report.ambiguous[0].candidates.len(), 2);
    assert_eq!(report.ambiguous[0].candidates[0].key, "vid-b");
    assert_eq!(
        report.ambiguous[0].candidates[0].score_breakdown,
        Some(breakdown)
    );
    assert_eq!(report.not_found.len(), 1);
    assert_eq!(report.not_found[0].artists, "Artist C, Feat D");
    assert_eq!(report.not_found[0].note, "no match on the destination");
    assert_eq!(report.not_found[0].source_order, Some(3));
    assert_eq!(
        report.not_found[0].search_queries,
        vec![
            "Artist C Missing Song".to_owned(),
            "Artist C Feat D Missing Song".to_owned(),
            "Artist C Missing Song Input Album".to_owned(),
            "Missing Song Input Album".to_owned(),
            "Artist C Missing Song official audio".to_owned(),
            "Artist C Missing Song topic".to_owned(),
            "Missing Song".to_owned(),
        ]
    );
}

#[test]
fn song_for_entry_prefers_source_catalog_metadata_for_local_downloads() {
    let mut source = input("Input Title", &["Input Artist"]);
    source.album_artists = vec!["Input Album Artist".to_owned()];
    source.disc_number = Some(1);
    source.track_number = Some(7);
    source.isrc = Some("ISRC123".to_owned());
    source.source_url = Some("https://open.spotify.com/track/input".to_owned());
    let track = entry(
        source,
        Some(MatchOutcome::Matched {
            key: "vid-1".to_owned(),
            score: 0.95,
            display: "Matched Artist - Matched Title".to_owned(),
            title: Some("Matched Title".to_owned()),
            artist: Some("Matched Artist".to_owned()),
            album: Some("Matched Album".to_owned()),
            duration_secs: Some(225),
            score_breakdown: None,
        }),
    );

    let song = song_for_entry(&track, "vid-1", "sp2yt-test", 7);

    assert_eq!(song.video_id, "vid-1");
    assert_eq!(song.title, "Input Title");
    assert_eq!(song.artist, "Input Artist");
    assert_eq!(song.album.as_deref(), Some("Input Album"));
    assert_eq!(song.duration, "1:02");
    assert_eq!(song.duration_secs, Some(62));
    assert_eq!(song.album_artist.as_deref(), Some("Input Album Artist"));
    assert_eq!(song.disc_number, Some(1));
    assert_eq!(song.track_number, Some(7));
    assert_eq!(song.isrc.as_deref(), Some("ISRC123"));
    assert_eq!(song.origin_key.as_deref(), Some("src:Input Title"));
    assert_eq!(
        song.origin_url.as_deref(),
        Some("https://open.spotify.com/track/input")
    );
    assert_eq!(song.import_session_id.as_deref(), Some("sp2yt-test"));
    assert_eq!(song.import_source_order, Some(7));
}

#[test]
fn song_for_entry_falls_back_to_source_input_when_candidate_metadata_is_missing() {
    let track = entry(input("Input Title", &["Artist A", "Artist B"]), None);

    let song = song_for_entry(&track, "vid-fallback", "sp2yt-test", 3);

    assert_eq!(song.video_id, "vid-fallback");
    assert_eq!(song.title, "Input Title");
    assert_eq!(song.artist, "Artist A, Artist B");
    assert_eq!(song.album.as_deref(), Some("Input Album"));
    assert_eq!(song.duration, "1:02");
    assert_eq!(song.duration_secs, Some(62));
    assert_eq!(song.import_session_id.as_deref(), Some("sp2yt-test"));
    assert_eq!(song.import_source_order, Some(3));
}

#[test]
fn song_for_entry_falls_back_to_candidate_when_source_fields_are_missing() {
    let mut source = input("", &[]);
    source.album = None;
    source.duration_secs = None;
    let track = entry(
        source,
        Some(MatchOutcome::Matched {
            key: "vid-empty".to_owned(),
            score: 0.91,
            display: "Matched Title".to_owned(),
            title: Some("Matched Title".to_owned()),
            artist: None,
            album: None,
            duration_secs: None,
            score_breakdown: None,
        }),
    );

    let song = song_for_entry(&track, "vid-empty", "sp2yt-test", 11);

    assert_eq!(song.title, "Matched Title");
    assert_eq!(song.artist, "");
    assert_eq!(song.album, None);
    assert_eq!(song.duration, "");
    assert_eq!(song.duration_secs, None);
    assert_eq!(song.import_session_id.as_deref(), Some("sp2yt-test"));
    assert_eq!(song.import_source_order, Some(11));
}

#[test]
fn progress_write_reports_current_track_and_outcome_counts() {
    let cp = Checkpoint::new(
        "job-progress".to_owned(),
        spec(TransferDest::YtmLikes),
        vec![
            entry(input("Matched Song", &["Artist A"]), Some(matched("vid-a"))),
            entry(
                input("Maybe Song", &["Artist B"]),
                Some(MatchOutcome::Ambiguous {
                    candidates: vec![AmbiguousCandidate {
                        key: "vid-b".to_owned(),
                        score: 0.70,
                        display: "Artist B - Candidate".to_owned(),
                        score_breakdown: None,
                    }],
                }),
            ),
            entry(
                input("Missing Song", &["Artist C"]),
                Some(MatchOutcome::NotFound),
            ),
        ],
    );

    let progress = progress_write(&cp, 2, 3, 1);

    assert_eq!(progress.job_id, "job-progress");
    assert_eq!(progress.stage, Stage::Writing);
    assert_eq!(progress.done, 2);
    assert_eq!(progress.total, 3);
    assert_eq!(progress.matched, 1);
    assert_eq!(progress.ambiguous, 1);
    assert_eq!(progress.not_found, 1);
    assert_eq!(progress.current, "Artist B — Maybe Song");
}

#[test]
fn progress_write_handles_missing_index_without_panicking() {
    let cp = Checkpoint::new(
        "job-progress-missing".to_owned(),
        spec(TransferDest::YtmLikes),
        vec![entry(
            input("Matched Song", &["Artist A"]),
            Some(matched("vid-a")),
        )],
    );

    let progress = progress_write(&cp, 1, 1, 99);

    assert_eq!(progress.job_id, "job-progress-missing");
    assert_eq!(progress.current, "");
    assert_eq!(progress.matched, 1);
}

#[tokio::test]
async fn run_job_resume_rematch_resets_checkpointed_matches_without_persisting() {
    let original_spec = spec(TransferDest::YtmLikes);
    let mut cp = Checkpoint::new(
        "job_with_underscores_no_persist".to_owned(),
        original_spec.clone(),
        vec![
            TrackEntry {
                input: TrackInput {
                    known_video_id: Some("cached-yt-id".to_owned()),
                    ..input("Known Id", &["Artist A"])
                },
                outcome: Some(matched("cached-yt-id")),
                review_decision: None,
                written: true,
            },
            entry(
                input("Already Missing", &["Artist B"]),
                Some(MatchOutcome::NotFound),
            ),
        ],
    );
    cp.stage = Stage::Writing;
    cp.skipped_local = 2;

    let mut resumed_spec = original_spec;
    resumed_spec.dry_run = true;
    resumed_spec.rematch = true;
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };
    let mut progress = Vec::new();

    let report = run_job(
        cp.job_id.clone(),
        resumed_spec,
        Some(cp),
        &mut ctx,
        &mut |p| progress.push(p),
    )
    .await
    .unwrap_or_else(|err| panic!("resume dry-run should not need clients: {}", err.error));

    assert_eq!(report.job_id, "job_with_underscores_no_persist");
    assert_eq!(report.total, 2);
    assert_eq!(report.matched, 0);
    assert_eq!(report.skipped_local, 2);
    assert!(report.ambiguous.is_empty());
    assert!(report.not_found.is_empty());
    assert!(
        progress.is_empty(),
        "writing-stage dry-run should not emit progress"
    );
}

#[tokio::test]
async fn match_stage_requires_ytm_client_for_unresolved_tracks() {
    let mut cp = Checkpoint::new(
        "job_match_missing_client".to_owned(),
        spec(TransferDest::YtmLikes),
        vec![entry(input("Needs Match", &["Artist"]), None)],
    );
    cp.stage = Stage::Matching;
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };

    let err = match_stage(&mut cp, &mut ctx, &mut |_p| {})
        .await
        .expect_err("unresolved YTM match should require a YTM client");

    assert!(!err.resumable);
    assert!(err.error.to_string().contains("YouTube Music cookie"));
    assert!(cp.tracks[0].outcome.is_none());
}

#[tokio::test]
async fn write_stage_dedupes_matches_before_missing_client_error() {
    let mut cp = Checkpoint::new(
        "job_write_missing_client".to_owned(),
        spec(TransferDest::YtmLikes),
        vec![
            entry(input("First", &["A"]), Some(matched("dup-id"))),
            entry(input("Duplicate", &["B"]), Some(matched("dup-id"))),
            entry(
                input("Take Best", &["C"]),
                Some(MatchOutcome::Ambiguous {
                    candidates: vec![AmbiguousCandidate {
                        key: "ambig-id".to_owned(),
                        score: 0.79,
                        display: "C - Candidate".to_owned(),
                        score_breakdown: None,
                    }],
                }),
            ),
            entry(input("Missing", &["D"]), Some(MatchOutcome::NotFound)),
        ],
    );
    cp.stage = Stage::Writing;
    cp.spec.take_best = true;
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };
    let mut report = build_report(&cp, 0);
    let mut progress = Vec::new();

    let err = write_stage(&mut cp, &mut ctx, &mut |p| progress.push(p), &mut report)
        .await
        .expect_err("YTM likes write should require a YTM client");

    assert!(!err.resumable);
    assert!(err.error.to_string().contains("YouTube Music cookie"));
    assert_eq!(report.duplicates_dropped, 1);
    assert_eq!(report.written, 0);
    assert!(progress.is_empty());
    assert!(cp.tracks.iter().all(|track| !track.written));
}

#[test]
fn collect_writes_skips_rejected_review_rows_even_with_take_best() {
    let mut rejected = entry(
        input("Rejected", &["A"]),
        Some(MatchOutcome::Ambiguous {
            candidates: vec![AmbiguousCandidate {
                key: "reject-id".to_owned(),
                score: 0.79,
                display: "A - Rejected".to_owned(),
                score_breakdown: None,
            }],
        }),
    );
    rejected.review_decision = Some(ReviewDecision::Rejected);
    let mut cp = Checkpoint::new(
        "job_review_rejected_local".to_owned(),
        spec(TransferDest::YtmLikes),
        vec![
            rejected,
            entry(input("Accepted", &["B"]), Some(matched("accepted-id"))),
        ],
    );
    cp.stage = Stage::Writing;
    cp.spec.take_best = true;
    let mut report = build_report(&cp, 0);

    let writes = collect_writes(&cp, &mut report);

    assert_eq!(writes, vec![(1, "accepted-id".to_owned())]);
    assert_eq!(report.duplicates_dropped, 0);
    assert_eq!(report.written, 0);
}

#[tokio::test]
async fn write_stage_creates_local_playlist_for_spotify_liked_source() {
    let playlist_name = "Spotify Liked Songs Local Write Test".to_owned();
    let mut cp = Checkpoint::new(
        "job-liked-local-playlist".to_owned(),
        spec(TransferDest::LocalPlaylist {
            name: Some(playlist_name.clone()),
        }),
        vec![entry(
            input("Liked Song", &["Artist A"]),
            Some(matched("vid-liked")),
        )],
    );
    cp.stage = Stage::Writing;
    cp.dest_name = Some(default_dest_name(&cp.spec.dest, "Spotify Liked Songs"));
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };
    let mut report = build_report(&cp, 0);
    let mut progress = Vec::new();

    write_stage(&mut cp, &mut ctx, &mut |p| progress.push(p), &mut report)
        .await
        .unwrap_or_else(|err| panic!("local write should not need service clients: {}", err.error));

    let store = crate::playlists::Playlists::load();
    let playlist = store
        .find(&playlist_name)
        .expect("liked songs local import should create a playlist");

    assert_eq!(playlist.songs.len(), 1);
    assert_eq!(playlist.songs[0].video_id, "vid-liked");
    assert_eq!(playlist.songs[0].title, "Liked Song");
    assert_eq!(cp.dest_id.as_deref(), Some(playlist.id.as_str()));
    assert_eq!(report.written, 1);
    assert_eq!(progress.len(), 1);
    assert!(cp.tracks[0].written);
}

#[tokio::test]
async fn file_export_rejects_sources_that_cannot_be_exported_without_clients() {
    let mut ctx = JobCtx {
        ytm: None,
        spotify: None,
        search_config: SearchConfig::default(),
        market: None,
    };
    let spec = JobSpec {
        source: TransferSource::SpotifyLiked,
        dest: TransferDest::File {
            path: std::path::PathBuf::from("out.json"),
            format: FileFormat::Json,
        },
        dry_run: false,
        min_score: 0.80,
        take_best: false,
        rematch: false,
    };

    let err = export_file(
        "job-export",
        &spec,
        &mut ctx,
        std::path::Path::new("out.json"),
        FileFormat::Json,
        Instant::now(),
    )
    .await
    .unwrap_err();

    assert!(!err.resumable);
    assert!(
        err.error
            .to_string()
            .contains("file export supports YouTube Music / local playlists")
    );
}
