use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    PublishError, PublishReport, PublishRequest, load, publish_track, record_server_confirmation,
    save, set_music_folder,
};
use crate::open_subsonic::profile::OpenSubsonicPaths;

const NOW: i64 = 1_800_000_000;

struct Fixture {
    root: PathBuf,
    paths: OpenSubsonicPaths,
    music: PathBuf,
    downloads: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "ytt-publish-ledger-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let data = root.join("data");
        let music = root.join("music");
        let downloads = root.join("downloads");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&music).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        let paths = OpenSubsonicPaths::for_data_root(data);
        Self {
            root,
            paths,
            music,
            downloads,
        }
    }

    fn configured(label: &str) -> Self {
        let fixture = Self::new(label);
        set_music_folder(&fixture.paths, &fixture.music).unwrap();
        fixture
    }

    fn download(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.downloads.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn request(&self, video_id: &str, source: PathBuf) -> PublishRequest {
        PublishRequest {
            video_id: video_id.to_owned(),
            title: "Song".to_owned(),
            artist: Some("Artist".to_owned()),
            album_artist: None,
            album: Some("Album".to_owned()),
            track_number: Some(1),
            source,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_missing_ledger_starts_empty_rather_than_failing() {
    let fixture = Fixture::new("missing");

    let ledger = load(&fixture.paths).unwrap();

    assert!(ledger.music_folder().is_none());
    assert!(ledger.published().is_empty());
    assert!(ledger.pending().is_empty());
}

#[test]
fn a_corrupt_ledger_is_reported_rather_than_silently_reset() {
    let fixture = Fixture::new("corrupt");
    fs::create_dir_all(fixture.paths.root()).unwrap();
    fs::write(
        fixture.paths.publish_store(),
        b"{\"kind\":\"someone else\"}",
    )
    .unwrap();

    // Silently resetting would look like "nothing was ever published" and hide the real problem.
    load(&fixture.paths).unwrap_err();
}

#[test]
fn the_music_folder_round_trips() {
    let fixture = Fixture::new("round-trip");

    set_music_folder(&fixture.paths, &fixture.music).unwrap();

    assert_eq!(
        load(&fixture.paths).unwrap().music_folder(),
        Some(fixture.music.as_path())
    );
}

#[test]
fn publishing_without_a_music_folder_is_refused_before_anything_is_written() {
    let fixture = Fixture::new("unconfigured");
    let source = fixture.download("song.m4a", b"audio");

    let error =
        publish_track(&fixture.paths, &fixture.request("abcdefghijk", source), NOW).unwrap_err();

    assert!(matches!(error, PublishError::NotConfigured));
    assert!(!fixture.music.join("YuTuTui").exists());
}

#[test]
fn a_published_track_is_recorded_with_its_digest() {
    let fixture = Fixture::configured("record");
    let source = fixture.download("song.m4a", b"audio bytes");

    let report =
        publish_track(&fixture.paths, &fixture.request("abcdefghijk", source), NOW).unwrap();

    assert!(matches!(report, PublishReport::Published { .. }));
    let ledger = load(&fixture.paths).unwrap();
    let track = ledger.published().get("abcdefghijk").unwrap();
    assert_eq!(track.len, b"audio bytes".len() as u64);
    assert!(!track.content_sha256.is_empty());
    assert!(!track.confirmed_on_server);
    assert_eq!(track.published_at_unix, NOW);
    // The journal exists only while the copy is in flight.
    assert!(ledger.pending().is_empty());
}

#[test]
fn republishing_the_same_bytes_reports_a_no_op() {
    let fixture = Fixture::configured("republish");
    let source = fixture.download("song.m4a", b"audio bytes");
    let request = fixture.request("abcdefghijk", source);

    publish_track(&fixture.paths, &request, NOW).unwrap();
    let second = publish_track(&fixture.paths, &request, NOW + 60).unwrap();

    assert!(matches!(second, PublishReport::AlreadyPublished { .. }));
    assert_eq!(load(&fixture.paths).unwrap().published().len(), 1);
}

#[test]
fn a_server_confirmation_survives_an_idempotent_republish() {
    let fixture = Fixture::configured("confirmation-survives");
    let source = fixture.download("song.m4a", b"audio bytes");
    let request = fixture.request("abcdefghijk", source);
    publish_track(&fixture.paths, &request, NOW).unwrap();
    record_server_confirmation(&fixture.paths, "abcdefghijk").unwrap();

    publish_track(&fixture.paths, &request, NOW + 60).unwrap();

    let ledger = load(&fixture.paths).unwrap();
    assert!(
        ledger
            .published()
            .get("abcdefghijk")
            .unwrap()
            .confirmed_on_server
    );
    assert_eq!(ledger.confirmed_count(), 1);
}

#[test]
fn a_conflict_leaves_the_existing_record_describing_what_is_on_the_server() {
    let fixture = Fixture::configured("conflict-record");
    let source = fixture.download("song.m4a", b"audio bytes");
    let request = fixture.request("abcdefghijk", source.clone());
    publish_track(&fixture.paths, &request, NOW).unwrap();
    record_server_confirmation(&fixture.paths, "abcdefghijk").unwrap();

    // A re-download changed the local bytes. Nothing can land, because the published name is
    // occupied by content that no longer matches.
    fs::write(&source, b"different audio entirely").unwrap();
    let report = publish_track(&fixture.paths, &request, NOW + 60).unwrap();

    assert!(matches!(report, PublishReport::Conflict { .. }));
    let ledger = load(&fixture.paths).unwrap();
    let track = ledger.published().get("abcdefghijk").unwrap();
    // Nothing was written, so the record must keep describing the published copy rather than the
    // local one, and its confirmation is still true of that copy.
    assert_eq!(track.len, b"audio bytes".len() as u64);
    assert!(track.confirmed_on_server);
    assert!(ledger.pending().is_empty());
}

#[test]
fn changed_bytes_drop_an_earlier_server_confirmation() {
    let fixture = Fixture::configured("confirmation-dropped");
    let source = fixture.download("song.m4a", b"audio bytes");
    let request = fixture.request("abcdefghijk", source.clone());
    publish_track(&fixture.paths, &request, NOW).unwrap();
    record_server_confirmation(&fixture.paths, "abcdefghijk").unwrap();

    // Same track, new bytes, and this time the published copy is gone so the new bytes can land.
    // The confirmation was about the old content and must not carry over to the new.
    fs::remove_file(
        fixture.music.join(
            load(&fixture.paths)
                .unwrap()
                .published()
                .get("abcdefghijk")
                .unwrap()
                .relative_path
                .clone(),
        ),
    )
    .unwrap();
    fs::write(&source, b"different audio entirely").unwrap();
    let report = publish_track(&fixture.paths, &request, NOW + 60).unwrap();

    assert!(matches!(report, PublishReport::Published { .. }));
    let ledger = load(&fixture.paths).unwrap();
    let track = ledger.published().get("abcdefghijk").unwrap();
    assert_eq!(track.len, b"different audio entirely".len() as u64);
    assert!(!track.confirmed_on_server);
    assert_eq!(ledger.confirmed_count(), 0);
}

#[test]
fn a_failed_copy_leaves_the_intent_journalled() {
    let fixture = Fixture::configured("failed-copy");
    let missing = fixture.downloads.join("never-downloaded.m4a");

    let error = publish_track(
        &fixture.paths,
        &fixture.request("abcdefghijk", missing),
        NOW,
    )
    .unwrap_err();

    assert!(matches!(error, PublishError::Transport(_)));
    let ledger = load(&fixture.paths).unwrap();
    // Nothing published, but `publish status` can still tell the user this was attempted.
    assert!(ledger.published().is_empty());
    let pending = ledger.pending().get("abcdefghijk").unwrap();
    assert_eq!(pending.started_at_unix, NOW);
    assert!(pending.stage_basename.starts_with(".ytt-publish-"));
}

#[test]
fn confirming_a_track_that_was_never_published_is_a_no_op() {
    let fixture = Fixture::configured("confirm-unknown");

    record_server_confirmation(&fixture.paths, "abcdefghijk").unwrap();

    assert!(load(&fixture.paths).unwrap().published().is_empty());
}

#[test]
fn the_ledger_refuses_to_grow_without_bound() {
    let fixture = Fixture::configured("cap");
    let mut ledger = load(&fixture.paths).unwrap();
    for index in 0..super::MAX_PUBLISHED_TRACKS {
        ledger.published.insert(
            format!("filler-{index:04}"),
            super::PublishedTrack {
                relative_path: format!("YuTuTui/A/B/{index}.m4a"),
                content_sha256: "0".repeat(64),
                len: 1,
                published_at_unix: NOW,
                confirmed_on_server: false,
            },
        );
    }
    save(&fixture.paths, &ledger).unwrap();
    let source = fixture.download("song.m4a", b"audio");

    let error = publish_track(
        &fixture.paths,
        &fixture.request("one-too-many", source),
        NOW,
    )
    .unwrap_err();

    assert!(matches!(error, PublishError::TooManyPublishedTracks));
}
