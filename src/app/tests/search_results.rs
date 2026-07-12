use super::*;

#[test]
fn stale_search_results_do_not_overwrite_a_newer_search() {
    let mut app = App::new(100);
    app.mode = Mode::Search;

    // Submit "a" (request_id → 1), then "abcdef" (request_id → 2) before "a" returns.
    app.search.input = "a".to_owned();
    let _ = app.submit_search_query();
    let first_id = app.search.request_id;
    app.search.input = "abcdef".to_owned();
    let _ = app.submit_search_query();
    let second_id = app.search.request_id;
    assert_ne!(first_id, second_id);

    // The newer search's results arrive first and populate the list.
    app.update(Msg::SearchResults {
        request_id: second_id,
        query: "abcdef".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![Song::remote("newid", "New", "Artist", "3:00")],
    });
    assert!(app.search.results.iter().any(|s| s.video_id == "newid"));

    // The older, slower search's results arrive AFTER — they must be dropped (the bug:
    // once the newer search cleared `searching`, the old guard let this through).
    app.update(Msg::SearchResults {
        request_id: first_id,
        query: "a".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![Song::remote("oldid", "Old", "Artist", "3:00")],
    });
    assert!(
        app.search.results.iter().all(|s| s.video_id != "oldid"),
        "stale results must not overwrite the newer search"
    );
    assert!(app.search.results.iter().any(|s| s.video_id == "newid"));
}

#[test]
fn reenabling_streaming_resets_the_failure_breaker() {
    // After 3 empty top-ups auto-disabled streaming, the failure count is stuck at the cap.
    let mut app = App::new(100);
    app.streaming.consecutive_failures = crate::playback_policy::AUTOPLAY_MAX_FAILURES;
    app.set_autoplay_streaming(true);
    assert!(app.autoplay_streaming);
    assert_eq!(
        app.streaming.consecutive_failures, 0,
        "re-enabling must clear the stale breaker, else the next single failure re-disables it"
    );
    // Disabling leaves the counter untouched (only re-enabling resets it).
    app.streaming.consecutive_failures = 2;
    app.set_autoplay_streaming(false);
    assert!(!app.autoplay_streaming);
    assert_eq!(app.streaming.consecutive_failures, 2);
}

#[test]
fn external_set_volume_clears_the_mute_latch() {
    let mut app = App::new(100);
    // Simulate a muted state: volume held at 0, pre-mute level remembered.
    app.playback.pre_mute_volume = Some(80);
    app.playback.volume = 0;
    // An OS-widget volume write is a direct change and must clear the latch, so a later `m`
    // mutes to this new level instead of restoring the stale 80.
    let cmds = app.update(Msg::Media(crate::media::MediaCommand::SetVolume(0.5)));
    assert_eq!(app.playback.volume, 0);
    assert_eq!(app.playback.pre_mute_volume, Some(80));
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd.player_command(), Some(PlayerCmd::SetVolume(50))))
    );
    app.admit_player_intents_for_test(&cmds);
    assert_eq!(app.playback.volume, 50);
    assert_eq!(
        app.playback.pre_mute_volume, None,
        "a direct volume write clears the mute latch"
    );
}

#[test]
fn results_then_enter_plays_and_returns_to_player() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![Song::remote("abc123", "Song", "Artist", "3:00")],
    });
    assert_eq!(app.search.focus, SearchFocus::Results);
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_loads_video(&cmds, "abc123");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn enter_on_search_result_queues_only_the_selected_song() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![
            Song::remote("id0", "Zero", "A", "3:00"),
            Song::remote("id1", "One", "B", "3:00"),
            Song::remote("id2", "Two", "C", "3:00"),
        ],
    });
    app.search.focus = SearchFocus::Results;
    app.search.selected = 1;
    app.search.anchor = 1;
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_loads_video(&cmds, "id1");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
    // Only the picked track lands in the queue — not the whole result list. Nothing was
    // playing, so it starts immediately.
    assert_eq!(app.queue.len(), 1);
}

#[test]
fn enter_on_search_result_plays_now_keeping_the_queue() {
    // A 3-track queue is already playing track 0.
    let mut app = app_playing(3, 0);
    let before_len = app.queue.len();
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));

    // Go to search, pick a fresh result, hit Enter → play it right now.
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![Song::remote("new9", "New", "Z", "3:00")],
    });
    app.search.focus = SearchFocus::Results;
    app.search.selected = 0;
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));

    // The picked track starts playing immediately and we jump to the Player…
    assert_loads_video(&cmds, "new9");
    assert_eq!(app.mode, Mode::Search, "screen waits for load admission");
    assert_eq!(app.queue.len(), before_len);
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("new9"));
    // …and the existing queue is preserved (grew by one, originals kept — not wiped).
    assert_eq!(app.queue.len(), before_len + 1);
    for kept in ["id0", "id1", "id2"] {
        assert!(app.queue.video_ids().any(|v| v == kept), "kept {kept}");
    }
}

#[test]
fn backslash_on_search_result_enqueues_without_interrupting() {
    // A 3-track queue is already playing track 0.
    let mut app = app_playing(3, 0);
    let before_len = app.queue.len();
    let playing = app.prefetch.loaded_video_id.clone();
    assert_eq!(playing.as_deref(), Some("id0"));

    // Go to search, pick a fresh result, press `\` → add to queue.
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![Song::remote("new9", "New", "Z", "3:00")],
    });
    app.search.focus = SearchFocus::Results;
    app.search.selected = 0;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('\\'))));

    // The existing queue is preserved and grows by exactly one…
    assert_eq!(app.queue.len(), before_len + 1);
    assert!(app.queue.video_ids().any(|v| v == "new9"));
    // …the current track keeps playing uninterrupted (no reload, no jump to Player)…
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert_no_load(&cmds);
    assert_eq!(app.mode, Mode::Search);
    // …and it's confirmed as a positive (green) toast, not an error.
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn search_result_confirm_stays_enter_when_common_confirm_is_remapped() {
    let mut app = App::new(100);
    app.keymap = confirm_on_f5_keymap();
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(1);

    let cmds = app.update(Msg::Key(key(KeyCode::F(5))));
    assert_no_load(&cmds);
    assert_eq!(app.mode, Mode::Search);

    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_loads_video(&cmds, "id0");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.mode, Mode::Player);
}
