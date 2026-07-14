use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_AUTO_ORGANIZE_FIXTURE: AtomicU64 = AtomicU64::new(1);

#[test]
fn missing_download_tools_keep_the_queue_pending_and_open_setup() {
    let mut app = App::new(100);
    app.downloads
        .pending
        .push_back(Song::remote("id", "title", "artist", "1:00"));
    assert!(app.block_downloads_for_missing(vec!["ffmpeg"]));
    assert_eq!(app.downloads.pending.len(), 1);
    assert!(matches!(
        app.tool_setup.as_ref().map(|prompt| prompt.context),
        Some(ToolSetupContext::Downloads)
    ));
}

fn test_import_context(
    session_id: &str,
    row_id: &str,
    source_order: u32,
    video_id: &str,
) -> crate::download::ImportDownloadContext {
    crate::download::ImportDownloadContext {
        session_id: session_id.to_owned(),
        row_id: row_id.to_owned(),
        source_order,
        video_id: video_id.to_owned(),
        claim: crate::transfer::artifact_identity::test_import_claim(
            session_id,
            row_id,
            source_order,
            video_id,
        ),
    }
}

struct AutoOrganizeFixture {
    root: PathBuf,
    session_id: String,
    session: crate::transfer::session::ImportSession,
}

impl AutoOrganizeFixture {
    fn new(label: &str, row_count: u32) -> Self {
        let sequence = NEXT_AUTO_ORGANIZE_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("sp2yt-auto-{label}-{}-{sequence}", std::process::id());
        let root = std::env::temp_dir().join(format!(
            "yututui-auto-organize-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create organize fixture root");
        let private_root = root.join(".yututui-inbox");
        let session_root = private_root.join(&session_id);
        let complete = session_root.join("complete");
        for directory in [&private_root, &session_root, &complete] {
            crate::util::safe_fs::ensure_private_dir(directory)
                .expect("create private organize fixture directory");
        }

        let rows = (1..=row_count)
            .map(|source_order| {
                let video_id = format!("auto{source_order:07}");
                let path = complete.join(format!("Track {source_order} [{video_id}].m4a"));
                std::fs::write(&path, format!("audio-{source_order}").as_bytes())
                    .expect("write organize fixture audio");
                crate::transfer::session::ImportSessionRow {
                    row_id: format!("row-{source_order:05}"),
                    source_order,
                    status: crate::transfer::session::ImportSessionRowStatus::Matched,
                    title: format!("Track {source_order}"),
                    artists: vec!["Artist".to_owned()],
                    album: Some("Album".to_owned()),
                    album_artists: vec!["Album Artist".to_owned()],
                    selected_key: Some(video_id),
                    written: true,
                    local_path: Some(path),
                    disc_number: Some(1),
                    track_number: Some(source_order),
                    ..crate::transfer::session::ImportSessionRow::default()
                }
            })
            .collect();
        let session = crate::transfer::session::ImportSession {
            session_id: session_id.clone(),
            job_id: session_id.clone(),
            rows,
            ..crate::transfer::session::ImportSession::default()
        };
        session.save().expect("save organize fixture session");
        Self {
            root,
            session_id,
            session,
        }
    }

    fn song(&self, source_order: u32) -> Song {
        let row = self
            .session
            .rows
            .iter()
            .find(|row| row.source_order == source_order)
            .expect("fixture row");
        Song::remote(
            row.selected_key.clone().expect("fixture video id"),
            row.title.clone(),
            row.artists.join(", "),
            "3:00",
        )
        .with_import_session(Some(self.session_id.clone()), Some(source_order))
    }

    fn source_path(&self, source_order: u32) -> PathBuf {
        self.session
            .rows
            .iter()
            .find(|row| row.source_order == source_order)
            .and_then(|row| row.local_path.clone())
            .expect("fixture source path")
    }

    fn configure(&self, app: &mut App) {
        app.config.local.roots = vec![crate::config::LocalRootConfig {
            path: self.root.clone(),
            enabled: Some(true),
            recursive: Some(true),
        }];
        app.config.local.import_path_template =
            Some("{album_artist}/{album}/{disc_track} - {title} [{youtube_id}]".to_owned());
    }

    fn cleanup(&self) {
        let _ = crate::transfer::session::ImportSession::delete_record(&self.session_id);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

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
    let root =
        std::env::temp_dir().join(format!("yututui-downloadable-batch-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let past_path = root.join("keep.m4a");
    std::fs::write(&past_path, b"audio").unwrap();
    let past = fsong("keep", "Kept", "A").with_local_path(past_path);
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
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn downloadable_batch_retries_a_manifest_record_whose_file_is_missing() {
    let mut app = App::new(100);
    let missing = std::env::temp_dir().join(format!(
        "yututui-missing-download-{}-keep.m4a",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&missing);
    app.download_store
        .record(&fsong("keep", "Kept", "A").with_local_path(missing));

    let batch = app.downloadable_batch(vec![fsong("keep", "Kept", "A")]);

    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].video_id, "keep");
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

    // Confirming clears the modal, queues the batch, and emits one DownloadCmd::Start per track.
    let cmds = app.confirm_download_apply();
    let downloads = cmds
        .iter()
        .filter(|c| matches!(c, Cmd::Download(DownloadCmd::Start(_))))
        .count();
    assert_eq!(downloads, 2);
    assert!(app.library_ui.confirm_download.is_none());
    assert_eq!(app.downloads.dispatched, 2);
}

#[test]
fn same_video_imports_are_tracked_and_settled_by_session_row() {
    let mut app = App::new(100);
    let first = Song::remote("shared-video", "First row", "Artist A", "1:00")
        .with_import_session(Some("sp2yt-session-a".to_owned()), Some(1));
    let second = Song::remote("shared-video", "Second row", "Artist B", "1:00")
        .with_import_session(Some("sp2yt-session-b".to_owned()), Some(1));

    assert_eq!(app.start_download(first).len(), 1);
    assert_eq!(app.start_download(second).len(), 1);
    assert_eq!(app.downloads.dispatched, 2);
    let first_key = crate::download::import_download_tracking_key("sp2yt-session-a", 1);
    let second_key = crate::download::import_download_tracking_key("sp2yt-session-b", 1);
    assert!(app.downloads.sources.contains_key(&first_key));
    assert!(app.downloads.sources.contains_key(&second_key));

    app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context("sp2yt-session-b", "row-00001", 1, "shared-video"),
        path: "/tmp/session-b.m4a".to_owned(),
    }));
    assert!(app.downloads.sources.contains_key(&first_key));
    assert!(!app.downloads.sources.contains_key(&second_key));
    assert_eq!(app.downloads.dispatched, 1);
    assert_eq!(app.library_ui.downloaded[0].title, "Second row");

    app.update(Msg::Download(DownloadMsg::ImportError {
        context: test_import_context("sp2yt-session-a", "row-00001", 1, "shared-video"),
        error: "delayed failure".to_owned(),
    }));
    assert!(!app.downloads.sources.contains_key(&first_key));
    assert_eq!(app.downloads.dispatched, 0);
    assert_eq!(
        app.downloads.active.get(&first_key),
        Some(&DownloadState::Failed)
    );
    assert_eq!(
        app.downloads.active.get(&second_key),
        Some(&DownloadState::Done)
    );
}

#[test]
fn late_progress_cannot_reopen_terminal_download_state() {
    let mut app = App::new(100);
    app.downloads
        .active
        .insert("ordinary".to_owned(), DownloadState::Done);
    app.update(Msg::Download(DownloadMsg::Progress {
        video_id: "ordinary".to_owned(),
        percent: 55.0,
    }));
    assert_eq!(
        app.downloads.active.get("ordinary"),
        Some(&DownloadState::Done)
    );
    assert_eq!(
        app.start_download(Song::remote("ordinary", "Retry", "Artist", "1:00"))
            .len(),
        1
    );
    assert_eq!(
        app.downloads.active.get("ordinary"),
        Some(&DownloadState::Running(0)),
        "an explicit retry resets terminal state before new progress"
    );

    let context = test_import_context("sp2yt-late-progress", "row-00003", 3, "shared-video");
    let tracking_key = context.tracking_key();
    app.downloads
        .active
        .insert(tracking_key.clone(), DownloadState::Failed);
    app.update(Msg::Download(DownloadMsg::ImportProgress {
        context,
        percent: 88.0,
    }));
    assert_eq!(
        app.downloads.active.get(&tracking_key),
        Some(&DownloadState::Failed)
    );
}

#[test]
fn duplicate_terminal_does_not_release_or_pump_an_inflight_slot_twice() {
    let mut app = App::new(100);
    let songs: Vec<_> = (0..98)
        .map(|index| fsong(&format!("terminal-{index}"), "Title", "Artist"))
        .collect();
    app.library_ui.confirm_download = Some(songs);
    app.confirm_download_apply();
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 2);

    let terminal = || {
        Msg::Download(DownloadMsg::Done {
            video_id: "terminal-0".to_owned(),
            path: String::new(),
        })
    };
    let first = app.update(terminal());
    assert_eq!(
        first
            .iter()
            .filter(|command| matches!(command, Cmd::Download(DownloadCmd::Start(_))))
            .count(),
        1
    );
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 1);

    assert!(app.update(terminal()).is_empty());
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(
        app.downloads.pending.len(),
        1,
        "an at-least-once terminal replay must not admit another queued job"
    );
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
            .filter(|c| matches!(c, Cmd::Download(DownloadCmd::Start(_))))
            .count(),
        96
    );
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 4);

    // Each completion frees a slot and drips the next queued download in, holding at the cap.
    app.update(Msg::Download(DownloadMsg::Done {
        video_id: "id0".to_string(),
        path: String::new(),
    }));
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 3);
}

#[test]
fn bulk_download_backlog_overflow_is_retained_for_explicit_retry() {
    let mut app = App::new(100);
    let many: Vec<Song> = (0..1_000)
        .map(|i| fsong(&format!("overflow-{i}"), "T", "A"))
        .collect();
    app.library_ui.confirm_download = Some(many);

    let cmds = app.confirm_download_apply();

    assert_eq!(
        cmds.iter()
            .filter(|cmd| matches!(cmd, Cmd::Download(DownloadCmd::Start(_))))
            .count(),
        96
    );
    assert_eq!(app.downloads.dispatched, 96);
    assert_eq!(app.downloads.pending.len(), 903);
    let retained = app
        .library_ui
        .confirm_download
        .as_ref()
        .expect("overflow remains available for retry");
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].video_id, "overflow-999");
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("999"));
    assert!(app.status.text.contains('1'));
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
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter))); // play the All-tab current row
    assert_eq!(load_url(&cmds), Some(path));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
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

    let cmds = app.update(Msg::Download(DownloadMsg::Done {
        video_id: "dQw4w9WgXcQ".to_owned(),
        path: "/tmp/Title [dQw4w9WgXcQ].m4a".to_owned(),
    }));
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
fn app_import_done_does_not_rewrite_worker_owned_session_row() {
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

    app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(session_id, "row-00003", 3, "dQw4w9WgXcQ"),
        path: "/tmp/Title [dQw4w9WgXcQ].m4a".to_owned(),
    }));

    let session = crate::transfer::session::ImportSession::load(session_id)
        .expect("load unchanged import session");
    assert!(!session.rows[0].written);
    assert!(session.rows[0].local_path.is_none());
    assert_eq!(session.counts.written, 0);
    let _ = crate::transfer::session::ImportSession::delete_record(session_id);
}

#[test]
fn app_import_error_does_not_rewrite_worker_owned_session_row() {
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

    app.update(Msg::Download(DownloadMsg::ImportError {
        context: test_import_context(session_id, "row-00002", 2, "dQw4w9WgXcQ"),
        error: "private video needs login".to_owned(),
    }));

    let session = crate::transfer::session::ImportSession::load(session_id)
        .expect("load updated import session");
    assert!(!session.rows[0].written);
    assert_eq!(session.rows[0].local_path, None);
    assert!(session.rows[0].errors.is_empty());
    assert_eq!(session.counts.written, 0);
    let _ = crate::transfer::session::ImportSession::delete_record(session_id);
}

#[test]
fn import_batch_auto_organizes_only_after_the_session_settles() {
    let _guard = crate::i18n::lock_for_test();
    let fixture = AutoOrganizeFixture::new("settled", 2);
    let mut app = App::new(100);
    fixture.configure(&mut app);
    app.library_ui.confirm_download = Some(vec![fixture.song(1), fixture.song(2)]);
    app.confirm_download_apply();

    let first = app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(&fixture.session_id, "row-00001", 1, "auto0000001"),
        path: fixture.source_path(1).to_string_lossy().into_owned(),
    }));
    assert!(
        !first
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. }))),
        "the first terminal must wait for the other running row"
    );
    let waiting = crate::transfer::session::ImportSession::load(&fixture.session_id)
        .expect("load waiting session");
    assert_eq!(waiting.rows[0].local_path, Some(fixture.source_path(1)));
    assert_eq!(waiting.rows[1].local_path, Some(fixture.source_path(2)));

    let last = app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(&fixture.session_id, "row-00002", 2, "auto0000002"),
        path: fixture.source_path(2).to_string_lossy().into_owned(),
    }));
    assert!(
        last.iter()
            .any(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. }))),
        "the last terminal refreshes Local Deck after publishing"
    );
    let organized = crate::transfer::session::ImportSession::load(&fixture.session_id)
        .expect("load organized session");
    for row in &organized.rows {
        let path = row.local_path.as_ref().expect("organized local path");
        assert!(path.starts_with(&fixture.root));
        assert!(!path.to_string_lossy().contains(".yututui-inbox"));
        assert!(path.is_file());
        let runtime = app
            .library_ui
            .downloaded
            .iter()
            .find(|song| song.import_source_order == Some(row.source_order))
            .expect("organized runtime download row");
        assert_eq!(runtime.local_path.as_ref(), Some(path));
        let manifest = app
            .download_store
            .tracks()
            .iter()
            .find(|song| song.import_source_order == Some(row.source_order))
            .expect("organized manifest row");
        assert_eq!(manifest.local_path.as_ref(), Some(path));
    }
    assert!(app.status.text.contains("Organized import session"));
    assert!(app.downloads.auto_organize_rows.is_empty());
    fixture.cleanup();
}

#[test]
fn import_batch_auto_organize_moves_successes_and_leaves_failures_retryable() {
    let _guard = crate::i18n::lock_for_test();
    let fixture = AutoOrganizeFixture::new("partial-failure", 2);
    let mut persisted = fixture.session.clone();
    persisted.rows[1].written = false;
    persisted.rows[1].local_path = None;
    persisted.rows[1].errors = vec!["private video".to_owned()];
    persisted.save().expect("save partial failure session");

    let mut app = App::new(100);
    fixture.configure(&mut app);
    app.library_ui.confirm_download = Some(vec![fixture.song(1), fixture.song(2)]);
    app.confirm_download_apply();
    app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(&fixture.session_id, "row-00001", 1, "auto0000001"),
        path: fixture.source_path(1).to_string_lossy().into_owned(),
    }));
    let final_commands = app.update(Msg::Download(DownloadMsg::ImportError {
        context: test_import_context(&fixture.session_id, "row-00002", 2, "auto0000002"),
        error: "private video".to_owned(),
    }));

    let organized = crate::transfer::session::ImportSession::load(&fixture.session_id)
        .expect("load partial organize session");
    let success = organized.rows[0]
        .local_path
        .as_ref()
        .expect("successful row published");
    assert!(!success.to_string_lossy().contains(".yututui-inbox"));
    assert!(organized.rows[0].written);
    assert!(!organized.rows[1].written);
    assert!(organized.rows[1].local_path.is_none());
    assert_eq!(organized.rows[1].errors, vec!["private video"]);
    let manifest = app
        .download_store
        .tracks()
        .iter()
        .find(|song| song.import_source_order == Some(1))
        .expect("successful manifest row");
    assert_eq!(manifest.local_path.as_ref(), Some(success));
    assert!(
        final_commands
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Downloads)))
    );
    assert!(app.status.text.contains("retry from Local Deck"));
    fixture.cleanup();
}

#[test]
fn import_batch_auto_organize_does_not_sweep_preexisting_inbox_rows() {
    let _guard = crate::i18n::lock_for_test();
    let fixture = AutoOrganizeFixture::new("cohort", 2);
    let old_path = fixture.source_path(1);
    let mut app = App::new(100);
    fixture.configure(&mut app);

    app.start_download(fixture.song(2));
    app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(&fixture.session_id, "row-00002", 2, "auto0000002"),
        path: fixture.source_path(2).to_string_lossy().into_owned(),
    }));

    let organized = crate::transfer::session::ImportSession::load(&fixture.session_id)
        .expect("load cohort organize session");
    assert_eq!(
        organized.rows[0].local_path.as_deref(),
        Some(old_path.as_path())
    );
    assert!(old_path.is_file());
    let current = organized.rows[1]
        .local_path
        .as_ref()
        .expect("current batch row published");
    assert!(!current.to_string_lossy().contains(".yututui-inbox"));
    fixture.cleanup();
}

#[test]
fn import_batch_auto_organize_failure_keeps_inbox_and_points_to_manual_retry() {
    let _guard = crate::i18n::lock_for_test();
    let fixture = AutoOrganizeFixture::new("organize-error", 1);
    let bad_root = fixture.root.with_extension("not-a-directory");
    std::fs::write(&bad_root, b"not a directory").expect("write invalid organize root");
    let source = fixture.source_path(1);
    let mut app = App::new(100);
    app.config.local.roots = vec![crate::config::LocalRootConfig {
        path: bad_root.clone(),
        enabled: Some(true),
        recursive: Some(true),
    }];

    app.start_download(fixture.song(1));
    app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(&fixture.session_id, "row-00001", 1, "auto0000001"),
        path: source.to_string_lossy().into_owned(),
    }));

    assert!(source.is_file(), "the inbox artifact remains available");
    let saved = crate::transfer::session::ImportSession::load(&fixture.session_id)
        .expect("load failed organize session");
    assert_eq!(saved.rows[0].local_path.as_deref(), Some(source.as_path()));
    assert_eq!(app.status.kind, StatusKind::Error);
    assert!(app.status.text.contains("artifacts remain recoverable"));
    assert!(app.status.text.contains("press m to retry"));
    assert!(app.downloads.auto_organize_rows.is_empty());
    let _ = std::fs::remove_file(bad_root);
    fixture.cleanup();
}

#[test]
fn import_batch_auto_organize_latches_refresh_behind_an_active_scan() {
    let _guard = crate::i18n::lock_for_test();
    let fixture = AutoOrganizeFixture::new("scan-latch", 1);
    let mut app = App::new(100);
    fixture.configure(&mut app);
    app.local_mode.index.loaded = true;
    app.local_mode.index.scanning = true;

    app.start_download(fixture.song(1));
    let terminal = app.update(Msg::Download(DownloadMsg::ImportDone {
        context: test_import_context(&fixture.session_id, "row-00001", 1, "auto0000001"),
        path: fixture.source_path(1).to_string_lossy().into_owned(),
    }));

    assert!(
        !terminal
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    );
    assert_eq!(app.local_mode.index.pending_rescan, Some(false));

    let follow_up = app.update(Msg::Local(LocalMsg::ScanFinished {
        index_path: None,
        result: crate::local::LocalScanResult {
            index: crate::local::LocalIndex::default(),
            summary: crate::local::LocalScanSummary::default(),
            errors: Vec::new(),
        },
    }));
    assert!(
        follow_up
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Local(LocalCmd::ScanRoots { .. })))
    );
    assert!(app.local_mode.index.scanning);
    assert_eq!(app.local_mode.index.pending_rescan, None);
    fixture.cleanup();
}

#[test]
fn download_done_with_empty_path_does_not_save() {
    let mut app = App::new(100);
    app.start_download(Song::remote("x", "Title", "Artist", "1:00"));
    assert!(app.downloads.sources.contains_key("x"));
    let cmds = app.update(Msg::Download(DownloadMsg::Done {
        video_id: "x".to_owned(),
        path: "   ".to_owned(),
    }));
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Downloads)))
    );
    assert!(
        !app.downloads.sources.contains_key("x"),
        "terminal empty-path results must release retained source metadata"
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
    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);
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

fn settle_confirmed_download_delete(app: &mut App, commands: Vec<Cmd>) -> Vec<Cmd> {
    let mut commands = commands.into_iter();
    let (paths, root) = match commands.next() {
        Some(Cmd::Download(DownloadCmd::Delete { paths, root })) => (paths, root),
        _ => panic!("confirmed deletion must emit one typed delete effect"),
    };
    assert!(commands.next().is_none());
    let (deleted, failures) = crate::download::delete_download_files(paths, &root);
    app.update(Msg::DownloadsDeleted {
        root,
        deleted,
        failed: failures.len(),
    })
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
    // Confirming only transfers ownership to the runtime; reducer code cannot unlink first.
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.library_ui.confirm_delete.is_none());
    assert!(file.exists());
    let cmds = settle_confirmed_download_delete(&mut app, cmds);
    assert!(!file.exists());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Download(DownloadCmd::Scan(_))))
    );
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
    let cmds = settle_confirmed_download_delete(&mut app, cmds);
    assert!(file.exists());
    assert_eq!(app.library_ui.downloaded.len(), 1);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Download(DownloadCmd::Scan(_))))
    );
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
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    settle_confirmed_download_delete(&mut app, cmds);
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
