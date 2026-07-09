use super::*;
use crate::transfer::checkpoint::{Checkpoint, ReportCandidate, ReviewDecision, TrackEntry};
use crate::transfer::matching::{
    AmbiguousCandidate, MatchOutcome, MatchScoreBreakdown, TrackInput,
};
use crate::transfer::{JobSpec, TransferDest, TransferSource};
use std::{fs, path::PathBuf};

fn local_deck_track(
    path: &str,
    title: &str,
    artist: &[&str],
    album: Option<&str>,
    album_artist: Option<&str>,
    genre: &[&str],
    modified_at: i64,
) -> crate::local::LocalTrack {
    let mut track = crate::local::LocalTrack::untagged(
        PathBuf::from(path),
        path.len() as u64 + 100,
        modified_at,
    );
    track.title = title.to_owned();
    track.artist = artist.iter().map(|value| (*value).to_owned()).collect();
    track.album = album.map(str::to_owned);
    track.album_artist = album_artist.map(str::to_owned);
    track.genre = genre.iter().map(|value| (*value).to_owned()).collect();
    track.duration_ms = Some(60_000);
    track
}

fn app_with_local_deck_index(tracks: Vec<crate::local::LocalTrack>) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    app.apply_local_mode_confirm(LocalModeConfirm::Enter);
    let mut index = crate::local::LocalIndex::default();
    index.set_tracks(tracks);
    app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            summary: crate::local::LocalScanSummary {
                indexed: index.tracks().len(),
                added: index.tracks().len(),
                ..crate::local::LocalScanSummary::default()
            },
            index,
            errors: Vec::new(),
        },
    }));
    app
}

fn save_ambiguous_import_job(job_id: &str) {
    let mut cp = Checkpoint::new(
        job_id.to_owned(),
        JobSpec {
            source: TransferSource::SpotifyPlaylist {
                id: "spotify-playlist".to_owned(),
            },
            dest: TransferDest::LocalPlaylist { name: None },
            dry_run: true,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        },
        vec![TrackEntry {
            input: TrackInput {
                title: "Maybe".to_owned(),
                artists: vec!["Artist".to_owned()],
                album_artists: vec!["Album Artist".to_owned()],
                album: Some("Album".to_owned()),
                album_id: None,
                album_uri: None,
                album_release_date: Some("2024-05-01".to_owned()),
                disc_number: Some(1),
                track_number: Some(1),
                duration_secs: Some(180),
                isrc: Some("USRC17607839".to_owned()),
                explicit: Some(false),
                source_url: Some("https://open.spotify.com/track/maybe".to_owned()),
                source_key: "spotify:track:maybe".to_owned(),
                known_video_id: None,
            },
            outcome: Some(MatchOutcome::Ambiguous {
                candidates: vec![
                    AmbiguousCandidate {
                        key: "dQw4w9WgXcQ".to_owned(),
                        score: 0.74,
                        display: "Artist - Maybe".to_owned(),
                        score_breakdown: Some(MatchScoreBreakdown {
                            total: 0.74,
                            title: 0.85,
                            artist: 1.0,
                            duration: 0.80,
                            album_bonus: 0.05,
                        }),
                    },
                    AmbiguousCandidate {
                        key: "eQw4w9WgXcQ".to_owned(),
                        score: 0.71,
                        display: "Artist - Maybe alternate".to_owned(),
                        score_breakdown: Some(MatchScoreBreakdown {
                            total: 0.71,
                            title: 0.80,
                            artist: 1.0,
                            duration: 0.72,
                            album_bonus: 0.05,
                        }),
                    },
                ],
            }),
            review_decision: None,
            written: false,
        }],
    );
    cp.stage = crate::transfer::Stage::Writing;
    cp.save().expect("save checkpoint");
    crate::transfer::session::ImportSession::from_checkpoint(&cp)
        .save()
        .expect("save import session");
}

fn single_cmd(cmds: Vec<Cmd>) -> Cmd {
    assert_eq!(cmds.len(), 1, "expected exactly one command");
    cmds.into_iter().next().unwrap()
}

fn finish_import_review_cmd(app: &mut App, cmd: Cmd) {
    let Cmd::Local(LocalCmd::ReviewImport {
        op_id,
        session_id,
        source_order,
        action,
    }) = cmd
    else {
        panic!("expected import review command");
    };
    let result = match action {
        ImportReviewAction::AcceptFirst => {
            crate::transfer::review_action::accept_first_candidate(&session_id, source_order)
        }
        ImportReviewAction::ChooseNext => {
            crate::transfer::review_action::choose_next_candidate(&session_id, source_order)
        }
        ImportReviewAction::Reject => {
            crate::transfer::review_action::reject_row(&session_id, source_order)
        }
        ImportReviewAction::Skip => {
            crate::transfer::review_action::skip_row(&session_id, source_order)
        }
    }
    .map_err(|error| format!("{error:#}"));
    app.update(Msg::Local(LocalMsg::ImportReviewFinished {
        op_id,
        session_id,
        source_order,
        action,
        result,
        elapsed_ms: 0,
    }));
}

fn finish_import_accept_all_cmd(app: &mut App, cmd: Cmd) {
    let Cmd::Local(LocalCmd::ReviewImportAcceptAll { op_id, session_id }) = cmd else {
        panic!("expected import accept-all command");
    };
    let result = crate::transfer::review_action::accept_all_candidates(&session_id)
        .map_err(|error| format!("{error:#}"));
    app.update(Msg::Local(LocalMsg::ImportReviewAcceptAllFinished {
        op_id,
        session_id,
        result,
        elapsed_ms: 0,
    }));
}

fn temp_import_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "yututui-local-import-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp import root");
    root
}

fn save_organizable_import_session(session_id: &str, root: &std::path::Path) -> PathBuf {
    let inbox = root
        .join(".yututui-inbox")
        .join(session_id)
        .join("complete");
    fs::create_dir_all(&inbox).expect("create import inbox");
    let audio = inbox.join("Move Me.m4a");
    fs::write(&audio, b"audio").expect("write inbox audio");
    fs::write(crate::downloads::sidecar_path(&audio), b"{}").expect("write inbox sidecar");

    let session = crate::transfer::session::ImportSession {
        schema_version: 1,
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        created_at: 0,
        updated_at: 101,
        stage: crate::transfer::Stage::Writing,
        counts: crate::transfer::session::ImportSessionCounts {
            total: 1,
            matched: 1,
            ..crate::transfer::session::ImportSessionCounts::default()
        },
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-00001".to_owned(),
            source_order: 1,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            title: "Move Me".to_owned(),
            artists: vec!["Track Artist".to_owned()],
            album_artists: vec!["Album Artist".to_owned()],
            album: Some("Album".to_owned()),
            disc_number: Some(1),
            track_number: Some(1),
            source_key: "spotify:track:move-me".to_owned(),
            selected_key: Some("move0000001".to_owned()),
            local_path: Some(audio.clone()),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    };
    session.save().expect("save organizable session");
    audio
}

fn save_failed_download_import_session(session_id: &str) {
    let session = crate::transfer::session::ImportSession {
        schema_version: 1,
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        created_at: 0,
        updated_at: 77,
        stage: crate::transfer::Stage::Writing,
        counts: crate::transfer::session::ImportSessionCounts {
            total: 1,
            matched: 1,
            ..crate::transfer::session::ImportSessionCounts::default()
        },
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-00009".to_owned(),
            source_order: 9,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            title: "Retry Me".to_owned(),
            artists: vec!["Retry Artist".to_owned()],
            album_artists: vec!["Retry Album Artist".to_owned()],
            album: Some("Retry Album".to_owned()),
            duration_secs: Some(199),
            source_key: "spotify:track:retry-me".to_owned(),
            source_url: Some("https://open.spotify.com/track/retry-me".to_owned()),
            selected_key: Some("retry000001".to_owned()),
            selected_score: Some(0.96),
            selected_display: Some("Retry Artist - Retry Me".to_owned()),
            errors: vec!["network failed".to_owned()],
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    };
    session.save().expect("save failed download session");
}

#[test]
fn local_deck_import_sessions_drill_down_in_source_order() {
    let mut second = local_deck_track(
        "/tmp/music/import/session/02 Second.m4a",
        "Second",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Pop"],
        20,
    );
    second.import_session_id = Some("sp2yt-session".to_owned());
    second.import_source_order = Some(2);

    let mut first = local_deck_track(
        "/tmp/music/import/session/01 First.m4a",
        "First",
        &["Artist"],
        Some("Album"),
        Some("Artist"),
        &["Pop"],
        10,
    );
    first.import_session_id = Some("sp2yt-session".to_owned());
    first.import_source_order = Some(1);

    let mut other = local_deck_track(
        "/tmp/music/import/other/01 Other.m4a",
        "Other",
        &["Other Artist"],
        Some("Other Album"),
        Some("Other Artist"),
        &["Indie"],
        30,
    );
    other.import_session_id = Some("sp2yt-other".to_owned());
    other.import_source_order = Some(1);

    let mut app = app_with_local_deck_index(vec![second, first, other]);
    app.update(Msg::Key(key(KeyCode::Char('9'))));

    assert_eq!(app.local_mode.ui.section, LocalSection::ImportSessions);
    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();
    assert!(
        labels
            .iter()
            .any(|label| label == "sp2yt-other  (1 tracks)")
    );
    assert!(
        labels
            .iter()
            .any(|label| label == "sp2yt-session  (2 tracks)")
    );

    app.local_mode.ui.filter_query = "sp2yt-session".to_owned();
    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();
    assert_eq!(labels, vec!["sp2yt-session  (2 tracks)"]);
    let session_index = 0;
    app.local_mode.ui.selected = session_index;
    app.local_mode.ui.anchor = session_index;
    let details = app.local_details_lines();
    for expected in [
        "Import session: sp2yt-session",
        "Tracks: 2 tracks",
        "Source order: #1-#2",
    ] {
        assert!(
            details.iter().any(|line| line == expected),
            "missing {expected:?} in {details:?}"
        );
    }

    let open = double_click_target(&mut app, MouseTarget::LocalRow(session_index));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();
    assert_eq!(app.local_rows_len(), 2);
    assert!(
        app.local_row_text(&app.local_visible_rows()[0])
            .contains("First")
    );
    assert!(
        app.local_row_text(&app.local_visible_rows()[1])
            .contains("Second")
    );

    let lines = app.local_details_lines();
    assert!(
        lines
            .iter()
            .any(|line| line == "Import session: sp2yt-session")
    );
    assert!(lines.iter().any(|line| line == "Source order: #1"));

    let play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("First")
    );
}

#[test]
fn local_deck_import_sessions_include_saved_session_rows_without_tracks() {
    let session_id = "sp2yt-local-inbox-session";
    let session = crate::transfer::session::ImportSession {
        schema_version: 1,
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        created_at: 0,
        updated_at: 99,
        stage: crate::transfer::Stage::Writing,
        source: crate::transfer::session::SessionEndpoint {
            kind: "spotify_playlist".to_owned(),
            key: Some("spotify-source".to_owned()),
            label: Some("Source".to_owned()),
        },
        destination: crate::transfer::session::SessionEndpoint {
            kind: "local_playlist".to_owned(),
            key: None,
            label: Some("Imported".to_owned()),
        },
        counts: crate::transfer::session::ImportSessionCounts {
            total: 3,
            matched: 1,
            ambiguous: 1,
            not_found: 1,
            ..crate::transfer::session::ImportSessionCounts::default()
        },
        rows: vec![
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00001".to_owned(),
                source_order: 1,
                status: crate::transfer::session::ImportSessionRowStatus::Matched,
                title: "Linked".to_owned(),
                artists: vec!["Artist".to_owned()],
                album_artists: vec!["Album Artist".to_owned()],
                album: Some("Album".to_owned()),
                album_release_date: Some("2024-05-01".to_owned()),
                disc_number: Some(1),
                track_number: Some(2),
                duration_secs: Some(180),
                isrc: Some("USRC17607839".to_owned()),
                explicit: Some(false),
                source_key: "spotify:track:linked".to_owned(),
                source_url: Some("https://open.spotify.com/track/linked".to_owned()),
                selected_key: Some("linked00001".to_owned()),
                selected_score: Some(0.91),
                selected_display: Some("Artist - Linked".to_owned()),
                candidates: vec![ReportCandidate {
                    key: "linked00001".to_owned(),
                    score: 0.91,
                    display: "Artist - Linked".to_owned(),
                    score_breakdown: Some(MatchScoreBreakdown {
                        total: 0.91,
                        title: 0.95,
                        artist: 1.0,
                        duration: 0.90,
                        album_bonus: 0.05,
                    }),
                }],
                local_path: Some(PathBuf::from("/tmp/inbox/Linked.m4a")),
                ..crate::transfer::session::ImportSessionRow::default()
            },
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00002".to_owned(),
                source_order: 2,
                status: crate::transfer::session::ImportSessionRowStatus::Ambiguous,
                title: "Review".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:review".to_owned(),
                ..crate::transfer::session::ImportSessionRow::default()
            },
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00003".to_owned(),
                source_order: 3,
                status: crate::transfer::session::ImportSessionRowStatus::NotFound,
                title: "Failed".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:failed".to_owned(),
                errors: vec!["download failed".to_owned()],
                ..crate::transfer::session::ImportSessionRow::default()
            },
        ],
    };
    session.save().expect("save import session");

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));

    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();
    let session_index = labels
        .iter()
        .position(|label| label.starts_with(session_id))
        .unwrap_or_else(|| panic!("missing saved session in labels: {labels:?}"));
    assert_eq!(
        labels[session_index],
        "sp2yt-local-inbox-session  (1/3 local, 1 failed, 1 review, 1 missing)"
    );

    app.local_mode.ui.filter_query = session_id.to_owned();
    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();
    assert_eq!(
        labels,
        vec!["sp2yt-local-inbox-session  (1/3 local, 1 failed, 1 review, 1 missing)"]
    );
    let session_index = 0;
    app.local_mode.ui.selected = session_index;
    app.local_mode.ui.anchor = session_index;
    let details = app.local_details_lines();
    for expected in [
        "Import session: sp2yt-local-inbox-session",
        "Rows: 3 rows",
        "Local files: 1",
        "Failed: 1",
        "Review: 1",
        "Missing: 1",
        "Source: Source",
        "Destination: Imported",
    ] {
        assert!(
            details.iter().any(|line| line == expected),
            "missing {expected:?} in {details:?}"
        );
    }

    let root_play = app.update(Msg::Key(key(KeyCode::Char('P'))));
    assert!(!root_play.is_empty());
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("Linked")
    );
    assert!(
        load_url(&root_play)
            .expect("session root should load first local row")
            .contains("/tmp/inbox/Linked.m4a")
    );
    app.mode = Mode::Library;

    let open = double_click_target(&mut app, MouseTarget::LocalRow(session_index));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();
    assert_eq!(app.local_rows_len(), 3);
    let row_labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();
    assert_eq!(row_labels[0], "#1 local Linked - Artist");
    assert_eq!(row_labels[1], "#2 review Review - Artist");
    assert_eq!(row_labels[2], "#3 failed Failed - Artist");

    app.local_mode.ui.filter_query = "USRC17607839".to_owned();
    let filtered: Vec<_> = app
        .local_visible_rows()
        .iter()
        .map(|row| app.local_row_text(row))
        .collect();
    assert_eq!(filtered, vec!["#1 local Linked - Artist"]);
    app.local_mode.ui.filter_query.clear();

    let linked_details = app.local_details_lines();
    for expected in [
        "Import session: sp2yt-local-inbox-session",
        "Row: #1",
        "Status: local",
        "Title: Linked",
        "Artist: Artist",
        "Album: Album",
        "Album artist: Album Artist",
        "Release date: 2024-05-01",
        "Track: disc 1 · track 2",
        "Duration: 3:00",
        "ISRC: USRC17607839",
        "Explicit: no",
        "Source: spotify:track:linked",
        "Source URL: https://open.spotify.com/track/linked",
        "Selected: Artist - Linked",
        "Score: 0.91",
        "Decision: undecided",
        "Download: downloaded",
        "Candidate 1: 0.91 Artist - Linked (linked00001)",
        "Score detail 1: total 0.91, title 0.95, artist 1.00, duration 0.90, album +0.05",
        "Path: /tmp/inbox/Linked.m4a",
    ] {
        assert!(
            linked_details.iter().any(|line| line == expected),
            "missing {expected:?} in {linked_details:?}"
        );
    }

    app.local_mode.ui.selected = 2;
    app.local_mode.ui.anchor = 2;
    let failed_details = app.local_details_lines();
    for expected in [
        "Row: #3",
        "Status: failed",
        "Title: Failed",
        "Error: download failed",
    ] {
        assert!(
            failed_details.iter().any(|line| line == expected),
            "missing {expected:?} in {failed_details:?}"
        );
    }
    let failed_play = double_click_target(&mut app, MouseTarget::LocalRow(2));
    assert!(failed_play.is_empty());

    app.local_mode.ui.selected = 0;
    app.local_mode.ui.anchor = 0;
    let play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    assert_eq!(
        app.queue.current().map(|song| song.title.as_str()),
        Some("Linked")
    );
    assert!(
        load_url(&play)
            .expect("linked row should load local path")
            .contains("/tmp/inbox/Linked.m4a")
    );
}

#[test]
fn local_deck_import_row_download_queues_import_inbox_request() {
    let session_id = "sp2yt-local-download-row";
    let session = crate::transfer::session::ImportSession {
        schema_version: 1,
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        created_at: 0,
        updated_at: 88,
        stage: crate::transfer::Stage::Writing,
        counts: crate::transfer::session::ImportSessionCounts {
            total: 1,
            matched: 1,
            ..crate::transfer::session::ImportSessionCounts::default()
        },
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-00007".to_owned(),
            source_order: 7,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            title: "Download Me".to_owned(),
            artists: vec!["Artist".to_owned()],
            album_artists: vec!["Album Artist".to_owned()],
            album: Some("Album".to_owned()),
            duration_secs: Some(181),
            isrc: Some("ISRC-DOWNLOAD".to_owned()),
            source_key: "spotify:track:download-me".to_owned(),
            source_url: Some("https://open.spotify.com/track/download-me".to_owned()),
            selected_key: Some("dQw4w9WgXcQ".to_owned()),
            selected_score: Some(0.94),
            selected_display: Some("Artist - Download Me".to_owned()),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    };
    session.save().expect("save import session");

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    let [Cmd::Download(song)] = cmds.as_slice() else {
        panic!("expected download command");
    };
    let request = crate::download::import_request_for_song(song).expect("import request");
    assert_eq!(request.session_id, session_id);
    assert_eq!(request.row_id, "row-00007");
    assert_eq!(request.source_order, 7);
    assert_eq!(request.song.video_id, "dQw4w9WgXcQ");
    assert_eq!(request.song.title, "Download Me");
    assert_eq!(request.song.artist, "Artist");
    assert_eq!(request.song.album.as_deref(), Some("Album"));
    assert_eq!(request.song.album_artist.as_deref(), Some("Album Artist"));
    assert_eq!(
        request.song.origin_key.as_deref(),
        Some("spotify:track:download-me")
    );
    assert_eq!(
        request.song.origin_url.as_deref(),
        Some("https://open.spotify.com/track/download-me")
    );
    assert_eq!(request.song.import_session_id.as_deref(), Some(session_id));
    assert_eq!(request.song.import_source_order, Some(7));
}

#[test]
fn local_deck_import_failed_row_r_retries_download() {
    let session_id = "sp2yt-local-download-retry";
    save_failed_download_import_session(session_id);

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    assert_eq!(app.local_rows_len(), 1);
    assert_eq!(
        app.local_row_text(&app.local_visible_rows()[0]),
        "#9 failed Retry Me - Retry Artist"
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    let [Cmd::Download(song)] = cmds.as_slice() else {
        panic!("expected retry download command");
    };
    let request = crate::download::import_request_for_song(song).expect("import request");
    assert_eq!(request.session_id, session_id);
    assert_eq!(request.row_id, "row-00009");
    assert_eq!(request.source_order, 9);
    assert_eq!(request.song.video_id, "retry000001");
    assert_eq!(request.song.title, "Retry Me");
    assert_eq!(request.song.artist, "Retry Artist");
    assert_eq!(request.song.album.as_deref(), Some("Retry Album"));
    assert_eq!(
        request.song.album_artist.as_deref(),
        Some("Retry Album Artist")
    );
    assert_eq!(
        request.song.origin_key.as_deref(),
        Some("spotify:track:retry-me")
    );
    assert_eq!(
        request.song.origin_url.as_deref(),
        Some("https://open.spotify.com/track/retry-me")
    );
}

#[test]
fn local_deck_import_row_s_starts_manual_youtube_search() {
    let session_id = "sp2yt-local-manual-search";
    save_ambiguous_import_job(session_id);

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();
    let hint = app.local_import_action_hint().expect("review action hint");
    for expected in [
        "a accept",
        "r reject",
        "c candidate",
        "x skip",
        "o open candidate",
        "s search",
    ] {
        assert!(hint.contains(expected), "missing {expected:?} in {hint:?}");
    }

    let cmds = app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.focus, SearchFocus::Input);
    assert_eq!(app.search.kind, SearchKind::Songs);
    assert_eq!(app.search.input, "Maybe Artist Album");
    let [
        Cmd::Search {
            query,
            source,
            config,
            ..
        },
    ] = cmds.as_slice()
    else {
        panic!("expected manual search command");
    };
    assert_eq!(query, "Maybe Artist Album");
    assert_eq!(*source, crate::search_source::SearchSource::Youtube);
    assert_eq!(config.source, crate::search_source::SearchSource::Youtube);
}

#[test]
fn local_deck_import_row_o_opens_selected_candidate_url() {
    let session_id = "sp2yt-local-open-candidate";
    save_ambiguous_import_job(session_id);

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('o'))));
    assert!(cmds.is_empty());
    assert!(
        app.status
            .text
            .contains("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );
}

#[test]
fn local_deck_recovers_corrupt_import_session_from_checkpoint() {
    let session_id = "sp2yt-local-recover-session";
    save_ambiguous_import_job(session_id);
    let session_path =
        crate::transfer::session::session_path(session_id).expect("import session path");
    fs::write(&session_path, b"{not valid json").expect("corrupt import session");

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    app.local_mode.ui.filter_query = session_id.to_owned();

    assert_eq!(app.local_rows_len(), 1);
    let recovered =
        crate::transfer::session::ImportSession::load(session_id).expect("load recovered session");
    assert_eq!(recovered.rows[0].source_order, 1);
}

#[test]
fn local_deck_import_review_keys_accept_and_reject_rows() {
    let accept_id = "sp2yt-local-review-accept";
    save_ambiguous_import_job(accept_id);

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = accept_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();
    assert_eq!(
        app.local_row_text(&app.local_visible_rows()[0]),
        "#1 review Maybe - Artist"
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));
    let cmd = single_cmd(cmds);
    let before =
        crate::transfer::session::ImportSession::load(accept_id).expect("load pending session");
    assert_eq!(before.rows[0].review_decision, None);
    assert!(app.status.text.contains("Accepting import row"));
    finish_import_review_cmd(&mut app, cmd);
    let accepted =
        crate::transfer::session::ImportSession::load(accept_id).expect("load accepted session");
    assert!(matches!(
        accepted.rows[0].review_decision,
        Some(ReviewDecision::Accepted { ref key, .. }) if key == "dQw4w9WgXcQ"
    ));
    assert_eq!(
        accepted.rows[0].status,
        crate::transfer::session::ImportSessionRowStatus::Matched
    );
    let details = app.local_details_lines();
    for expected in ["Decision: accepted", "Download: ready"] {
        assert!(
            details.iter().any(|line| line == expected),
            "missing {expected:?} in {details:?}"
        );
    }

    let download = app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(matches!(download.as_slice(), [Cmd::Download(_)]));

    let reject_id = "sp2yt-local-review-reject";
    save_ambiguous_import_job(reject_id);
    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = reject_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    let cmd = single_cmd(cmds);
    finish_import_review_cmd(&mut app, cmd);
    let rejected =
        crate::transfer::session::ImportSession::load(reject_id).expect("load rejected session");
    assert_eq!(
        rejected.rows[0].review_decision,
        Some(ReviewDecision::Rejected)
    );
    assert_eq!(
        rejected.rows[0].status,
        crate::transfer::session::ImportSessionRowStatus::Ambiguous
    );
    assert!(app.status.text.contains("Rejected import row"));
}

#[test]
fn local_deck_import_review_keys_choose_next_and_skip_rows() {
    let choose_id = "sp2yt-local-review-choose";
    save_ambiguous_import_job(choose_id);

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = choose_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('c'))));
    let cmd = single_cmd(cmds);
    finish_import_review_cmd(&mut app, cmd);
    let chosen =
        crate::transfer::session::ImportSession::load(choose_id).expect("load chosen session");
    assert!(matches!(
        chosen.rows[0].review_decision,
        Some(ReviewDecision::Accepted { ref key, .. }) if key == "eQw4w9WgXcQ"
    ));
    assert_eq!(
        chosen.rows[0].selected_display.as_deref(),
        Some("Artist - Maybe alternate")
    );
    assert!(app.status.text.contains("Selected import candidate"));

    let skip_id = "sp2yt-local-review-skip";
    save_ambiguous_import_job(skip_id);
    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    app.local_mode.ui.filter_query = skip_id.to_owned();
    assert_eq!(app.local_rows_len(), 1);

    let cmds = app.update(Msg::Key(key(KeyCode::Char('x'))));
    let cmd = single_cmd(cmds);
    finish_import_review_cmd(&mut app, cmd);
    let skipped =
        crate::transfer::session::ImportSession::load(skip_id).expect("load skipped session");
    assert_eq!(
        skipped.rows[0].review_decision,
        Some(ReviewDecision::Skipped)
    );
    assert!(app.status.text.contains("Skipped import row"));
    assert_eq!(
        app.local_rows_len(),
        0,
        "skipped rows should leave the attention inbox"
    );
}

#[test]
fn local_deck_shift_a_confirms_and_accepts_all_session_candidates() {
    let session_id = "sp2yt-local-review-accept-all";
    save_ambiguous_import_job(session_id);

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('9'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    let open = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(open.is_empty());
    app.local_mode.ui.filter_query.clear();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('A'))));
    assert!(cmds.is_empty());
    assert_eq!(
        app.local_mode
            .pending_accept_all_confirm
            .as_ref()
            .map(|confirm| (confirm.session_id.clone(), confirm.candidate_count)),
        Some((session_id.to_owned(), 1))
    );

    let cmd = single_cmd(app.update(Msg::Key(key(KeyCode::Enter))));
    finish_import_accept_all_cmd(&mut app, cmd);
    let accepted =
        crate::transfer::session::ImportSession::load(session_id).expect("load accepted session");
    assert!(matches!(
        accepted.rows[0].review_decision,
        Some(ReviewDecision::Accepted { ref key, .. }) if key == "dQw4w9WgXcQ"
    ));
    assert!(app.status.text.contains("Accepted 1 import candidate"));
}

#[test]
fn local_deck_inbox_lists_actionable_import_rows() {
    let session_id = "sp2yt-local-inbox-section";
    let session = crate::transfer::session::ImportSession {
        schema_version: 1,
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        created_at: 0,
        updated_at: 9_999_999,
        stage: crate::transfer::Stage::Writing,
        counts: crate::transfer::session::ImportSessionCounts {
            total: 4,
            matched: 2,
            ambiguous: 1,
            not_found: 1,
            ..crate::transfer::session::ImportSessionCounts::default()
        },
        rows: vec![
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00001".to_owned(),
                source_order: 1,
                status: crate::transfer::session::ImportSessionRowStatus::Matched,
                title: "Inbox Ready".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:inbox-ready".to_owned(),
                selected_key: Some("inboxready1".to_owned()),
                local_path: Some(PathBuf::from(
                    "/tmp/.yututui-inbox/sp2yt-local-inbox-section/complete/Inbox Ready.m4a",
                )),
                ..crate::transfer::session::ImportSessionRow::default()
            },
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00002".to_owned(),
                source_order: 2,
                status: crate::transfer::session::ImportSessionRowStatus::Matched,
                title: "Committed Outside".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:committed-outside".to_owned(),
                selected_key: Some("committed01".to_owned()),
                local_path: Some(PathBuf::from("/tmp/library/Committed Outside.m4a")),
                ..crate::transfer::session::ImportSessionRow::default()
            },
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00003".to_owned(),
                source_order: 3,
                status: crate::transfer::session::ImportSessionRowStatus::NotFound,
                title: "Inbox Failed".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:inbox-failed".to_owned(),
                errors: vec!["download failed".to_owned()],
                ..crate::transfer::session::ImportSessionRow::default()
            },
            crate::transfer::session::ImportSessionRow {
                row_id: "row-00004".to_owned(),
                source_order: 4,
                status: crate::transfer::session::ImportSessionRowStatus::Ambiguous,
                title: "Inbox Review".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:inbox-review".to_owned(),
                ..crate::transfer::session::ImportSessionRow::default()
            },
        ],
        ..crate::transfer::session::ImportSession::default()
    };
    session.save().expect("save import session");

    let mut app = app_with_local_deck_index(Vec::new());
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    app.local_mode.ui.filter_query = "spotify:track:inbox".to_owned();

    assert_eq!(app.local_mode.ui.section, LocalSection::Inbox);
    let labels: Vec<_> = app
        .local_visible_rows()
        .iter()
        .take(4)
        .map(|row| app.local_row_text(row))
        .collect();
    assert_eq!(labels[0], "#1 inbox Inbox Ready - Artist");
    assert_eq!(labels[1], "#3 failed Inbox Failed - Artist");
    assert_eq!(labels[2], "#4 review Inbox Review - Artist");
    assert!(
        labels
            .iter()
            .all(|label| !label.contains("Committed Outside")),
        "committed row should not appear in inbox labels: {labels:?}"
    );

    let details = app.local_details_lines();
    for expected in [
        "Import session: sp2yt-local-inbox-section",
        "Row: #1",
        "Status: inbox",
        "Title: Inbox Ready",
    ] {
        assert!(
            details.iter().any(|line| line == expected),
            "missing {expected:?} in {details:?}"
        );
    }

    let play = double_click_target(&mut app, MouseTarget::LocalRow(0));
    assert!(!play.is_empty());
    assert!(
        load_url(&play)
            .expect("inbox row should load local path")
            .contains(".yututui-inbox/sp2yt-local-inbox-section/complete/Inbox Ready.m4a")
    );
}

#[test]
fn local_deck_import_inbox_organize_confirms_then_moves_session_files() {
    let session_id = "sp2yt-local-organize-tui";
    let root = temp_import_root("organize-tui");
    let audio = save_organizable_import_session(session_id, &root);
    let sidecar = crate::downloads::sidecar_path(&audio);
    let target = root.join("Album Artist").join("01 - Move Me.m4a");

    let mut app = app_with_local_deck_index(Vec::new());
    app.config.local.include_download_dir = Some(false);
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: root.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];
    app.config.local.import_path_template =
        Some("{album_artist}/{disc_track} - {title}".to_owned());
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    app.local_mode.ui.filter_query = session_id.to_owned();
    assert_eq!(app.local_rows_len(), 1);
    let hint = app.local_import_action_hint().expect("inbox action hint");
    assert!(hint.contains("m commit"), "missing commit hint in {hint:?}");

    let preview = app.local_details_lines();
    assert!(
        preview
            .iter()
            .any(|line| line == &format!("Target: {}", target.display())),
        "missing target preview in {preview:?}"
    );

    let cmds = app.update(Msg::Key(key(KeyCode::Char('m'))));
    assert!(cmds.is_empty());
    assert_eq!(
        app.local_mode
            .pending_organize_confirm
            .as_ref()
            .map(|confirm| confirm.move_count),
        Some(1)
    );

    let cancel = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(cancel.is_empty());
    assert!(app.local_mode.pending_organize_confirm.is_none());
    assert!(audio.exists(), "cancel must leave the inbox audio in place");
    assert!(
        sidecar.exists(),
        "cancel must leave the inbox sidecar in place"
    );

    app.update(Msg::Key(key(KeyCode::Char('m'))));
    let apply = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        apply.as_slice(),
        [Cmd::Local(LocalCmd::ScanRoots { .. })]
    ));
    assert!(!audio.exists(), "audio should move out of the inbox");
    assert!(!sidecar.exists(), "sidecar should move out of the inbox");
    assert!(target.exists(), "audio should land at the organize target");
    assert!(
        crate::downloads::sidecar_path(&target).exists(),
        "sidecar should follow the audio"
    );
    let saved =
        crate::transfer::session::ImportSession::load(session_id).expect("load organized session");
    assert_eq!(saved.rows[0].local_path.as_deref(), Some(target.as_path()));
    assert_eq!(
        app.local_rows_len(),
        0,
        "organized rows should leave the attention inbox"
    );
    assert!(app.status.text.contains("Organized import session"));

    let _ = fs::remove_dir_all(root);
}
