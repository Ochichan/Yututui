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
fn status_facades_set_kind_text_and_clear() {
    let mut app = App::new(100);

    app.set_status_info("ready");
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(app.status.text, "ready");
    assert!(app.dirty);

    app.dirty = false;
    app.set_status_error("failed");
    assert_eq!(app.status.kind, StatusKind::Error);
    assert_eq!(app.status.text, "failed");
    assert!(app.dirty);

    app.clear_status();
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.is_empty());
}

#[test]
fn art_overlay_mask_bits_are_unique_and_fit_u16() {
    use super::artwork::ART_OVERLAY_BITS;

    let mut seen = 0u16;
    for (name, bit) in ART_OVERLAY_BITS {
        assert_ne!(*bit, 0, "{name} bit must be non-zero");
        assert!(
            bit.is_power_of_two(),
            "{name} bit must contain exactly one flag"
        );
        assert_eq!(seen & *bit, 0, "{name} bit overlaps another overlay");
        seen |= *bit;
    }
    assert_eq!(
        ART_OVERLAY_BITS.len(),
        16,
        "u16 overlay mask is fully allocated"
    );
    assert!(seen & (1 << 15) != 0, "highest allocated bit is tracked");
    assert_eq!(seen.count_ones(), ART_OVERLAY_BITS.len() as u32);
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
