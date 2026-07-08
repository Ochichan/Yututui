use super::*;

#[test]
fn youtube_id_and_share_url_distinguish_local_from_remote() {
    // Remote: `video_id` is the YouTube ID, so the share URL is built from it.
    let remote = Song::remote("abc123", "t", "a", "3:00");
    assert_eq!(remote.youtube_id(), Some("abc123"));
    assert_eq!(
        remote.share_url().as_deref(),
        Some("https://www.youtube.com/watch?v=abc123")
    );

    // Pure local file: never sourced online, so there is no URL to share.
    let local = Song::local_file(PathBuf::from("/tmp/song.m4a"));
    assert_eq!(local.youtube_id(), None);
    assert_eq!(local.share_url(), None);

    // Downloaded catalog track: local on disk but still knows its YouTube URL (#3).
    let downloaded = remote.with_local_path(PathBuf::from("/tmp/abc123.m4a"));
    assert!(downloaded.is_local());
    assert_eq!(downloaded.youtube_id(), Some("abc123"));
    assert_eq!(
        downloaded.share_url().as_deref(),
        Some("https://www.youtube.com/watch?v=abc123")
    );
}

#[test]
fn copy_link_copies_youtube_url_for_remote_track() {
    let mut app = app_playing(1, 0); // queue of remote songs (video_id "id0")
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert_eq!(app.status.kind, StatusKind::Info);
    assert_eq!(
        app.status.text,
        t!(
            "✓ Link copied to clipboard",
            "✓ 링크가 클립보드에 복사됐어요"
        )
    );
}

#[test]
fn copy_link_warns_for_local_only_track() {
    let mut app = App::new(100);
    app.queue.set(
        vec![Song::local_file(PathBuf::from("/tmp/copylink-local.m4a"))],
        0,
    );
    app.mode = Mode::Player;
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert_ne!(app.status.kind, StatusKind::Info);
    assert_eq!(
        app.status.text,
        t!(
            "This track is local-only — no YouTube link",
            "로컬 전용 트랙이라 유튜브 링크가 없어요"
        )
    );
}

/// A bare, pre-enrichment local entry (e.g. a `local:` track persisted to history before the
/// fix, or a plain scanned file): `local:` identity, no `yt_video_id`, real `local_path`.

#[test]
fn downloadable_batch_skips_local_downloaded_and_dupes() {
    let mut app = App::new(100);
    // Remember one track as already downloaded in a past session (manifest keeps its YT id).
    let past = fsong("keep", "Kept", "A").with_local_path(PathBuf::from("/dl/keep.m4a"));
    app.download_store.record(&past);

    let batch = app.downloadable_batch(vec![
        fsong("a", "A", "x"),               // fresh -> keep
        bare_local("/dl/local.m4a", "Loc"), // local file -> skip
        fsong("keep", "Kept", "A"),         // already downloaded -> skip
        fsong("a", "A dup", "x"),           // duplicate id within batch -> skip
        fsong("b", "B", "y"),               // fresh -> keep
    ]);

    let ids: Vec<&str> = batch.iter().map(|s| s.video_id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b"]);
}

#[test]
fn bulk_download_confirm_flow_queues_and_emits() {
    let mut app = App::new(100);

    // A fully-skipped selection opens no modal and emits no downloads.
    let cmds = app.open_confirm_download(vec![bare_local("/dl/x.m4a", "X")]);
    assert!(cmds.is_empty());
    assert!(app.library_ui.confirm_download.is_none());

    // A real batch raises the confirm popup carrying the deduped count — still no downloads yet.
    let cmds = app.open_confirm_download(vec![fsong("a", "A", "x"), fsong("b", "B", "y")]);
    assert!(cmds.is_empty());
    assert_eq!(
        app.library_ui.confirm_download.as_ref().map(Vec::len),
        Some(2)
    );

    // Confirming clears the modal, queues the batch, and emits one Cmd::Download per track.
    let cmds = app.confirm_download_apply();
    let downloads = cmds
        .iter()
        .filter(|c| matches!(c, Cmd::Download(_)))
        .count();
    assert_eq!(downloads, 2);
    assert!(app.library_ui.confirm_download.is_none());
    assert_eq!(app.downloads.dispatched, 2);
}

#[test]
fn bulk_download_stays_under_channel_bound_and_drips() {
    let mut app = App::new(100);
    let many: Vec<Song> = (0..100)
        .map(|i| fsong(&format!("id{i}"), "T", "A"))
        .collect();
    app.library_ui.confirm_download = Some(app.downloadable_batch(many));

    let cmds = app.confirm_download_apply();
    // Only up to the in-flight cap dispatches; the overflow waits in `pending` (no channel flood).
    assert_eq!(
        cmds.iter()
            .filter(|c| matches!(c, Cmd::Download(_)))
            .count(),
        96
    );
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 4);

    // Each completion frees a slot and drips the next queued download in, holding at the cap.
    app.update(Msg::DownloadDone {
        video_id: "id0".to_string(),
        path: String::new(),
    });
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 3);
}

#[test]
fn local_file_recovers_embedded_id_from_filename() {
    // Our downloader names files `Title [<id>].m4a`; a rescan recovers the id + clean title.
    let tagged = Song::local_file(PathBuf::from("/tmp/My Song [dQw4w9WgXcQ].m4a"));
    assert_eq!(tagged.title, "My Song");
    assert_eq!(tagged.youtube_id(), Some("dQw4w9WgXcQ"));
    assert_eq!(
        tagged.share_url().as_deref(),
        Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );

    // A plain filename stays local-only.
    let plain = Song::local_file(PathBuf::from("/tmp/Just A File.m4a"));
    assert_eq!(plain.title, "Just A File");
    assert_eq!(plain.youtube_id(), None);

    // A bracketed-but-not-an-id title is not mistaken for a tag (not 11 id chars).
    let bracketed = Song::local_file(PathBuf::from("/tmp/Live Mix [Vol. 3].m4a"));
    assert_eq!(bracketed.title, "Live Mix [Vol. 3]");
    assert_eq!(bracketed.youtube_id(), None);

    // Documented false-positive boundary: any trailing `[…]` whose contents are exactly 11
    // id-shaped chars ([A-Za-z0-9_-]) IS treated as an embedded id, even ordinary words —
    // the 11-char shape is heuristic, not proof. A 10- or 12-char bracket does not match.
    let collision = Song::local_file(PathBuf::from("/tmp/Song [hello_world].m4a")); // 11 chars
    assert_eq!(collision.title, "Song");
    assert_eq!(collision.youtube_id(), Some("hello_world"));
    let too_short = Song::local_file(PathBuf::from("/tmp/Song [helloworld].m4a")); // 10 chars
    assert_eq!(too_short.youtube_id(), None);
}

#[test]
fn enrich_downloads_restores_id_from_manifest_then_title_match() {
    let mut app = App::new(100);
    // Manifest remembers an enriched download (real artist + YouTube id), keyed by video_id.
    let rich = Song::remote("dQw4w9WgXcQ", "Title", "Real Artist", "3:00")
        .with_local_path(PathBuf::from("/tmp/whatever.m4a"));
    app.download_store.record(&rich);
    let from_manifest = app.enrich_downloads(vec![bare_local("/tmp/whatever.m4a", "Title")]);
    assert_eq!(from_manifest[0].artist, "Real Artist");
    assert_eq!(from_manifest[0].youtube_id(), Some("dQw4w9WgXcQ"));

    // Legacy best-effort: an id-less scanned file borrows the id of a remote favorite with the
    // same normalized title.
    app.library.favorites = vec![Song::remote("favmatch1234", "My Tune", "A", "3:00")];
    let from_title = app.enrich_downloads(vec![bare_local("/tmp/plain.m4a", "my tune")]);
    assert_eq!(from_title[0].youtube_id(), Some("favmatch1234"));

    // A *local-only* favorite (no YouTube origin) must NOT be borrowed from.
    app.library.favorites = vec![bare_local("/tmp/other.m4a", "Lonely")];
    let unmatched = app.enrich_downloads(vec![bare_local("/tmp/lonely.m4a", "Lonely")]);
    assert_eq!(unmatched[0].youtube_id(), None);
}

#[test]
fn copy_link_recovers_id_from_filename_for_bare_local_track() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    // A bare queue entry (no yt id) whose on-disk file carries the `[id]` tag.
    app.queue.set(
        vec![bare_local(
            "/tmp/Great Song [dQw4w9WgXcQ].m4a",
            "Great Song",
        )],
        0,
    );
    app.mode = Mode::Player;
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert_eq!(
        app.status.kind,
        StatusKind::Info,
        "id recovered from filename"
    );
}

#[test]
fn copy_link_recovers_id_via_title_match_against_favorites() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.library.favorites = vec![Song::remote("favmatch1234", "My Tune", "A", "3:00")];
    app.queue
        .set(vec![bare_local("/tmp/plain.m4a", "My Tune")], 0);
    app.mode = Mode::Player;
    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert_eq!(
        app.status.kind,
        StatusKind::Info,
        "id recovered by title match"
    );
}

#[test]
fn copy_link_works_on_all_tab_when_history_holds_a_bare_local_entry() {
    // Finding 1: All-tab dedup prefers the history entry over the enriched download for the same
    // title. The history entry is bare (`local:`, no yt id), but its file is `[id]`-tagged, so the
    // copy-time recovery still produces a YouTube URL.
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    let path = "/tmp/Shared [dQw4w9WgXcQ].m4a";
    app.library.history.push_front(bare_local(path, "Shared")); // pre-fix play, wins All-tab dedup
    app.library_ui.downloaded =
        vec![bare_local(path, "Shared").with_yt_id("dQw4w9WgXcQ".to_owned())];

    app.update(Msg::Key(key(KeyCode::Char('l')))); // Library, All tab
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter))); // play the All-tab current row
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(load_url(&cmds), Some(path));
    assert!(
        app.queue.current().unwrap().youtube_id().is_none(),
        "the bare history entry plays"
    );

    app.update(Msg::Key(key(KeyCode::Char('y'))));
    assert_eq!(
        app.status.kind,
        StatusKind::Info,
        "copy still recovers the YouTube URL"
    );
}

#[test]
fn download_done_records_manifest_and_saves() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    let remote = Song::remote("dQw4w9WgXcQ", "Title", "Real Artist", "3:00");
    app.start_download(remote); // populates downloads.sources

    let cmds = app.update(Msg::DownloadDone {
        video_id: "dQw4w9WgXcQ".to_owned(),
        path: "/tmp/Title [dQw4w9WgXcQ].m4a".to_owned(),
    });
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Downloads))),
        "a completed download persists the manifest"
    );
    assert_eq!(
        app.library_ui.downloaded[0].youtube_id(),
        Some("dQw4w9WgXcQ")
    );
    // The manifest remembers it: a later bare scan of the same file re-enriches.
    let vid = app.library_ui.downloaded[0].video_id.clone();
    let scanned = Song {
        video_id: vid,
        ..Song::local_file(PathBuf::from("/tmp/x.m4a"))
    };
    let out = app.download_store.enrich(vec![scanned]);
    assert_eq!(out[0].youtube_id(), Some("dQw4w9WgXcQ"));
    assert_eq!(out[0].artist, "Real Artist");
}

#[test]
fn download_done_updates_import_session_row() {
    let mut app = App::new(100);
    let session_id = "sp2yt-app-download-row";
    let session = crate::transfer::session::ImportSession {
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-00003".to_owned(),
            source_order: 3,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            title: "Title".to_owned(),
            artists: vec!["Artist".to_owned()],
            selected_key: Some("dQw4w9WgXcQ".to_owned()),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    };
    session.save().expect("save import session");
    let remote = Song::remote("dQw4w9WgXcQ", "Title", "Artist", "3:00")
        .with_import_session(Some(session_id.to_owned()), Some(3));
    app.start_download(remote);

    app.update(Msg::DownloadDone {
        video_id: "dQw4w9WgXcQ".to_owned(),
        path: "/tmp/Title [dQw4w9WgXcQ].m4a".to_owned(),
    });

    let session = crate::transfer::session::ImportSession::load(session_id)
        .expect("load updated import session");
    assert!(session.rows[0].written);
    assert_eq!(
        session.rows[0].local_path,
        Some(PathBuf::from("/tmp/Title [dQw4w9WgXcQ].m4a"))
    );
    assert_eq!(session.counts.written, 1);
}

#[test]
fn download_error_updates_import_session_row() {
    let mut app = App::new(100);
    let session_id = "sp2yt-app-download-error";
    let session = crate::transfer::session::ImportSession {
        session_id: session_id.to_owned(),
        job_id: session_id.to_owned(),
        rows: vec![crate::transfer::session::ImportSessionRow {
            row_id: "row-00002".to_owned(),
            source_order: 2,
            status: crate::transfer::session::ImportSessionRowStatus::Matched,
            title: "Title".to_owned(),
            artists: vec!["Artist".to_owned()],
            selected_key: Some("dQw4w9WgXcQ".to_owned()),
            ..crate::transfer::session::ImportSessionRow::default()
        }],
        ..crate::transfer::session::ImportSession::default()
    };
    session.save().expect("save import session");
    let remote = Song::remote("dQw4w9WgXcQ", "Title", "Artist", "3:00")
        .with_import_session(Some(session_id.to_owned()), Some(2));
    app.start_download(remote);

    app.update(Msg::DownloadError {
        video_id: "dQw4w9WgXcQ".to_owned(),
        error: "private video needs login".to_owned(),
    });

    let session = crate::transfer::session::ImportSession::load(session_id)
        .expect("load updated import session");
    assert!(!session.rows[0].written);
    assert_eq!(session.rows[0].local_path, None);
    assert_eq!(session.rows[0].errors, vec!["private video needs login"]);
    assert_eq!(session.counts.written, 0);
}

#[test]
fn download_done_with_empty_path_does_not_save() {
    let mut app = App::new(100);
    let cmds = app.update(Msg::DownloadDone {
        video_id: "x".to_owned(),
        path: "   ".to_owned(),
    });
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Downloads)))
    );
}

// --- library multi-select delete (drag + Delete), per-tab semantics ------

/// A real, empty audio file in the temp dir, named uniquely so parallel tests don't clash.

#[test]
fn library_delete_range_removes_from_favorites() {
    let mut app = App::new(100);
    app.library
        .toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.library
        .toggle_favorite(&Song::remote("b", "tb", "x", "0:10"));
    app.library
        .toggle_favorite(&Song::remote("c", "tc", "x", "0:10")); // [c, b, a]
    open_library_tab(&mut app, LibraryTab::Favorites);
    // Cursor on row 0, drag-anchor on row 1: the selection spans rows 0..=1 (c, b).
    app.library_ui.selected = 0;
    app.library_ui.anchor = 1;
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    let ids: Vec<&str> = app
        .library
        .favorites
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["a"]);
    assert_eq!(app.library_ui.selected, 0);
}

#[test]
fn library_delete_range_removes_from_history() {
    let mut app = App::new(100);
    app.library
        .record_play(&Song::remote("a", "ta", "x", "0:10"));
    app.library
        .record_play(&Song::remote("b", "tb", "x", "0:10"));
    app.library
        .record_play(&Song::remote("c", "tc", "x", "0:10")); // front->back: c, b, a
    open_library_tab(&mut app, LibraryTab::History);
    app.library_ui.selected = 1;
    app.library_ui.anchor = 2; // rows 1..=2 = b, a
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    let ids: Vec<&str> = app
        .library
        .history
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["c"]);
}

#[test]
fn library_page_and_jump_keys_move_the_cursor() {
    let mut app = App::new(100);
    for i in 0..30 {
        app.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    open_library_tab(&mut app, LibraryTab::History);
    let len = app.library_len();
    assert_eq!(len, 30);
    app.library_ui.selected = 0;
    app.library_ui.anchor = 0;
    // A 12-row viewport pages by 11 (one row of overlap).
    app.bridges.list_viewport_rows.set(12);

    app.update(Msg::Key(key(KeyCode::PageDown)));
    assert_eq!(app.library_ui.selected, 11);
    assert_eq!(app.library_ui.anchor, 11);
    app.update(Msg::Key(key(KeyCode::PageUp)));
    assert_eq!(app.library_ui.selected, 0);

    app.update(Msg::Key(key(KeyCode::End)));
    assert_eq!(app.library_ui.selected, len - 1);
    assert_eq!(app.library_ui.anchor, len - 1);
    app.update(Msg::Key(key(KeyCode::Home)));
    assert_eq!(app.library_ui.selected, 0);
    assert_eq!(app.library_ui.anchor, 0);
}

#[test]
fn search_page_and_jump_keys_move_the_cursor() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(30);
    app.search.selected = 0;
    app.bridges.list_viewport_rows.set(12);

    app.update(Msg::Key(key(KeyCode::PageDown)));
    assert_eq!(app.search.selected, 11);
    app.update(Msg::Key(key(KeyCode::End)));
    assert_eq!(app.search.selected, 29);
    app.update(Msg::Key(key(KeyCode::PageUp)));
    assert_eq!(app.search.selected, 18);
    app.update(Msg::Key(key(KeyCode::Home)));
    assert_eq!(app.search.selected, 0);
}

#[test]
fn wheel_scrolls_the_viewport_not_the_selection() {
    use crate::ui::scroll::SCROLLOFF;
    // The wheel moves the viewport offset by MOUSE_SCROLL_LINES (3), clamped at the ends,
    // and leaves the selection where it is (it may scroll out of view). `resolve` records
    // the viewport (normally a render's job) and reads the honored offset back.
    let mut app = App::new(100);
    for i in 0..20 {
        app.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    open_library_tab(&mut app, LibraryTab::History);
    app.library_ui.selected = 0;
    let len = app.library_len();
    app.bridges
        .library_scroll
        .resolve(app.library_ui.selected, 10, len, SCROLLOFF);

    app.update(Msg::MouseScroll {
        up: false,
        col: 0,
        row: 0,
        ctrl: false,
    });
    assert_eq!(app.library_ui.selected, 0); // selection untouched by the wheel
    assert_eq!(
        app.bridges
            .library_scroll
            .resolve(app.library_ui.selected, 10, len, SCROLLOFF),
        3
    );
    app.update(Msg::MouseScroll {
        up: true,
        col: 0,
        row: 0,
        ctrl: false,
    });
    assert_eq!(
        app.bridges
            .library_scroll
            .resolve(app.library_ui.selected, 10, len, SCROLLOFF),
        0
    ); // clamped at top

    // Search: same decoupling, clamped at the last page.
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(20);
    app.search.selected = 19;
    let len = app.search.results.len();
    app.bridges
        .search_scroll
        .resolve(app.search.selected, 10, len, SCROLLOFF); // offset -> last page (10)
    app.update(Msg::MouseScroll {
        up: false,
        col: 0,
        row: 0,
        ctrl: false,
    });
    assert_eq!(app.search.selected, 19); // selection untouched
    assert_eq!(
        app.bridges
            .search_scroll
            .resolve(app.search.selected, 10, len, SCROLLOFF),
        10
    ); // clamped at end
    app.update(Msg::MouseScroll {
        up: true,
        col: 0,
        row: 0,
        ctrl: false,
    });
    assert_eq!(
        app.bridges
            .search_scroll
            .resolve(app.search.selected, 10, len, SCROLLOFF),
        7
    );
}

#[test]
fn settings_click_on_a_visible_row_does_not_scroll() {
    // The Settings field list now keeps a persistent offset (scrolloff = 0), so clicking a row
    // that is already on-screen focuses it in place instead of snapping the viewport (the old
    // `ListState::default()` re-derived the offset from 0 each frame and pinned it to an edge).
    let mut app = app_playing(1, 0);
    app.open_settings();
    // Render of a long tab scrolled well down: focus display row 30 in a viewport of 10.
    let off = app.bridges.settings_scroll.resolve(30, 10, 40, 0);
    assert!(
        (off..off + 10).contains(&30),
        "row 30 is visible at offset {off}"
    );
    // A click lands on the topmost visible row, then the bottommost: neither moves the offset.
    assert_eq!(
        app.bridges.settings_scroll.resolve(off, 10, 40, 0),
        off,
        "top row click stays"
    );
    assert_eq!(
        app.bridges.settings_scroll.resolve(off + 9, 10, 40, 0),
        off,
        "bottom row click stays"
    );
}

#[test]
fn settings_scroll_resets_on_tab_switch() {
    let mut app = app_playing(1, 0);
    app.open_settings();
    // Scroll both the field list and a Keys column down (as a render + wheel would).
    app.bridges.settings_scroll.resolve(0, 10, 50, 0);
    app.bridges.settings_scroll.wheel(false, 8, 50);
    assert_eq!(app.bridges.settings_scroll.resolve(0, 10, 50, 0), 8);
    app.bridges.settings_keys_scroll[0].resolve(0, 5, 30, 0);
    app.bridges.settings_keys_scroll[0].wheel(false, 4, 30);
    assert_eq!(app.bridges.settings_keys_scroll[0].resolve(0, 5, 30, 0), 4);

    app.settings_switch_tab(true);

    assert_eq!(
        app.bridges.settings_scroll.resolve(0, 10, 50, 0),
        0,
        "field list resets to top"
    );
    assert_eq!(
        app.bridges.settings_keys_scroll[0].resolve(0, 5, 30, 0),
        0,
        "keys column resets"
    );
}

#[test]
fn settings_scroll_resets_on_reopen() {
    let mut app = app_playing(1, 0);
    app.open_settings();
    app.bridges.settings_scroll.resolve(0, 10, 50, 0);
    app.bridges.settings_scroll.wheel(false, 5, 50);
    assert_eq!(app.bridges.settings_scroll.resolve(0, 10, 50, 0), 5);
    app.close_settings();
    app.open_settings();
    assert_eq!(
        app.bridges.settings_scroll.resolve(0, 10, 50, 0),
        0,
        "reopening resets the offset"
    );
}

#[test]
fn liststate_select_none_resets_offset_so_keys_columns_must_guard_it() {
    // The Keys tab pre-seeds each column's offset via `ListState::with_offset`, then selects
    // only the focused column. This pins the ratatui API reason why: `select(None)` zeroes the
    // offset, so calling it on the unfocused column would discard the pre-seeded scroll.
    let mut state = ratatui::widgets::ListState::default().with_offset(5);
    state.select(Some(3));
    assert_eq!(
        state.offset(),
        5,
        "select(Some) keeps the pre-seeded offset"
    );
    state.select(None);
    assert_eq!(
        state.offset(),
        0,
        "select(None) resets the offset — why the unfocused column skips it"
    );
}

#[test]
fn library_delete_is_disabled_in_all_tab() {
    let mut app = App::new(100);
    app.library
        .toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(cmds.is_empty());
    assert_eq!(app.library.favorites.len(), 1); // untouched
}

#[test]
fn library_all_dedups_same_title_across_collections() {
    let mut app = App::new(100);
    app.library
        .toggle_favorite(&Song::remote("yt1", "Song", "Artist", "3:00"));
    // A downloaded file named after the same track (`Song.m4a` -> title "Song").
    app.library_ui.downloaded = vec![Song::local_file(PathBuf::from("/tmp/Song.m4a"))];
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    // The remote favorite and the local file collapse to a single All-tab row...
    assert_eq!(app.library_len(), 1);
    // ...and the catalog entry (first in the chain) is the one kept.
    assert_eq!(app.library_rows()[0].video_id, "yt1");
}

#[test]
fn downloads_delete_confirms_then_removes_file() {
    let file = temp_audio_file("del");
    let mut app = App::new(100);
    app.config.download_dir = file.parent().map(PathBuf::from);
    app.library_ui.downloaded = vec![Song::local_file(file.clone())];
    open_library_tab(&mut app, LibraryTab::Downloads);
    // Delete opens the confirmation modal rather than deleting outright.
    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(cmds.is_empty());
    assert!(app.library_ui.confirm_delete.is_some());
    assert!(file.exists());
    // Confirming removes the file from disk and asks for a rescan.
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.library_ui.confirm_delete.is_none());
    assert!(!file.exists());
    assert!(cmds.iter().any(|c| matches!(c, Cmd::ScanDownloads(_))));
}

#[test]
fn downloads_delete_refuses_file_outside_download_dir() {
    let file = temp_audio_file("outside");
    let root = std::env::temp_dir().join(format!("yututui-app-test-root-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut app = App::new(100);
    app.config.download_dir = Some(root.clone());
    app.library_ui.downloaded = vec![Song::local_file(file.clone())];
    open_library_tab(&mut app, LibraryTab::Downloads);
    app.update(Msg::Key(key(KeyCode::Delete)));
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(file.exists());
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert!(cmds.iter().any(|c| matches!(c, Cmd::ScanDownloads(_))));
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn downloads_delete_refuses_symlink() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!(
        "yututui-app-test-symlink-root-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let real = root.join("real.m4a");
    let link = root.join("link.m4a");
    std::fs::write(&real, b"").unwrap();
    symlink(&real, &link).unwrap();
    let mut app = App::new(100);
    app.config.download_dir = Some(root.clone());
    app.library_ui.downloaded = vec![Song::local_file(link.clone())];
    open_library_tab(&mut app, LibraryTab::Downloads);
    app.update(Msg::Key(key(KeyCode::Delete)));
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(link.exists());
    assert!(real.exists());
    assert_eq!(app.library_ui.downloaded.len(), 1);
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_file(&real);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn downloads_delete_cancel_keeps_file() {
    let file = temp_audio_file("keep");
    let mut app = App::new(100);
    app.library_ui.downloaded = vec![Song::local_file(file.clone())];
    open_library_tab(&mut app, LibraryTab::Downloads);
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(app.library_ui.confirm_delete.is_some());
    // Any non-confirming key backs out and leaves the file alone.
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.confirm_delete.is_none());
    assert!(file.exists());
    assert!(cmds.is_empty());
    let _ = std::fs::remove_file(&file);
}
