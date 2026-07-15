use super::*;
use crate::local::FileFingerprint;
use std::time::Instant;

fn track(path: &str, title: &str, artist: &[&str]) -> LocalTrack {
    let mut track = LocalTrack::untagged(PathBuf::from(path), 100, 10);
    track.title = title.to_owned();
    track.artist = artist.iter().map(|value| (*value).to_owned()).collect();
    track.duration_ms = Some(180_000);
    track
}

fn retag_id(track: &mut LocalTrack, salt: i64) {
    track.fingerprint = FileFingerprint::path_mtime_size(&track.path, salt, 100);
    track.id = LocalTrackId::from_fingerprint(&track.fingerprint);
}

fn fixture() -> Vec<LocalTrack> {
    let mut exact = track(
        "/music/IU/Palette/01 Palette.flac",
        "Ｐａｌｅｔｔｅ",
        &["아이유"],
    );
    exact.album = Some("Palette".to_owned());
    exact.album_artist = Some("IU".to_owned());
    exact.genre = vec!["K-Pop".to_owned()];
    exact.year = Some(2017);
    exact.disc_no = Some(1);
    exact.track_no = Some(1);
    exact.format = Some(AudioFormat::Flac);
    exact.modified_at = 30;
    exact.embedded_art_key = Some("cover".to_owned());

    let mut prefix = track(
        "/music/IU/Palette/02 Palette Jam.mp3",
        "Palette Jam",
        &["IU"],
    );
    prefix.album = Some("Palette".to_owned());
    prefix.album_artist = Some("IU".to_owned());
    prefix.genre = vec!["Pop".to_owned()];
    prefix.year = Some(2018);
    prefix.disc_no = Some(1);
    prefix.track_no = Some(2);
    prefix.format = Some(AudioFormat::Mp3);
    prefix.modified_at = 20;
    prefix.linked_video_id = Some("yt-palette-jam".to_owned());

    let mut path = track(
        "/music/palette-archive/hidden.ogg",
        "Hidden Song",
        &["Other"],
    );
    path.album = Some("Archive".to_owned());
    path.year = Some(1999);
    path.format = Some(AudioFormat::Ogg);
    path.modified_at = 10;

    vec![exact, prefix, path]
}

fn search_tracks(corpus: &LocalFindCorpus, text: &str) -> LocalFindSnapshot {
    let query = LocalFindQuery::parse(text).expect("valid query");
    corpus.search(&query, LocalFindScope::Tracks, LocalFindSort::Relevance, 7)
}

#[test]
fn parser_normalizes_nfkc_quotes_and_ands_terms() {
    let query = LocalFindQuery::parse("Ｔ:\"Ｐａｌｅｔｔｅ Ｊａｍ\" ar:ＩＵ 좋은").unwrap();
    assert_eq!(
        query.terms,
        vec![
            LocalFindTerm {
                field: LocalFindField::Title,
                value: "palette jam".to_owned(),
            },
            LocalFindTerm {
                field: LocalFindField::TrackArtist,
                value: "iu".to_owned(),
            },
            LocalFindTerm {
                field: LocalFindField::Any,
                value: "좋은".to_owned(),
            },
        ]
    );
}

#[test]
fn parser_covers_every_structured_prefix() {
    let query = LocalFindQuery::parse(
        "t:x ar:y al:z aa:q g:pop path:/music fmt:flac year:1990..1999 is:lossless is:local-only missing:artist missing:album missing:cover sort:recent",
    )
    .unwrap();
    assert_eq!(query.terms.len(), 7);
    assert_eq!(
        query.years,
        vec![LocalFindYearRange {
            start: 1990,
            end: 1999
        }]
    );
    assert_eq!(
        query.predicates,
        vec![LocalFindIs::Lossless, LocalFindIs::LocalOnly]
    );
    assert_eq!(
        query.missing,
        vec![
            LocalFindMissing::Artist,
            LocalFindMissing::Album,
            LocalFindMissing::Cover
        ]
    );
    assert_eq!(query.sort_override, Some(LocalFindSort::Recent));
}

#[test]
fn recognized_prefix_errors_do_not_turn_into_literal_terms() {
    for (query, kind) in [
        ("t:", LocalFindParseErrorKind::EmptyValue),
        ("year:nope", LocalFindParseErrorKind::InvalidYear),
        ("year:2000..1990", LocalFindParseErrorKind::InvalidYear),
        ("is:remote", LocalFindParseErrorKind::InvalidValue),
        ("missing:title", LocalFindParseErrorKind::InvalidValue),
        ("sort:fuzzy", LocalFindParseErrorKind::InvalidValue),
        ("\"open", LocalFindParseErrorKind::UnclosedQuote),
    ] {
        assert_eq!(LocalFindQuery::parse(query).unwrap_err().kind, kind);
    }
}

#[test]
fn parse_errors_are_localized_from_the_typed_error_kind() {
    let _guard = crate::i18n::lock_for_test();
    let error = LocalFindQuery::parse("year:nope").unwrap_err();
    assert_eq!(
        error.localized_message(),
        "year: expects a year or inclusive start..end range"
    );

    crate::i18n::set_language(crate::i18n::Language::Korean);
    let korean = error.localized_message();
    assert!(korean.contains("유효하지 않은 연도"));
    assert!(korean.contains("year:nope"));
}

#[test]
fn unknown_prefix_is_literal_text() {
    let query = LocalFindQuery::parse("mood:blue").unwrap();
    assert_eq!(
        query.terms,
        vec![LocalFindTerm {
            field: LocalFindField::Any,
            value: "mood:blue".to_owned(),
        }]
    );
}

#[test]
fn typed_commands_are_closed_and_unknown_commands_never_execute() {
    for command in LocalFindCommand::ALL {
        let parsed = LocalFindQuery::parse(&format!("> {}", command.as_str())).unwrap();
        assert_eq!(parsed.command.unwrap().exact, Some(command));
    }
    let partial = LocalFindQuery::parse("> scan").unwrap().command.unwrap();
    assert_eq!(partial.exact, None);
    assert_eq!(partial.suggestions, vec![LocalFindCommand::ScanErrors]);
    let unknown = LocalFindQuery::parse("> rm -rf /")
        .unwrap()
        .command
        .unwrap();
    assert_eq!(unknown.exact, None);
    assert!(unknown.suggestions.is_empty());
}

#[test]
fn search_applies_and_field_year_boolean_missing_and_format_filters() {
    let mut tracks = fixture();
    tracks[0].artist.clear();
    let corpus = LocalFindCorpus::from_tracks(&tracks);
    let snapshot = search_tracks(
        &corpus,
        "t:palette al:palette fmt:flac year:2017 is:lossless is:local-only missing:artist",
    );
    assert_eq!(snapshot.total_hits, 1);
    assert_eq!(snapshot.groups[0].hits[0].label, "Ｐａｌｅｔｔｅ");
    assert_eq!(snapshot.generation, 7);
}

#[test]
fn relevance_is_exact_then_prefix_then_path_only() {
    let corpus = LocalFindCorpus::from_tracks(&fixture());
    let snapshot = search_tracks(&corpus, "palette");
    let labels: Vec<_> = snapshot.groups[0]
        .hits
        .iter()
        .map(|hit| hit.label.as_str())
        .collect();
    assert_eq!(labels, vec!["Ｐａｌｅｔｔｅ", "Palette Jam", "Hidden Song"]);
}

#[test]
fn structured_title_relevance_is_exact_then_word_prefix_then_substring() {
    let exact = track("/music/exact.flac", "Jam", &["One"]);
    let prefix = track("/music/prefix.flac", "Zed Jam", &["Two"]);
    let substring = track("/music/substring.flac", "Ajam", &["Three"]);
    let corpus = LocalFindCorpus::from_tracks(&[substring, prefix, exact]);

    let snapshot = search_tracks(&corpus, "t:jam");
    let labels = snapshot.groups[0]
        .hits
        .iter()
        .map(|hit| hit.label.as_str())
        .collect::<Vec<_>>();
    assert_eq!(labels, vec!["Jam", "Zed Jam", "Ajam"]);

    let and_filtered = search_tracks(&corpus, "t:jam ar:two");
    assert_eq!(and_filtered.total_hits, 1);
    assert_eq!(and_filtered.groups[0].hits[0].label, "Zed Jam");
}

#[test]
fn explicit_sorts_are_deterministic_and_put_missing_year_last() {
    let corpus = LocalFindCorpus::from_tracks(&fixture());
    let recent = search_tracks(&corpus, "sort:recent");
    let recent_labels: Vec<_> = recent.groups[0]
        .hits
        .iter()
        .map(|hit| hit.label.as_str())
        .collect();
    assert_eq!(
        recent_labels,
        vec!["Ｐａｌｅｔｔｅ", "Palette Jam", "Hidden Song"]
    );

    let year = search_tracks(&corpus, "sort:year");
    let years: Vec<_> = year.groups[0].hits.iter().map(|hit| hit.year).collect();
    assert_eq!(years, vec![Some(2018), Some(2017), Some(1999)]);
}

#[test]
fn all_groups_are_fixed_and_all_mix_uses_direct_tracks_only() {
    let mut tracks = fixture();
    tracks[2].genre = vec!["K-Pop".to_owned()];
    let corpus = LocalFindCorpus::from_tracks(&tracks);
    let query = LocalFindQuery::parse("palette").unwrap();
    let snapshot = corpus.search(&query, LocalFindScope::All, LocalFindSort::Relevance, 1);
    let scopes: Vec<_> = snapshot.groups.iter().map(|group| group.scope).collect();
    assert_eq!(
        scopes,
        vec![
            LocalFindScope::Tracks,
            LocalFindScope::Albums,
            LocalFindScope::Artists,
            LocalFindScope::Genres,
            LocalFindScope::Folders,
        ]
    );
    let mix = corpus.mix_for_snapshot(&snapshot).unwrap();
    assert_eq!(
        mix.len(),
        3,
        "only three direct matching tracks enter All mix"
    );
}

#[test]
fn snapshot_hit_at_indexes_across_groups_without_flattening() {
    let mut tracks = fixture();
    tracks[2].genre = vec!["K-Pop".to_owned()];
    let corpus = LocalFindCorpus::from_tracks(&tracks);
    let snapshot = corpus.search(
        &LocalFindQuery::parse("palette").unwrap(),
        LocalFindScope::All,
        LocalFindSort::Relevance,
        1,
    );
    assert!(snapshot.groups.len() > 1);

    let flattened = snapshot.hits().map(|hit| &hit.id).collect::<Vec<_>>();
    for (index, expected) in flattened.iter().enumerate() {
        assert_eq!(snapshot.hit_at(index).map(|hit| &hit.id), Some(*expected));
    }
    assert_eq!(snapshot.hit_at(flattened.len()), None);
    assert_eq!(snapshot.hit_at(usize::MAX), None);
}

#[test]
fn collection_mix_expands_and_deduplicates_stable_track_ids() {
    let tracks = fixture();
    let playlist = LocalFindPlaylistInput {
        id: "repeat".to_owned(),
        name: "Palette Repeat".to_owned(),
        entries: vec![
            LocalFindPlaylistEntryInput {
                local_track_id: Some(tracks[0].id.clone()),
                ..LocalFindPlaylistEntryInput::default()
            },
            LocalFindPlaylistEntryInput {
                local_track_id: Some(tracks[0].id.clone()),
                ..LocalFindPlaylistEntryInput::default()
            },
            LocalFindPlaylistEntryInput {
                local_track_id: Some(tracks[1].id.clone()),
                ..LocalFindPlaylistEntryInput::default()
            },
        ],
    };
    let corpus = LocalFindCorpus::build(
        &tracks,
        &[playlist],
        LocalFindCorpusRevision {
            index: 4,
            playlists: 5,
            downloads: 0,
            options: 0,
        },
        &LocalFindCorpusOptions::default(),
    );
    let snapshot = corpus.search(
        &LocalFindQuery::parse("repeat").unwrap(),
        LocalFindScope::Playlists,
        LocalFindSort::Relevance,
        9,
    );
    assert_eq!(snapshot.groups[0].hits[0].locally_playable_count, 3);
    assert_eq!(
        snapshot.groups[0].hits[0].match_reason,
        Some(LocalFindMatchReason::PlaylistName)
    );
    assert_eq!(corpus.mix_for_snapshot(&snapshot).unwrap().len(), 2);
    assert_eq!(snapshot.corpus_revision.index, 4);

    let track_match = corpus.search(
        &LocalFindQuery::parse("t:\"Palette Jam\"").unwrap(),
        LocalFindScope::Playlists,
        LocalFindSort::Relevance,
        10,
    );
    assert_eq!(
        track_match.groups[0].hits[0].match_reason,
        Some(LocalFindMatchReason::ResolvedLocalTrack)
    );
}

#[test]
fn playlist_query_requires_the_name_or_one_resolved_track_to_satisfy_the_whole_and() {
    let first = track("/music/blue.flac", "Blue", &["One"]);
    let second = track("/music/red.flac", "Red", &["Two"]);
    let playlist = LocalFindPlaylistInput {
        id: "road".to_owned(),
        name: "Road Mix".to_owned(),
        entries: vec![
            LocalFindPlaylistEntryInput {
                local_track_id: Some(first.id.clone()),
                ..LocalFindPlaylistEntryInput::default()
            },
            LocalFindPlaylistEntryInput {
                local_track_id: Some(second.id.clone()),
                ..LocalFindPlaylistEntryInput::default()
            },
        ],
    };
    let corpus = LocalFindCorpus::build(
        &[first, second],
        &[playlist],
        LocalFindCorpusRevision::default(),
        &LocalFindCorpusOptions::default(),
    );
    let search = |raw| {
        corpus.search(
            &LocalFindQuery::parse(raw).unwrap(),
            LocalFindScope::Playlists,
            LocalFindSort::Relevance,
            1,
        )
    };

    assert_eq!(search("blue ar:two").total_hits, 0);
    assert_eq!(search("road ar:two").total_hits, 0);
    let track_match = search("blue ar:one");
    assert_eq!(track_match.total_hits, 1);
    assert_eq!(
        track_match.groups[0].hits[0].match_reason,
        Some(LocalFindMatchReason::ResolvedLocalTrack)
    );
    assert_eq!(
        search("road").groups[0].hits[0].match_reason,
        Some(LocalFindMatchReason::PlaylistName)
    );
}

#[test]
fn stale_snapshot_is_rejected() {
    let tracks = fixture();
    let first = LocalFindCorpus::build(
        &tracks,
        &[],
        LocalFindCorpusRevision {
            index: 1,
            playlists: 0,
            downloads: 0,
            options: 0,
        },
        &LocalFindCorpusOptions::default(),
    );
    let second = LocalFindCorpus::build(
        &tracks,
        &[],
        LocalFindCorpusRevision {
            index: 2,
            playlists: 0,
            downloads: 0,
            options: 0,
        },
        &LocalFindCorpusOptions::default(),
    );
    let snapshot = first.search(
        &LocalFindQuery::parse("palette").unwrap(),
        LocalFindScope::Tracks,
        LocalFindSort::Relevance,
        3,
    );
    assert_eq!(second.mix_for_snapshot(&snapshot), None);
}

#[test]
fn playlist_resolution_uses_priority_and_rejects_ambiguity() {
    let mut tracks = fixture();
    tracks[0].linked_video_id = Some("stable-video".to_owned());
    tracks[0].isrc = Some("ISRC-ONE".to_owned());
    tracks[1].isrc = Some("ISRC-DUP".to_owned());
    tracks[2].isrc = Some("ISRC-DUP".to_owned());
    let playlist = LocalFindPlaylistInput {
        id: "mix".to_owned(),
        name: "Mix".to_owned(),
        entries: vec![
            LocalFindPlaylistEntryInput {
                readable_local_path: Some(tracks[1].path.clone()),
                stable_keys: vec!["stable-video".to_owned()],
                ..LocalFindPlaylistEntryInput::default()
            },
            LocalFindPlaylistEntryInput {
                stable_keys: vec!["stable-video".to_owned()],
                ..LocalFindPlaylistEntryInput::default()
            },
            LocalFindPlaylistEntryInput {
                isrc: Some("isrc-dup".to_owned()),
                ..LocalFindPlaylistEntryInput::default()
            },
            LocalFindPlaylistEntryInput {
                title: tracks[2].title.clone(),
                artists: tracks[2].artist.clone(),
                album: tracks[2].album.clone(),
                duration_ms: tracks[2].duration_ms,
                ..LocalFindPlaylistEntryInput::default()
            },
        ],
    };
    let projection = resolve_playlist_projection(&tracks, &playlist);
    assert!(matches!(
        projection.resolutions[0],
        LocalFindPlaylistEntryResolution::Resolved {
            matched_by: LocalFindPlaylistMatch::LocalPath,
            ..
        }
    ));
    assert!(matches!(
        projection.resolutions[1],
        LocalFindPlaylistEntryResolution::Resolved {
            matched_by: LocalFindPlaylistMatch::StableIdentity,
            ..
        }
    ));
    assert_eq!(
        projection.resolutions[2],
        LocalFindPlaylistEntryResolution::Ambiguous
    );
    assert!(matches!(
        projection.resolutions[3],
        LocalFindPlaylistEntryResolution::Resolved {
            matched_by: LocalFindPlaylistMatch::Metadata,
            ..
        }
    ));
}

#[test]
fn remote_only_playlist_is_hidden_from_all_but_visible_in_playlist_scope() {
    let tracks = fixture();
    let remote = LocalFindPlaylistInput {
        id: "remote".to_owned(),
        name: "Remote Only".to_owned(),
        entries: vec![LocalFindPlaylistEntryInput {
            title: "Not Local".to_owned(),
            artists: vec!["Nobody".to_owned()],
            ..LocalFindPlaylistEntryInput::default()
        }],
    };
    let corpus = LocalFindCorpus::build(
        &tracks,
        &[remote],
        LocalFindCorpusRevision::default(),
        &LocalFindCorpusOptions::default(),
    );
    let query = LocalFindQuery::parse("remote").unwrap();
    let all = corpus.search(&query, LocalFindScope::All, LocalFindSort::Relevance, 1);
    assert!(
        all.groups
            .iter()
            .all(|group| group.scope != LocalFindScope::Playlists)
    );
    let playlists = corpus.search(
        &query,
        LocalFindScope::Playlists,
        LocalFindSort::Relevance,
        2,
    );
    assert_eq!(playlists.groups[0].hits[0].locally_playable_count, 0);
    assert_eq!(playlists.groups[0].hits[0].total_track_count, 1);
    assert!(!playlists.groups[0].hits[0].is_playable());
}

#[test]
fn downloaded_roots_are_classified_without_probing_disk() {
    let tracks = fixture();
    let corpus = LocalFindCorpus::build(
        &tracks,
        &[],
        LocalFindCorpusRevision::default(),
        &LocalFindCorpusOptions {
            downloaded_roots: vec![PathBuf::from("/music/IU")],
        },
    );
    let snapshot = search_tracks(&corpus, "is:downloaded");
    assert_eq!(snapshot.total_hits, 2);
}

#[test]
fn duplicate_metadata_resolution_is_ambiguous() {
    let mut first = track("/one/song.flac", "Same", &["Artist"]);
    let mut second = track("/two/song.flac", "Same", &["Artist"]);
    retag_id(&mut first, 1);
    retag_id(&mut second, 2);
    let projection = resolve_playlist_projection(
        &[first, second],
        &LocalFindPlaylistInput {
            id: "ambiguous".to_owned(),
            name: "Ambiguous".to_owned(),
            entries: vec![LocalFindPlaylistEntryInput {
                title: "Same".to_owned(),
                artists: vec!["Artist".to_owned()],
                duration_ms: Some(180_000),
                ..LocalFindPlaylistEntryInput::default()
            }],
        },
    );
    assert_eq!(
        projection.resolutions,
        vec![LocalFindPlaylistEntryResolution::Ambiguous]
    );
}

#[test]
fn cancellable_search_retires_an_obsolete_worker_without_a_snapshot() {
    let corpus = LocalFindCorpus::from_tracks(&fixture());
    let query = LocalFindQuery::parse("palette").unwrap();
    let polls = std::cell::Cell::new(0_u8);

    let snapshot = corpus.search_cancellable(
        &query,
        LocalFindScope::All,
        LocalFindSort::Relevance,
        7,
        || {
            polls.set(polls.get().saturating_add(1));
            polls.get() >= 2
        },
    );

    assert!(snapshot.is_none());
    assert!(polls.get() >= 2);
}

#[test]
fn cancellable_search_polls_inside_one_large_collection() {
    let tracks: Vec<_> = (0..130)
        .map(|index| {
            let mut track = track(
                &format!("/music/album/{index:03}.flac"),
                &format!("Song {index:03}"),
                &["Artist"],
            );
            track.album = Some("One Album".to_owned());
            track
        })
        .collect();
    let corpus = LocalFindCorpus::from_tracks(&tracks);
    let query = LocalFindQuery::parse("never-matches").unwrap();
    let polls = std::cell::Cell::new(0_u8);

    let snapshot = corpus.search_cancellable(
        &query,
        LocalFindScope::Albums,
        LocalFindSort::Relevance,
        8,
        || {
            polls.set(polls.get().saturating_add(1));
            polls.get() >= 5
        },
    );

    assert!(snapshot.is_none());
    assert!(polls.get() >= 5);
}

#[test]
fn cancellable_playlist_match_polls_resolved_tracks() {
    let tracks = (0..130)
        .map(|index| {
            track(
                &format!("/music/playlist/{index:03}.flac"),
                &format!("Song {index:03}"),
                &["Artist"],
            )
        })
        .collect::<Vec<_>>();
    let playlist = LocalFindPlaylistInput {
        id: "large".to_owned(),
        name: "Large Collection".to_owned(),
        entries: tracks
            .iter()
            .map(|track| LocalFindPlaylistEntryInput {
                local_track_id: Some(track.id.clone()),
                ..LocalFindPlaylistEntryInput::default()
            })
            .collect(),
    };
    let corpus = LocalFindCorpus::build(
        &tracks,
        &[playlist],
        LocalFindCorpusRevision::default(),
        &LocalFindCorpusOptions::default(),
    );
    let query = LocalFindQuery::parse("never-matches").unwrap();
    let polls = std::cell::Cell::new(0_u8);

    let snapshot = corpus.search_cancellable(
        &query,
        LocalFindScope::Playlists,
        LocalFindSort::Relevance,
        9,
        || {
            polls.set(polls.get().saturating_add(1));
            polls.get() >= 5
        },
    );

    assert!(snapshot.is_none());
    assert!(polls.get() >= 5);
}

/// Synthetic, metadata-rich tracks for the opt-in performance evidence below. Every value is
/// derived in memory: paths are identities/search fields only and are never opened or probed.
fn performance_tracks(count: usize) -> Vec<LocalTrack> {
    (0..count)
        .map(|index| {
            let artist_index = index % 1_000;
            let album_index = index % 5_000;
            let extension = if index % 4 == 0 { "flac" } else { "mp3" };
            let title = if index % 97 == 0 {
                format!("Neon Skyline Take {index:05}")
            } else {
                format!("Archive Cut {index:05}")
            };
            let path = PathBuf::from(format!(
                "/virtual/library/performer-{artist_index:04}/record-{album_index:04}/{index:05}.{extension}"
            ));
            let mut track = LocalTrack::untagged(
                path,
                3_000_000 + index as u64,
                1_700_000_000 + index as i64,
            );
            track.title = title;
            track.artist = vec![format!("Performer {artist_index:04}")];
            track.album = Some(format!("Record {album_index:04}"));
            track.album_artist = track.artist.first().cloned();
            track.genre = vec![match index % 4 {
                0 => "Rock",
                1 => "Jazz",
                2 => "Pop",
                _ => "Folk",
            }
            .to_owned()];
            track.year = Some(1980 + (index % 45) as i32);
            track.disc_no = Some(1 + (index % 2) as u32);
            track.track_no = Some(1 + (index % 24) as u32);
            track.duration_ms = Some(120_000 + (index % 240) as u64 * 1_000);
            track.embedded_art_key = (index % 3 != 0).then(|| format!("cover-{album_index:04}"));
            track.linked_video_id = (index % 5 == 0).then(|| format!("video-{index:05}"));
            track
        })
        .collect()
}

/// Manual Phase-6 evidence, excluded from normal gates because wall-clock measurements are noisy.
/// Run with `--ignored --nocapture`; output is observational and deliberately has no time limit.
#[test]
#[ignore = "manual Local Find 10k/50k corpus and incremental-query timing evidence"]
fn measure_local_find_corpus_build_and_per_keystroke_queries() {
    const QUERIES: [&str; 7] = [
        "n",
        "ne",
        "neo",
        "neon",
        "neon skyline",
        "neon skyline ar:\"Performer 0042\"",
        "t:archive al:\"Record 0042\" year:2000..2020",
    ];

    eprintln!(
        "[local-find-perf] profile={} timings are evidence only; no threshold is asserted",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    for track_count in [10_000, 50_000] {
        let fixture_started = Instant::now();
        let tracks = performance_tracks(track_count);
        eprintln!(
            "[local-find-perf] tracks={track_count} phase=fixture elapsed={:?}",
            fixture_started.elapsed()
        );

        let build_started = Instant::now();
        let corpus = LocalFindCorpus::build(
            std::hint::black_box(tracks.as_slice()),
            &[],
            LocalFindCorpusRevision {
                index: track_count as u64,
                playlists: 0,
                downloads: 0,
                options: 0,
            },
            &LocalFindCorpusOptions {
                downloaded_roots: vec![PathBuf::from("/virtual/library")],
            },
        );
        let build_elapsed = build_started.elapsed();
        assert_eq!(corpus.track_count(), track_count);
        eprintln!(
            "[local-find-perf] tracks={track_count} phase=corpus_build elapsed={build_elapsed:?}"
        );

        for (keystroke, raw_query) in QUERIES.iter().enumerate() {
            let evaluate_started = Instant::now();
            let query = LocalFindQuery::parse(std::hint::black_box(raw_query))
                .expect("performance query must remain valid");
            let snapshot = std::hint::black_box(&corpus).search(
                std::hint::black_box(&query),
                LocalFindScope::All,
                LocalFindSort::Relevance,
                (keystroke + 1) as u64,
            );
            let elapsed = evaluate_started.elapsed();
            let hits = std::hint::black_box(snapshot.total_hits);
            eprintln!(
                "[local-find-perf] tracks={track_count} keystroke={} query={raw_query:?} phase=parse_and_evaluate elapsed={elapsed:?} hits={hits}",
                keystroke + 1,
            );
        }
    }
}
