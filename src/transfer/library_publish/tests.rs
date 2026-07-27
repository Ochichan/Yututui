use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    LIBRARY_SUBTREE, PublishOutcome, PublishTrack, plan_publish_path, publish_into_library,
};

fn temp_root(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "ytt-library-publish-{label}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

struct Fixture {
    root: PathBuf,
    music: PathBuf,
    downloads: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = temp_root(label);
        let music = root.join("music");
        let downloads = root.join("downloads");
        fs::create_dir_all(&music).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        Self {
            root,
            music,
            downloads,
        }
    }

    fn download(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.downloads.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn track<'a>(video_id: &'a str, title: &'a str) -> PublishTrack<'a> {
    PublishTrack {
        video_id,
        title,
        artist: Some("Some Artist"),
        album_artist: None,
        album: Some("Some Album"),
        track_number: Some(3),
    }
}

#[test]
fn a_track_lands_under_the_library_subtree_with_its_exact_bytes() {
    let fixture = Fixture::new("happy-path");
    let source = fixture.download("song.m4a", b"audio bytes");
    let plan = plan_publish_path(&track("dQw4w9WgXcQ", "Some Song"), Some("m4a"));

    let outcome = publish_into_library(&source, &fixture.music, &plan).unwrap();

    let published = match outcome {
        PublishOutcome::Published(published) => published,
        other => panic!("expected a fresh publication, got {other:?}"),
    };
    assert_eq!(
        published.relative_path,
        "YuTuTui/Some Artist/Some Album/03 - Some Song [dQw4w9WgXcQ].m4a"
    );
    assert_eq!(published.len, b"audio bytes".len() as u64);
    let landed = fixture.music.join(&published.relative_path);
    assert_eq!(fs::read(&landed).unwrap(), b"audio bytes");
    // The source is a copy source, never a move source.
    assert!(source.is_file());
}

#[test]
fn publication_never_escapes_the_library_subtree() {
    let fixture = Fixture::new("subtree");
    let source = fixture.download("song.m4a", b"audio");
    let plan = plan_publish_path(
        &PublishTrack {
            video_id: "../../escape",
            title: "../../../etc/passwd",
            artist: Some("../.."),
            album_artist: None,
            album: Some("/etc"),
            track_number: None,
        },
        Some("m4a"),
    );

    publish_into_library(&source, &fixture.music, &plan).unwrap();

    assert!(plan.relative_dir.starts_with(LIBRARY_SUBTREE));
    for component in plan.relative_dir.components() {
        assert_ne!(component.as_os_str(), "..");
    }
    assert!(fixture.music.join(LIBRARY_SUBTREE).is_dir());
    assert!(!fixture.root.join("escape").exists());
    assert!(!fixture.root.join("etc").exists());
}

#[test]
fn republishing_identical_bytes_writes_nothing() {
    let fixture = Fixture::new("republish-identical");
    let source = fixture.download("song.m4a", b"audio bytes");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));

    let first = publish_into_library(&source, &fixture.music, &plan).unwrap();
    let landed = fixture.music.join(plan.relative_path());
    let first_modified = fs::metadata(&landed).unwrap().modified().unwrap();

    let second = publish_into_library(&source, &fixture.music, &plan).unwrap();

    assert!(matches!(first, PublishOutcome::Published(_)));
    match second {
        PublishOutcome::AlreadyPublished(published) => {
            assert_eq!(published.relative_path, plan.relative_path());
        }
        other => panic!("expected an idempotent no-op, got {other:?}"),
    }
    assert_eq!(
        fs::metadata(&landed).unwrap().modified().unwrap(),
        first_modified
    );
}

#[test]
fn a_different_file_at_the_final_name_is_reported_and_never_replaced() {
    let fixture = Fixture::new("conflict");
    let source = fixture.download("song.m4a", b"mine");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));
    let landed = fixture.music.join(plan.relative_path());
    fs::create_dir_all(landed.parent().unwrap()).unwrap();
    fs::write(&landed, b"someone else's file").unwrap();

    let outcome = publish_into_library(&source, &fixture.music, &plan).unwrap();

    match outcome {
        PublishOutcome::Conflict {
            relative_path,
            published_sha256,
            local_sha256,
        } => {
            assert_eq!(relative_path, plan.relative_path());
            assert_ne!(published_sha256, local_sha256);
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(fs::read(&landed).unwrap(), b"someone else's file");
}

#[test]
fn a_missing_publication_is_copied_again() {
    let fixture = Fixture::new("recopy");
    let source = fixture.download("song.m4a", b"audio bytes");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));

    publish_into_library(&source, &fixture.music, &plan).unwrap();
    let landed = fixture.music.join(plan.relative_path());
    fs::remove_file(&landed).unwrap();

    let outcome = publish_into_library(&source, &fixture.music, &plan).unwrap();

    assert!(matches!(outcome, PublishOutcome::Published(_)));
    assert_eq!(fs::read(&landed).unwrap(), b"audio bytes");
}

#[test]
fn an_interrupted_stage_is_reused_rather_than_duplicated_or_deleted() {
    let fixture = Fixture::new("stage-reuse");
    let source = fixture.download("song.m4a", b"audio bytes");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));
    let directory = fixture.music.join(&plan.relative_dir);
    fs::create_dir_all(&directory).unwrap();
    let stage = directory.join(&plan.stage_basename);
    // A crash mid-copy leaves exactly this: a partial file under the deterministic stage name.
    fs::write(&stage, b"partial garbage from an interrupted attempt").unwrap();

    let outcome = publish_into_library(&source, &fixture.music, &plan).unwrap();

    assert!(matches!(outcome, PublishOutcome::Published(_)));
    assert_eq!(
        fs::read(fixture.music.join(plan.relative_path())).unwrap(),
        b"audio bytes"
    );
    // The stage was promoted, so it is gone by rename, not by an unlink this module performed.
    assert!(!stage.exists());
    let strays: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != plan.basename.as_os_str())
        .collect();
    assert!(strays.is_empty(), "unexpected leftovers: {strays:?}");
}

#[test]
fn the_stage_name_is_hidden_from_server_scanners() {
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));
    assert!(
        plan.stage_basename.to_string_lossy().starts_with('.'),
        "the stage must be dot-prefixed so a scanner ignores an interrupted copy"
    );
    assert!(plan.stage_basename.to_string_lossy().ends_with(".part"));
}

#[test]
fn a_symlinked_source_is_refused() {
    let fixture = Fixture::new("symlink-source");
    let real = fixture.download("real.m4a", b"audio");
    let link = fixture.downloads.join("link.m4a");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &link).unwrap();
    #[cfg(not(unix))]
    {
        let _ = &real;
        return;
    }
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));

    let error = publish_into_library(&link, &fixture.music, &plan).unwrap_err();

    assert!(
        error.to_string().contains("not a regular file"),
        "unexpected error: {error}"
    );
}

#[test]
#[cfg(unix)]
fn a_symlinked_library_subtree_is_refused_rather_than_followed() {
    let fixture = Fixture::new("symlink-subtree");
    let outside = fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.music.join(LIBRARY_SUBTREE)).unwrap();
    let source = fixture.download("song.m4a", b"audio");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));

    let error = publish_into_library(&source, &fixture.music, &plan).unwrap_err();

    assert!(
        error.to_string().contains("non-directory artifact scope"),
        "unexpected error: {error}"
    );
    assert!(!outside.join("Some Artist").exists());
}

#[test]
#[cfg(unix)]
fn a_group_readable_music_folder_publishes_a_group_readable_track() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let fixture = Fixture::new("group-readable");
    fs::set_permissions(&fixture.music, fs::Permissions::from_mode(0o750)).unwrap();
    let source = fixture.download("song.m4a", b"audio");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));

    publish_into_library(&source, &fixture.music, &plan).unwrap();

    // A server daemon running under its own account has to be able to read this.
    let landed = fixture.music.join(plan.relative_path());
    assert_eq!(fs::metadata(&landed).unwrap().mode() & 0o777, 0o640);
    assert_eq!(
        fs::metadata(fixture.music.join(LIBRARY_SUBTREE))
            .unwrap()
            .mode()
            & 0o777,
        0o750
    );
}

#[test]
#[cfg(unix)]
fn a_private_music_folder_keeps_published_tracks_private() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let fixture = Fixture::new("private-root");
    fs::set_permissions(&fixture.music, fs::Permissions::from_mode(0o700)).unwrap();
    let source = fixture.download("song.m4a", b"audio");
    let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some("m4a"));

    publish_into_library(&source, &fixture.music, &plan).unwrap();

    let landed = fixture.music.join(plan.relative_path());
    assert_eq!(fs::metadata(&landed).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn missing_metadata_falls_back_without_producing_an_empty_component() {
    let plan = plan_publish_path(
        &PublishTrack {
            video_id: "abcdefghijk",
            title: "Song",
            artist: Some("   "),
            album_artist: None,
            album: None,
            track_number: None,
        },
        None,
    );

    assert_eq!(
        plan.relative_dir,
        Path::new(LIBRARY_SUBTREE)
            .join("Unknown Artist")
            .join("Unknown Album")
    );
    assert_eq!(plan.basename.to_string_lossy(), "Song [abcdefghijk]");
}

#[test]
fn the_album_artist_wins_over_the_track_artist() {
    let plan = plan_publish_path(
        &PublishTrack {
            video_id: "abcdefghijk",
            title: "Song",
            artist: Some("Featured Guest"),
            album_artist: Some("Album Owner"),
            album: Some("Album"),
            track_number: None,
        },
        Some("opus"),
    );

    assert!(plan.relative_dir.ends_with(Path::new("Album Owner/Album")));
    assert!(plan.basename.to_string_lossy().ends_with(".opus"));
}

#[test]
fn a_hostile_extension_is_dropped_rather_than_written() {
    for hostile in ["../sh", "m4a/../..", "", ".", "exe\0", "averylongextension"] {
        let plan = plan_publish_path(&track("abcdefghijk", "Song"), Some(hostile));
        assert_eq!(
            plan.basename.to_string_lossy(),
            "03 - Song [abcdefghijk]",
            "extension {hostile:?} must not reach the filename"
        );
    }
}
