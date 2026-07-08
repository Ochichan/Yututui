use super::*;

#[test]
fn enter_on_library_plays_selected_song_keeping_the_queue() {
    // A 2-track queue is already playing.
    let mut app = app_playing(2, 0);
    let before_len = app.queue.len();
    // Library has a couple of history tracks; open it and select the top one.
    app.library
        .record_play(&Song::remote("lib0", "Lib Zero", "A", "3:00"));
    app.library
        .record_play(&Song::remote("lib1", "Lib One", "B", "3:00"));
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;
    app.library_ui.selected = 0;
    let target = app.selected_library_song().unwrap().video_id;

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    // The picked track plays immediately and we jump to the Player…
    assert_eq!(app.mode, Mode::Player);
    assert_loads_video(&cmds, target.as_str());
    assert_eq!(
        app.prefetch.loaded_video_id.as_deref(),
        Some(target.as_str())
    );
    // …and the existing queue is preserved (grew by one, originals kept — not wiped).
    assert_eq!(app.queue.len(), before_len + 1);
    for kept in ["id0", "id1"] {
        assert!(app.queue.video_ids().any(|v| v == kept), "kept {kept}");
    }
}

#[test]
fn backslash_on_library_enqueues_selected_song_without_interrupting() {
    let mut app = app_playing(2, 0);
    let before_len = app.queue.len();
    let playing = app.prefetch.loaded_video_id.clone();
    app.library
        .record_play(&Song::remote("lib9", "Lib Nine", "Z", "3:00"));
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;
    app.library_ui.selected = 0;
    let target = app.selected_library_song().unwrap().video_id;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));
    // Enqueued without interrupting: no reload, stays in the Library, queue grows by one.
    assert_no_load(&cmds);
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert_eq!(app.queue.len(), before_len + 1);
    assert!(app.queue.video_ids().any(|v| v == target.as_str()));
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn enqueue_defaults_to_appending_at_the_queue_end() {
    let mut app = app_playing(3, 0);
    app.library
        .record_play(&Song::remote("lib9", "Lib Nine", "Z", "3:00"));
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;
    app.library_ui.selected = 0;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));

    assert_no_load(&cmds);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id0", "id1", "id2", "lib9"]);
}

#[test]
fn enqueue_next_setting_inserts_after_the_current_track() {
    let mut app = app_playing(3, 0);
    app.config.enqueue_next = Some(true);
    app.library
        .record_play(&Song::remote("lib9", "Lib Nine", "Z", "3:00"));
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;
    app.library_ui.selected = 0;

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));

    assert_no_load(&cmds);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id0", "lib9", "id1", "id2"]);
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn enter_on_library_drag_selection_plays_all_selected_tracks() {
    let mut app = app_playing(2, 0);
    app.library.favorites = vec![
        Song::remote("f0", "F0", "A", "3:00"),
        Song::remote("f1", "F1", "B", "3:00"),
        Song::remote("f2", "F2", "C", "3:00"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library_ui.anchor = 0;
    app.library_ui.selected = 2;
    let before_len = app.queue.len();

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(app.mode, Mode::Player);
    assert_loads_video(&cmds, "f0");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("f0"));
    assert_eq!(app.queue.len(), before_len + 3);
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(&ids[1..4], &["f0", "f1", "f2"]);
}

#[test]
fn backslash_on_library_drag_selection_enqueues_all_selected_tracks() {
    let mut app = app_playing(2, 0);
    let playing = app.prefetch.loaded_video_id.clone();
    app.library.favorites = vec![
        Song::remote("f0", "F0", "A", "3:00"),
        Song::remote("f1", "F1", "B", "3:00"),
        Song::remote("f2", "F2", "C", "3:00"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library_ui.anchor = 0;
    app.library_ui.selected = 2;
    let before_len = app.queue.len();

    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));

    assert_no_load(&cmds);
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert_eq!(app.queue.len(), before_len + 3);
    for id in ["f0", "f1", "f2"] {
        assert!(app.queue.video_ids().any(|v| v == id), "queued {id}");
    }
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn a_on_library_plays_the_whole_tab_as_a_fresh_queue() {
    // A 2-track queue is already playing id0/id1.
    let mut app = app_playing(2, 0);
    app.library.favorites = vec![
        Song::remote("f0", "F0", "A", "3:00"),
        Song::remote("f1", "F1", "B", "3:00"),
        Song::remote("f2", "F2", "C", "3:00"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library_ui.selected = 1;
    let rows = app.library_songs().len();
    assert_eq!(rows, 3);

    let cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));
    // The whole tab replaces the queue: the old id0/id1 are gone, playback starts at f1.
    assert_eq!(app.mode, Mode::Player);
    assert_loads_video(&cmds, "f1");
    assert_eq!(app.queue.len(), rows);
    assert!(app.queue.video_ids().all(|v| v != "id0" && v != "id1"));
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("f1"));
}
