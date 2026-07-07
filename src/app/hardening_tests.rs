use super::*;
use std::path::PathBuf;

fn load_url(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter().find_map(|c| match c {
        Cmd::Player(PlayerCmd::Load(url)) => Some(url.as_str()),
        _ => None,
    })
}

#[test]
fn download_scan_truncation_is_user_visible() {
    let mut app = App::new(100);
    app.update(Msg::DownloadsScanned(crate::library::DownloadScan {
        songs: vec![Song::local_file(PathBuf::from("/tmp/a.m4a"))],
        truncated: true,
        limit: 999,
    }));
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert!(app.status.text.contains("999"));
    assert!(app.status.text.contains("hidden") || app.status.text.contains("숨김"));
}

#[test]
fn download_dir_error_updates_status_without_track_state() {
    let mut app = App::new(100);
    app.update(Msg::DownloadDirError {
        error: "queue full".to_owned(),
    });
    assert!(app.downloads.active.is_empty());
    assert!(app.status.text.contains("queue full"));
}

#[test]
fn scrobble_queue_dropped_event_updates_status() {
    let mut app = App::new(100);
    app.update(Msg::Scrobble(
        crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 3 },
    ));
    assert!(app.status.text.contains("3"));
    assert_eq!(app.status.kind, StatusKind::Error);
}

#[test]
fn load_song_skips_invalid_external_playback_url() {
    let mut app = App::new(100);
    let bad = Song::from_source(
        SearchSource::RadioBrowser,
        "bad",
        "Bad Radio",
        "",
        "",
        crate::api::PlayableRef::RadioStream {
            url: "file:///etc/passwd".to_owned(),
        },
    );
    app.queue
        .set(vec![bad, Song::remote("id1", "t-id1", "a", "1:00")], 0);
    let cmds = app.load_song(app.queue.current().cloned());
    let url = load_url(&cmds).expect("next playable track should load");
    assert!(!url.starts_with("file:"));
    assert!(url.contains("music.youtube.com/watch") && url.contains("id1"));
}
