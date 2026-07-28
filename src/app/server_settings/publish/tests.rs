use std::path::PathBuf;

use super::*;
use crate::open_subsonic::LibraryScanRequest as Scan;

fn downloaded(video_id: &str) -> Song {
    let mut song = Song::local_file(PathBuf::from(format!("/tmp/{video_id}.m4a")));
    song.video_id = video_id.to_owned();
    song.title = "Song".to_owned();
    song.local_path = Some(PathBuf::from(format!("/tmp/{video_id}.m4a")));
    song
}

fn outcome(report: TrackPublishReport, scan: Scan) -> TrackPublishOutcome {
    TrackPublishOutcome { report, scan }
}

#[test]
fn publishing_a_downloaded_track_emits_one_generation_stamped_command() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;

    let commands = app.start_publish_track_to_server(&downloaded("abcdefghijk"));

    let generation = app.server.settings.generation;
    assert!(matches!(
        commands.as_slice(),
        [Cmd::MusicServer(MusicServerCommand::PublishTrack {
            generation: emitted,
            video_id,
        })] if *emitted == generation && video_id == "abcdefghijk"
    ));
    assert_eq!(
        app.server.settings.busy,
        Some(MusicServerBusy::PlaylistRecovery)
    );
}

#[test]
fn a_track_without_a_downloaded_file_is_refused_before_any_command() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;
    let mut song = downloaded("abcdefghijk");
    song.local_path = None;

    let commands = app.start_publish_track_to_server(&song);

    assert!(commands.is_empty());
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.server.settings.busy.is_none());
}

#[test]
fn a_second_publish_is_ignored_while_one_is_in_flight() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;
    assert_eq!(
        app.start_publish_track_to_server(&downloaded("first"))
            .len(),
        1
    );

    let commands = app.start_publish_track_to_server(&downloaded("second"));

    assert!(commands.is_empty());
}

#[test]
fn a_stale_result_is_dropped_without_touching_the_status() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;
    app.start_publish_track_to_server(&downloaded("abcdefghijk"));
    let stale = app.server.settings.generation.wrapping_sub(1);
    app.status.text = "untouched".to_owned();

    let commands = app.finish_music_server_event(MusicServerEvent::TrackPublished {
        generation: stale,
        result: Ok(outcome(TrackPublishReport::Published, Scan::Started)),
    });

    assert!(commands.is_empty());
    assert_eq!(app.status.text, "untouched");
    // The in-flight publish still owns the busy slot; a stale reply must not free it.
    assert_eq!(
        app.server.settings.busy,
        Some(MusicServerBusy::PlaylistRecovery)
    );
}

#[test]
fn a_conflict_reports_that_nothing_was_replaced() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;

    app.finish_track_published(Ok(outcome(TrackPublishReport::Conflict, Scan::Started)));

    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(
        app.status.text.contains("nothing was replaced"),
        "{}",
        app.status.text
    );
    // A conflict says nothing about scanning, because nothing was published to scan for.
    assert!(
        !app.status.text.contains("rescanning"),
        "{}",
        app.status.text
    );
    assert!(app.server.settings.busy.is_none());
}

#[test]
fn every_scan_outcome_keeps_a_successful_copy_successful() {
    for scan in [
        Scan::Started,
        Scan::NoServer,
        Scan::Unsupported,
        Scan::NotPermitted,
        Scan::Unavailable,
    ] {
        let mut app = App::new(50);
        app.server.settings.summary.configured = true;

        app.finish_track_published(Ok(outcome(TrackPublishReport::Published, scan)));

        // The bytes are in the music folder before the scan is asked for, so no scan outcome may
        // demote a completed publication into an error.
        assert_eq!(app.status.kind, StatusKind::Info, "{scan:?}");
        assert!(
            app.status.text.contains("Copied to the music server"),
            "{scan:?}"
        );
        assert!(app.server.settings.busy.is_none(), "{scan:?}");
    }
}

#[test]
fn an_idempotent_republish_says_it_changed_nothing() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;

    app.finish_track_published(Ok(outcome(
        TrackPublishReport::AlreadyPublished,
        Scan::Unsupported,
    )));

    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("unchanged"), "{}", app.status.text);
}

#[test]
fn a_failure_surfaces_its_own_label_and_frees_the_busy_slot() {
    let mut app = App::new(50);
    app.server.settings.summary.configured = true;
    app.start_publish_track_to_server(&downloaded("abcdefghijk"));

    app.finish_track_published(Err(MusicServerFailure::Unavailable));

    assert_eq!(app.status.kind, StatusKind::Error);
    assert_eq!(app.status.text, MusicServerFailure::Unavailable.label());
    assert!(app.server.settings.busy.is_none());
}
