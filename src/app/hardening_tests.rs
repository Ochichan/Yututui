use super::*;
use crossterm::event::{KeyEventKind, KeyEventState};
use std::path::PathBuf;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn load_url(cmds: &[Cmd]) -> Option<&str> {
    cmds.iter()
        .flat_map(Cmd::player_commands)
        .find_map(|command| match command {
            PlayerCmd::Load(url) => Some(url.as_str()),
            _ => None,
        })
}

fn hardening_song(id: &str, title: &str, artist: &str) -> Song {
    Song::remote(id, title, artist, "0:10")
}

fn app_with_hardening_favorite() -> App {
    let mut app = App::new(100);
    app.library.favorites = vec![hardening_song("a", "Lovely", "Billie Eilish")];
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
    app
}

fn app_with_hardening_search_results() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![
            hardening_song("a", "Lovely", "Billie Eilish"),
            hardening_song("b", "Bad Guy", "Billie Eilish"),
            hardening_song("c", "Anti-Hero", "Taylor Swift"),
        ],
    });
    assert_eq!(app.search.focus, SearchFocus::Results);
    app
}

#[test]
fn search_input_rejects_over_cap_and_forbidden_chars() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    app.search.input = "a".repeat(crate::util::query::MAX_SEARCH_QUERY_BYTES);

    app.update(Msg::Key(key(KeyCode::Char('b'))));
    assert_eq!(
        app.search.input.len(),
        crate::util::query::MAX_SEARCH_QUERY_BYTES
    );
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("too long"));

    app.search.input.clear();
    app.update(Msg::Key(key(KeyCode::Char('\u{202e}'))));
    assert!(app.search.input.is_empty());
    assert!(app.status.text.contains("Unsupported character"));
}

#[test]
fn search_submit_revalidates_existing_query_buffer() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.input = format!("abc{}", '\u{202e}');

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(cmds.is_empty());
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("Unsupported character"));
}

#[test]
fn library_filter_rejects_over_cap_input() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_hardening_favorite();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.library_ui.filter_query = "a".repeat(crate::util::query::MAX_FILTER_QUERY_BYTES);

    app.update(Msg::Key(key(KeyCode::Char('b'))));

    assert_eq!(
        app.library_ui.filter_query.len(),
        crate::util::query::MAX_FILTER_QUERY_BYTES
    );
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("too long"));
}

#[test]
fn search_filter_rejects_over_cap_input() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = app_with_hardening_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.search_filter.query = "a".repeat(crate::util::query::MAX_FILTER_QUERY_BYTES);

    app.update(Msg::Key(key(KeyCode::Char('b'))));

    assert_eq!(
        app.search_filter.query.len(),
        crate::util::query::MAX_FILTER_QUERY_BYTES
    );
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("too long"));
}

#[test]
fn download_scan_truncation_is_user_visible() {
    let mut app = App::new(100);
    app.update(Msg::Data(DataMsg::DownloadsScanned(
        crate::library::DownloadScan {
            songs: vec![Song::local_file(PathBuf::from("/tmp/a.m4a"))],
            truncated: true,
            limit: 999,
        },
    )));
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert!(app.status.text.contains("999"));
    assert!(app.status.text.contains("hidden") || app.status.text.contains("숨김"));
}

#[test]
fn download_dir_error_updates_status_without_track_state() {
    let mut app = App::new(100);
    app.update(Msg::Download(DownloadMsg::DirError {
        error: "queue full".to_owned(),
    }));
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
fn scrobble_append_recovery_does_not_claim_zero_items_are_waiting() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);

    app.update(Msg::Scrobble(
        crate::scrobble::ScrobbleEvent::QueueStalled { pending: 0 },
    ));

    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.status.text.contains("recovered"));
    assert!(!app.status.text.contains("0 scrobbles waiting"));
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
fn art_overlay_mask_bits_are_unique_and_fit_u32() {
    use super::artwork::ART_OVERLAY_BITS;

    let mut seen = 0u32;
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
        18,
        "all assigned u32 overlay bits are inventoried"
    );
    assert!(seen & (1 << 19) != 0, "highest allocated bit is tracked");
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
